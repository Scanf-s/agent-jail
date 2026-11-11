use crate::cgroups::{self, Cgroup};
use crate::config::{ContainerConfig, NetworkMode};
use crate::container;
use crate::filesystem::{PivotRoot, RunDir};
use crate::ipc::StartupChannels;
use crate::network;
use crate::proc_stat;
use crate::signals::{self, Event, RuntimeSignals};
use crate::state::{Record, StateDir};
use crate::user::Mapping;
use anyhow::{Context, Result, bail};
use nix::fcntl::OFlag;
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{
    ForkResult, Gid, Pid, Uid, chdir, fork, pipe2, read, setgid, setgroups, setuid, write,
};
use oci_spec::runtime::ContainerState;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::process::ExitCode;

pub struct Runtime {
    config: ContainerConfig,
    mapping: Mapping,
    state_dir: StateDir,
    cgroup: Cgroup,
    run_dir: RunDir,
    channels: Option<StartupChannels>,
}

impl Runtime {
    /// Create the resources of a new container. Its state directory goes under `state_root`.
    pub fn new(config: ContainerConfig, mapping: Mapping, state_root: &Path) -> Result<Self> {
        if let Some(id) = &config.id {
            StateDir::check_unused(state_root, id)?;
        }
        // This may replace the process, so it runs before the runtime opens any resource.
        let (delegated, id) = cgroups::enter_delegated_cgroup(config.id.as_ref())?;
        // If a later step fails, dropping the cgroup and the state directory removes them again.
        let cgroup = Cgroup::new(&delegated, &id)?;
        cgroup.configure(&config.resources)?;
        let record = Record::creating(
            &id,
            cgroup.path().to_owned(),
            config.bundle.clone(),
            config.annotations.clone(),
        );
        let state_dir = StateDir::create(state_root, &id, record)?;
        let run_dir = RunDir::create(&id)?;

        let channels = StartupChannels::new()?;

        Ok(Self {
            config,
            mapping,
            state_dir,
            cgroup,
            run_dir,
            channels: Some(channels),
        })
    }

    pub fn run(mut self) -> Result<ExitCode> {
        let signals = RuntimeSignals::block()?;
        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => self.run_parent(child, &signals),
            ForkResult::Child => self.run_setup_process(&signals),
        }
    }

    fn run_parent(&mut self, child: Pid, signals: &RuntimeSignals) -> Result<ExitCode> {
        let result = self.wait_for_container(child, signals);
        // If the setup process ended before container init, init still runs, and nothing else
        // would end it.
        let ended = self.cgroup.kill_all();
        // The setup process removes the run directory. This covers a run that fails before that.
        if let Err(error) = self.run_dir.remove() {
            eprintln!("aj: warning: {error:#}");
        }
        match ended {
            Ok(()) => {
                if let Err(error) = self.state_dir.remove() {
                    eprintln!("aj: warning: {error:#}");
                }
            }
            // The state keeps the container visible in `aj list` and its ID in use while its
            // processes may still run.
            Err(error) => {
                eprintln!(
                    "aj: warning: end the processes left in the container, so its state stays: \
                     {error:#}"
                );
                self.state_dir.keep();
            }
        }
        result
    }

    fn wait_for_container(&mut self, child: Pid, signals: &RuntimeSignals) -> Result<ExitCode> {
        if let Err(error) = self.setup_parent(child) {
            let _ = kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(error).context("container setup failed; child killed");
        }

        let status = wait_for_setup_process(child, signals)?;
        Ok(ExitCode::from(exit_status(status)?))
    }

    fn setup_parent(&mut self, child: Pid) -> Result<()> {
        let channels = self
            .channels
            .take()
            .context("startup channels already used")?;
        let mut ipc = channels.for_parent();

        // The setup process waits until it belongs to the cgroup.
        self.cgroup
            .add_pid(child)
            .context("add setup child to cgroup")?;
        ipc.signal_cgroup_ready()?;

        // UID/GID maps can be written after the child creates its user namespace.
        ipc.wait_for_user_namespace()?;
        self.mapping
            .apply(child)
            .context("map container UIDs and GIDs")?;
        ipc.signal_mapping_ready()?;

        ipc.wait_for_network_namespace()?;
        match self.config.network {
            // The setup process lives until container init exits, so pasta keeps running.
            NetworkMode::Pasta => network::start_pasta(child).context("start pasta")?,
            // Container init brings lo up by itself.
            NetworkMode::None => {}
        }
        ipc.signal_network_ready()?;
        Ok(())
    }

    fn run_setup_process(&mut self, signals: &RuntimeSignals) -> ! {
        if let Err(error) = self.setup_process(signals) {
            eprintln!("aj: setup process failed: {error:#}");
            std::process::exit(127);
        }
        unreachable!("setup process returned without exiting");
    }

    fn setup_process(&mut self, signals: &RuntimeSignals) -> Result<()> {
        let channels = self
            .channels
            .take()
            .context("startup channels already used")?;
        let mut ipc = channels.for_child();
        ipc.wait_for_cgroup()?;

        unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER)")?;
        ipc.signal_user_namespace_ready()?;

        ipc.wait_for_mapping()?;

        // The next fork places container init at PID 1 in the new PID namespace.
        // This process already belongs to the container cgroup, so the new cgroup namespace
        // shows that cgroup as its root.
        unshare(CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWCGROUP)
            .context("unshare(CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWCGROUP)")?;
        ipc.signal_network_namespace_ready()?;
        ipc.wait_for_network()?;

        self.launch_init(signals)
    }

    fn launch_init(&mut self, signals: &RuntimeSignals) -> Result<()> {
        // Container init writes a byte to this pipe if it fails before the program runs. The
        // write end has O_CLOEXEC, so a successful exec closes it, and the setup process reads
        // EOF instead.
        let (startup_reader, startup_writer) =
            pipe2(OFlag::O_CLOEXEC).context("create the startup pipe")?;
        match unsafe { fork() }.context("fork (setup) failed")? {
            ForkResult::Parent { child } => {
                drop(startup_writer);
                let code = supervise(child, signals, startup_reader, || {
                    self.record_running(child);
                });
                self.record_stopped(code.as_ref().ok().copied());
                // Only a process in the user namespace can remove files of subordinate IDs.
                // The overlay is already gone, because init's mount namespace ended with it.
                if let Err(error) = self.run_dir.remove() {
                    eprintln!("aj: warning: {error:#}");
                }
                std::process::exit(i32::from(code?));
            }
            ForkResult::Child => {
                drop(startup_reader);
                if let Err(error) = self.run_init() {
                    eprintln!("aj: container init failed: {error:#}");
                    let _ = write(&startup_writer, &[1]);
                    std::process::exit(127);
                }
                unreachable!("container init returned without executing command");
            }
        }
    }

    fn record_running(&mut self, init: Pid) {
        let result = (|| -> Result<()> {
            let start_time = proc_stat::start_time(init)?;
            self.state_dir.update(|record| {
                record
                    .oci
                    .set_status(ContainerState::Running)
                    .set_pid(Some(init.as_raw()));
                record.init_start_time = Some(start_time);
            })
        })();
        if let Err(error) = result {
            eprintln!("aj: warning: record that the container runs: {error:#}");
        }
    }

    /// Record that the container program ended, with its exit code if the runtime knows it.
    fn record_stopped(&mut self, exit_code: Option<u8>) {
        let result = self.state_dir.update(|record| {
            record.oci.set_status(ContainerState::Stopped);
            record.exit_code = exit_code;
        });
        if let Err(error) = result {
            eprintln!("aj: warning: record that the container stopped: {error:#}");
        }
    }

    fn run_init(&self) -> Result<()> {
        match self.config.network {
            // pasta has already brought lo up.
            NetworkMode::Pasta => {}
            NetworkMode::None => network::bring_up_loopback()?,
        }

        PivotRoot::new(
            self.config.rootfs.clone(),
            &self.run_dir,
            &self.config.volumes,
        )
        .isolate_filesystem()?;

        match self.config.network {
            NetworkMode::Pasta => network::write_resolv_conf(&self.config.volumes)?,
            NetworkMode::None => {}
        }

        let process = &self.config.process;
        chdir(&process.cwd)
            .with_context(|| format!("chdir to working directory {}", process.cwd.display()))?;
        // Without this, the program keeps the supplementary groups of the host user.
        let groups: Vec<Gid> = process
            .additional_gids
            .iter()
            .map(|&gid| Gid::from_raw(gid))
            .collect();
        setgroups(&groups).context("set supplementary groups")?;
        setgid(Gid::from_raw(process.gid))?;
        setuid(Uid::from_raw(process.uid))?;

        signals::reset_for_exec()?;
        container::exec(&process.args, &process.env)
    }
}

/// Wait until the setup process ends.
///
/// SIGTERM and SIGHUP usually reach only the runtime, for example from `kill` or a process
/// manager, so they go to the setup process. A terminal sends SIGINT and SIGQUIT to the whole
/// foreground process group, so the setup process and the container already receive them.
fn wait_for_setup_process(child: Pid, signals: &RuntimeSignals) -> Result<WaitStatus> {
    loop {
        let signal = match signals.wait() {
            Ok(signal) => signal,
            // Returning early would remove the run directory while the container still uses it.
            Err(error) => {
                eprintln!("aj: warning: {error:#}");
                return waitpid(child, None).context("waitpid failed");
            }
        };
        match signal {
            Signal::SIGCHLD => {
                match waitpid(child, Some(WaitPidFlag::WNOHANG)).context("waitpid failed")? {
                    WaitStatus::StillAlive => {}
                    status => return Ok(status),
                }
            }
            signal @ (Signal::SIGTERM | Signal::SIGHUP) => {
                let _ = kill(child, signal);
            }
            _ => {}
        }
    }
}

/// Wait until container init ends, and return the exit code for the runtime.
///
/// `startup` is the read end of the startup pipe. When it shows that init runs the container
/// program, this calls `on_running`.
///
/// If waiting fails, this kills and reaps init, so that the caller can remove the run directory.
fn supervise(
    init: Pid,
    signals: &RuntimeSignals,
    startup: OwnedFd,
    on_running: impl FnMut(),
) -> Result<u8> {
    let result = wait_for_init(init, signals, startup, on_running);
    if result.is_err() {
        let _ = kill(init, Signal::SIGKILL);
        let _ = waitpid(init, None);
    }
    result
}

fn wait_for_init(
    init: Pid,
    signals: &RuntimeSignals,
    startup: OwnedFd,
    mut on_running: impl FnMut(),
) -> Result<u8> {
    let mut startup = Some(startup);
    // The termination signal for which the runtime killed init.
    let mut termination_signal = None;
    loop {
        // While init starts, signals must still reach this loop. Otherwise Ctrl-C could not end
        // an init that hangs before exec.
        let event = match &startup {
            Some(pipe) => signals.wait_or_readable(pipe.as_fd())?,
            None => Event::Signal(signals.wait()?),
        };
        let signal = match event {
            Event::Signal(signal) => signal,
            Event::Readable => {
                let Some(pipe) = startup.take() else {
                    continue;
                };
                match read_startup(init, &pipe)? {
                    Startup::Running => on_running(),
                    // Init is ending, and its SIGCHLD ends this loop.
                    Startup::Failed => {}
                    Startup::Ended(status) => {
                        return exit_code_with_termination_signal(status, termination_signal);
                    }
                }
                continue;
            }
        };
        match signal {
            Signal::SIGCHLD => {
                match waitpid(init, Some(WaitPidFlag::WNOHANG)).context("waitpid(init) failed")? {
                    WaitStatus::StillAlive => {}
                    status => return exit_code_with_termination_signal(status, termination_signal),
                }
            }
            signal => {
                let default_action = match signals::uses_default_action(init, signal) {
                    Ok(default_action) => default_action,
                    // Passing the signal on leaves the decision to the program, as if it had
                    // a handler.
                    Err(error) => {
                        eprintln!("aj: warning: pass {signal} on to the container: {error:#}");
                        false
                    }
                };
                if default_action {
                    // Outside a container, the program would end on this signal. As PID 1 it
                    // does not receive the signal, so the runtime ends it.
                    termination_signal.get_or_insert(signal);
                    let _ = kill(init, Signal::SIGKILL);
                } else if matches!(signal, Signal::SIGTERM | Signal::SIGHUP) {
                    // The terminal does not send these to init, so the runtime passes them on.
                    let _ = kill(init, signal);
                }
            }
        }
    }
}

/// What the startup pipe showed about container init.
enum Startup {
    /// Init executed the container program.
    Running,
    /// Init failed before it executed the program, and it is ending.
    Failed,
    /// Init ended before it executed the program, with this status.
    Ended(WaitStatus),
}

/// Read the startup pipe of `init` after it became readable.
fn read_startup(init: Pid, startup: &OwnedFd) -> Result<Startup> {
    let mut byte = [0];
    if read(startup.as_raw_fd(), &mut byte).context("read the startup pipe")? > 0 {
        return Ok(Startup::Failed);
    }
    // A successful exec closes the write end. So does an init that ends before exec without
    // writing, for example on SIGKILL. Only an init that still runs has run the program.
    match waitpid(init, Some(WaitPidFlag::WNOHANG)).context("waitpid(init) failed")? {
        WaitStatus::StillAlive => Ok(Startup::Running),
        status => Ok(Startup::Ended(status)),
    }
}

/// Like `exit_status`, but if the runtime killed init for a termination signal, the code shows
/// that signal instead of SIGKILL.
fn exit_code_with_termination_signal(
    status: WaitStatus,
    termination_signal: Option<Signal>,
) -> Result<u8> {
    match (status, termination_signal) {
        (WaitStatus::Signaled(_, Signal::SIGKILL, _), Some(signal)) => Ok(128 + signal as u8),
        _ => exit_status(status),
    }
}

/// The exit code that a shell reports for `status`.
fn exit_status(status: WaitStatus) -> Result<u8> {
    match status {
        WaitStatus::Exited(_, code) => Ok(code as u8),
        WaitStatus::Signaled(_, signal, _) => Ok(128 + signal as u8),
        other => bail!("unexpected wait status: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_termination_signal_when_the_runtime_killed_init() {
        let pid = Pid::from_raw(42);
        let killed = WaitStatus::Signaled(pid, Signal::SIGKILL, false);
        assert_eq!(
            exit_code_with_termination_signal(killed, Some(Signal::SIGINT)).unwrap(),
            130
        );
        assert_eq!(
            exit_code_with_termination_signal(killed, None).unwrap(),
            137
        );
        // Init can exit by itself just before the runtime kills it.
        let exited = WaitStatus::Exited(pid, 3);
        assert_eq!(
            exit_code_with_termination_signal(exited, Some(Signal::SIGTERM)).unwrap(),
            3
        );
    }
}
