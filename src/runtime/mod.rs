use crate::cgroups::{self, Cgroup};
use crate::config::{ContainerConfig, NetworkMode};
use crate::container;
use crate::filesystem::{PivotRoot, RunDir};
use crate::ipc::{self, ChildChannels, StartupChannels};
use crate::network;
use crate::proc_stat;
use crate::signals::{self, Event, RuntimeSignals};
use crate::state::{Record, StateDir};
use crate::user::Mapping;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
use nix::unistd::{ForkResult, Gid, Pid, Uid, chdir, fork, setgid, setgroups, setuid};
use oci_spec::runtime::ContainerState;
use startup::Report;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

mod startup;

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
        let run_dir = RunDir::create(&id, cgroup.path())?;

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
            report!("aj: warning: {error:#}");
        }
        match ended {
            Ok(()) => {
                if let Err(error) = self.state_dir.remove() {
                    report!("aj: warning: {error:#}");
                }
            }
            // The state keeps the container visible in `aj list` and its ID in use while its
            // processes may still run.
            Err(error) => {
                report!(
                    "aj: warning: end the processes left in the container, so its state stays: \
                     {error:#}"
                );
                self.state_dir.keep();
            }
        }
        result
    }

    fn wait_for_container(&mut self, child: Pid, signals: &RuntimeSignals) -> Result<ExitCode> {
        let created = match self.setup_parent(child) {
            Ok(created) => created,
            Err(error) => {
                let _ = kill(child, Signal::SIGKILL);
                let _ = waitpid(child, None);
                return Err(error).context("container setup failed; child killed");
            }
        };

        let status = wait_for_setup_process(child, signals, created, || {
            // Init has its own cgroup now. In the container cgroup, the setup process would
            // count against the memory limit of the container, and the OOM killer could end it.
            if let Err(error) = self.cgroup.move_to_runtime(child) {
                report!("aj: warning: {error:#}");
            }
        })?;
        Ok(ExitCode::from(exit_status(status)?))
    }

    /// Set up the container with the setup process `child`. Returns the read end of the pipe on
    /// which the setup process reports that the container is created.
    fn setup_parent(&mut self, child: Pid) -> Result<OwnedFd> {
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
        ipc.take_created_reader()
    }

    fn run_setup_process(&mut self, signals: &RuntimeSignals) -> ! {
        if let Err(error) = self.setup_process(signals) {
            report!("aj: setup process failed: {error:#}");
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

        self.launch_init(signals, ipc)
    }

    fn launch_init(&mut self, signals: &RuntimeSignals, mut ipc: ChildChannels) -> Result<()> {
        let (channel, init_channel) = startup::channel()?;
        match unsafe { fork() }.context("fork (setup) failed")? {
            ForkResult::Parent { child } => {
                drop(init_channel);
                let code = supervise(child, signals, channel, |startup| match startup {
                    // A created container must report the PID of init, so the program does
                    // not start without the record.
                    Startup::Created => {
                        self.record_created(child)?;
                        if let Err(error) = ipc.signal_created() {
                            report!("aj: warning: {error:#}");
                        }
                        Ok(())
                    }
                    Startup::Running => {
                        if let Err(error) = self.record_running() {
                            report!("aj: warning: {error:#}");
                        }
                        Ok(())
                    }
                    Startup::Failed(message) => {
                        report!("aj: container init failed: {message}");
                        Ok(())
                    }
                });
                self.record_stopped(code.as_ref().ok().copied());
                // Only a process in the user namespace can remove files of subordinate IDs.
                // The overlay is already gone, because init's mount namespace ended with it.
                if let Err(error) = self.run_dir.remove() {
                    report!("aj: warning: {error:#}");
                }
                std::process::exit(i32::from(code?));
            }
            ForkResult::Child => {
                drop((channel, ipc));
                // The locks show that a runtime process is alive. Init is not one, and it can
                // wait for the start of the program for a long time.
                self.state_dir.close_lock();
                self.run_dir.close_lock();
                if let Err(error) = self.run_init(&init_channel) {
                    // The setup process prints the error.
                    let _ = startup::send_failure(&init_channel, &error);
                    std::process::exit(127);
                }
                unreachable!("container init returned without executing command");
            }
        }
    }

    /// Record that container init is ready to execute the program.
    fn record_created(&mut self, init: Pid) -> Result<()> {
        let start_time = proc_stat::start_time(init)?;
        self.state_dir
            .update(|record| {
                record
                    .oci
                    .set_status(ContainerState::Created)
                    .set_pid(Some(init.as_raw()));
                record.init_start_time = Some(start_time);
            })
            .context("record that the container is created")
    }

    fn record_running(&mut self) -> Result<()> {
        self.state_dir
            .update(|record| {
                record.oci.set_status(ContainerState::Running);
            })
            .context("record that the container runs")
    }

    /// Record that the container program ended, with its exit code if the runtime knows it.
    fn record_stopped(&mut self, exit_code: Option<u8>) {
        let result = self.state_dir.update(|record| {
            record.oci.set_status(ContainerState::Stopped);
            record.exit_code = exit_code;
        });
        if let Err(error) = result {
            report!("aj: warning: record that the container stopped: {error:#}");
        }
    }

    /// Prepare the container and execute its program. `startup` is the end of the startup
    /// channel that init keeps.
    fn run_init(&self, startup: &UnixStream) -> Result<()> {
        // The setup of the container can take a while, for example to mount the overlay.
        end_with_monitor()?;
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
        // A change of the user clears the parent death signal.
        end_with_monitor()?;
        startup::wait_for_start(startup)?;

        signals::reset_for_exec()?;
        container::exec(&process.args, &process.env)
    }
}

/// Make the kernel kill this process, container init, when the setup process ends.
///
/// The setup process waits for init and cleans up after it, so the container must not run
/// without it. The program can still clear the setting, for example by changing its user. The
/// cleanup of a later run ends such a container.
///
/// If the setup process ends before the setting takes effect, init finds the startup channel
/// closed when it waits for the start of the program.
fn end_with_monitor() -> Result<()> {
    set_pdeathsig(Signal::SIGKILL).context("set the parent death signal")
}

/// Wait until the setup process ends.
///
/// `created` is the read end of the pipe on which the setup process reports that the container
/// is created. Then this calls `on_created`.
///
/// SIGTERM and SIGHUP usually reach only the runtime, for example from `kill` or a process
/// manager, so they go to the setup process. A terminal sends SIGINT and SIGQUIT to the whole
/// foreground process group, so the setup process and the container already receive them.
fn wait_for_setup_process(
    child: Pid,
    signals: &RuntimeSignals,
    created: OwnedFd,
    mut on_created: impl FnMut(),
) -> Result<WaitStatus> {
    let mut created = Some(created);
    loop {
        let event = match &created {
            Some(pipe) => signals.wait_or_readable(pipe.as_fd()),
            None => signals.wait().map(Event::Signal),
        };
        let signal = match event {
            Ok(Event::Signal(signal)) => signal,
            Ok(Event::Readable) => {
                // At EOF, the setup process ended before it reported, and SIGCHLD follows.
                if let Some(pipe) = created.take()
                    && ipc::read_ready(&pipe, "created")?
                {
                    on_created();
                }
                continue;
            }
            // Returning early would remove the run directory while the container still uses it.
            Err(error) => {
                report!("aj: warning: {error:#}");
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

/// What container init did before its program ran.
enum Startup {
    /// Init is ready to execute the program.
    Created,
    /// Init executed the program.
    Running,
    /// Init cannot run the program, for this reason.
    Failed(String),
}

/// Wait until container init ends, and return the exit code for the runtime.
///
/// `channel` is the end of the startup channel that the setup process keeps. This passes what
/// init does before its program runs to `on_startup`, and starts the program after
/// `on_startup` accepted `Startup::Created`.
///
/// If waiting fails, this kills and reaps init, so that the caller can remove the run directory.
fn supervise(
    init: Pid,
    signals: &RuntimeSignals,
    channel: UnixStream,
    on_startup: impl FnMut(Startup) -> Result<()>,
) -> Result<u8> {
    let result = wait_for_init(init, signals, channel, on_startup);
    if result.is_err() {
        let _ = kill(init, Signal::SIGKILL);
        let _ = waitpid(init, None);
    }
    result
}

fn wait_for_init(
    init: Pid,
    signals: &RuntimeSignals,
    channel: UnixStream,
    mut on_startup: impl FnMut(Startup) -> Result<()>,
) -> Result<u8> {
    let mut channel = Some(channel);
    // Whether init was told to execute the program.
    let mut started = false;
    // The termination signal for which the runtime killed init.
    let mut termination_signal = None;
    loop {
        // While init starts, signals must still reach this loop. Otherwise Ctrl-C could not end
        // an init that hangs before exec.
        let event = match &channel {
            Some(channel) => signals.wait_or_readable(channel.as_fd())?,
            None => Event::Signal(signals.wait()?),
        };
        let signal = match event {
            Event::Signal(signal) => signal,
            Event::Readable => {
                let Some(open) = &channel else {
                    continue;
                };
                match startup::read_report(open)? {
                    Report::Ready => {
                        on_startup(Startup::Created)?;
                        startup::start(open)?;
                        started = true;
                    }
                    report => {
                        channel = None;
                        if let Some(startup) = conclusion(report, started) {
                            on_startup(startup)?;
                        }
                    }
                }
                continue;
            }
        };
        match signal {
            Signal::SIGCHLD => {
                if !has_ended(init)? {
                    continue;
                }
                // Init can report and end before this loop reads the report. Its end of the
                // channel closed when it ended, so these reads do not block.
                if let Some(open) = channel.take() {
                    loop {
                        match startup::read_report(&open)? {
                            // The program cannot start any more.
                            Report::Ready => {}
                            report => {
                                if let Some(startup) = conclusion(report, started) {
                                    on_startup(startup)?;
                                }
                                break;
                            }
                        }
                    }
                }
                let status = waitpid(init, None).context("waitpid(init) failed")?;
                return exit_code_with_termination_signal(status, termination_signal);
            }
            signal => {
                let default_action = match signals::uses_default_action(init, signal) {
                    Ok(default_action) => default_action,
                    // Passing the signal on leaves the decision to the program, as if it had
                    // a handler.
                    Err(error) => {
                        report!("aj: warning: pass {signal} on to the container: {error:#}");
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

/// What the last report of init on the startup channel shows, if `started` tells whether init
/// was told to execute the program.
fn conclusion(report: Report, started: bool) -> Option<Startup> {
    match report {
        Report::Ready => None,
        // After the start, init closes its end when the exec succeeds. The program may have
        // ended already, as /bin/true does.
        Report::Closed if started => Some(Startup::Running),
        // Init ended before it could run the program, for example on SIGKILL.
        Report::Closed => None,
        Report::Failed(message) => Some(Startup::Failed(message)),
    }
}

/// Whether `init` has ended. This does not reap it, so its exit status and its `/proc` entry
/// stay.
fn has_ended(init: Pid) -> Result<bool> {
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    let status = waitid(Id::Pid(init), flags).context("waitid(init) failed")?;
    Ok(status != WaitStatus::StillAlive)
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
