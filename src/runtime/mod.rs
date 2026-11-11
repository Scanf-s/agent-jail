use crate::cgroups::{self, Cgroup};
use crate::config::{ContainerConfig, NetworkMode};
use crate::container;
use crate::filesystem::{PivotRoot, RunDir};
use crate::ipc::{self, ChildChannels, StartupChannels};
use crate::network;
use crate::proc_stat;
use crate::signals::{self, Event, RuntimeSignals, TERMINATION_SIGNALS};
use crate::state::{Record, StateDir};
use crate::user::Mapping;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
use nix::unistd::{
    ForkResult, Gid, Pid, Uid, chdir, dup2, fork, setgid, setgroups, setsid, setuid,
};
use oci_spec::runtime::ContainerState;
use start::{StartRequest, StartSocket};
use startup::Report;
use std::fs::OpenOptions;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

mod start;
mod startup;

pub use start::start;

/// What the runtime does with a container that it created.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Run the program at once, wait until it ends, and remove the container, as `run` does.
    Run,
    /// Exit when the container is created, and keep it until a later command starts it, as
    /// OCI `create` does.
    Create,
}

/// How the wait of the runtime for the setup process ended.
enum Outcome {
    /// `create` created the container.
    Created,
    /// The setup process ended with this exit code.
    Exited(u8),
}

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

    /// Run the program of the container, wait until it ends, and remove the container.
    pub fn run(self) -> Result<ExitCode> {
        self.launch(Mode::Run)
    }

    /// Create the container and return. Its program waits until `start`.
    pub fn create(self) -> Result<ExitCode> {
        self.launch(Mode::Create)
    }

    fn launch(mut self, mode: Mode) -> Result<ExitCode> {
        let signals = RuntimeSignals::block()?;
        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => self.run_parent(child, &signals, mode),
            ForkResult::Child => self.run_setup_process(&signals, mode),
        }
    }

    fn run_parent(&mut self, child: Pid, signals: &RuntimeSignals, mode: Mode) -> Result<ExitCode> {
        let result = match self.wait_for_container(child, signals, mode) {
            Ok(Outcome::Created) => {
                // The setup process now waits for the start, and it removes the run directory
                // when init ends. The state stays until the container is deleted. The container
                // cgroup holds init, so dropping it here does not remove it.
                self.state_dir.keep();
                return Ok(ExitCode::SUCCESS);
            }
            Ok(Outcome::Exited(code)) => Ok(ExitCode::from(code)),
            Err(error) => Err(error),
        };
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

    fn wait_for_container(
        &mut self,
        child: Pid,
        signals: &RuntimeSignals,
        mode: Mode,
    ) -> Result<Outcome> {
        let created = match self.setup_parent(child) {
            Ok(created) => created,
            Err(error) => {
                let _ = kill(child, Signal::SIGKILL);
                let _ = waitpid(child, None);
                return Err(error).context("container setup failed; child killed");
            }
        };

        let forwarded: &[Signal] = match mode {
            // A terminal sends SIGINT and SIGQUIT to the whole foreground process group, so the
            // setup process and the container already receive them.
            Mode::Run => &[Signal::SIGTERM, Signal::SIGHUP],
            // The setup process has a session of its own.
            Mode::Create => &TERMINATION_SIGNALS,
        };
        let mut created = Some(created);
        loop {
            match wait_for_setup_process(child, signals, created.take(), forwarded)? {
                SetupEvent::Created => {
                    // Init has its own cgroup now. In the container cgroup, the setup process
                    // would count against the memory limit of the container, and the OOM killer
                    // could end it.
                    if let Err(error) = self.cgroup.move_to_runtime(child) {
                        report!("aj: warning: {error:#}");
                    }
                    if mode == Mode::Create {
                        return Ok(Outcome::Created);
                    }
                }
                SetupEvent::Ended(status) => return Ok(Outcome::Exited(exit_status(status)?)),
            }
        }
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

    fn run_setup_process(&mut self, signals: &RuntimeSignals, mode: Mode) -> ! {
        if let Err(error) = self.setup_process(signals, mode) {
            report!("aj: setup process failed: {error:#}");
            std::process::exit(127);
        }
        unreachable!("setup process returned without exiting");
    }

    fn setup_process(&mut self, signals: &RuntimeSignals, mode: Mode) -> Result<()> {
        if mode == Mode::Create {
            // The container stays after `aj create` returns. In a session of its own, it does
            // not receive the signals of the caller's terminal, for example Ctrl-C for a later
            // command. Until then, the runtime passes those signals on.
            setsid().context("create a session for the container")?;
        }
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

        self.launch_init(signals, ipc, mode)
    }

    fn launch_init(
        &mut self,
        signals: &RuntimeSignals,
        mut ipc: ChildChannels,
        mode: Mode,
    ) -> Result<()> {
        let (channel, init_channel) = startup::channel()?;
        let socket = match mode {
            Mode::Run => None,
            Mode::Create => Some(StartSocket::listen(&self.state_dir)?),
        };
        match unsafe { fork() }.context("fork (setup) failed")? {
            ForkResult::Parent { child } => {
                drop(init_channel);
                // Whether `aj create` reported the container as created.
                let mut reported = false;
                let code = supervise(child, signals, channel, socket, |startup| match startup {
                    // A created container must report the PID of init, so the program does
                    // not start without the record.
                    Startup::Created => {
                        self.record_created(child, mode)?;
                        match mode {
                            Mode::Run => {
                                if let Err(error) = ipc.signal_created() {
                                    report!("aj: warning: {error:#}");
                                }
                            }
                            // Without the report, `aj create` fails, so the container must
                            // not stay.
                            Mode::Create => {
                                ipc.signal_created()
                                    .context("report the container to aj create")?;
                                reported = true;
                                detach()?;
                            }
                        }
                        Ok(())
                    }
                    Startup::Running => match (self.record_running(), mode) {
                        (Ok(()), _) => Ok(()),
                        // The run removes the state when the program ends.
                        (Err(error), Mode::Run) => {
                            report!("aj: warning: {error:#}");
                            Ok(())
                        }
                        // The state would show a program that runs as created.
                        (Err(error), Mode::Create) => Err(error),
                    },
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
                // A failed `aj create` must leave no container, also when it ended before it
                // could clean up.
                if mode == Mode::Create
                    && !reported
                    && let Err(error) = self.state_dir.remove()
                {
                    report!("aj: warning: {error:#}");
                }
                std::process::exit(i32::from(code?));
            }
            ForkResult::Child => {
                drop((channel, ipc, socket));
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
    fn record_created(&mut self, init: Pid, mode: Mode) -> Result<()> {
        let start_time = proc_stat::start_time(init)?;
        self.state_dir
            .update(|record| {
                record
                    .oci
                    .set_status(ContainerState::Created)
                    .set_pid(Some(init.as_raw()));
                record.init_start_time = Some(start_time);
                // From now on, the user knows the container, so it stays until deleted.
                record.auto_delete = mode == Mode::Run;
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

/// Let the setup process outlive `aj create`, which returns after this.
///
/// The caller may close the other ends of the standard streams, and a working directory keeps
/// its file system busy. Init keeps its own copies of the streams, as OCI requires.
fn detach() -> Result<()> {
    chdir("/").context("chdir to /")?;
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("open /dev/null")?;
    for stream in 0..=2 {
        dup2(null.as_raw_fd(), stream).context("replace a standard stream")?;
    }
    Ok(())
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

/// What the runtime saw of the setup process.
enum SetupEvent {
    /// The setup process reported that the container is created.
    Created,
    /// The setup process ended with this status.
    Ended(WaitStatus),
}

/// Wait until the setup process reports on `created` that the container is created, or until
/// it ends. Without `created`, wait until it ends.
///
/// The signals in `forwarded` usually reach only the runtime, for example from `kill` or a
/// process manager, so they go to the setup process.
fn wait_for_setup_process(
    child: Pid,
    signals: &RuntimeSignals,
    mut created: Option<OwnedFd>,
    forwarded: &[Signal],
) -> Result<SetupEvent> {
    loop {
        let event = match &created {
            Some(pipe) => signals.wait_or_readable(&[pipe.as_fd()]),
            None => signals.wait().map(Event::Signal),
        };
        let signal = match event {
            Ok(Event::Signal(signal)) => signal,
            Ok(Event::Readable(_)) => {
                // At EOF, the setup process ended before it reported, and SIGCHLD follows.
                if let Some(pipe) = created.take()
                    && ipc::read_ready(&pipe, "created")?
                {
                    return Ok(SetupEvent::Created);
                }
                continue;
            }
            // Returning early would remove the run directory while the container still uses it.
            Err(error) => {
                report!("aj: warning: {error:#}");
                let status = waitpid(child, None).context("waitpid failed")?;
                return Ok(SetupEvent::Ended(status));
            }
        };
        match signal {
            Signal::SIGCHLD => {
                match waitpid(child, Some(WaitPidFlag::WNOHANG)).context("waitpid failed")? {
                    WaitStatus::StillAlive => {}
                    status => return Ok(SetupEvent::Ended(status)),
                }
            }
            signal if forwarded.contains(&signal) => {
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
/// init does before its program runs to `on_startup`. After `on_startup` accepted
/// `Startup::Created`, this starts the program: at once without `socket`, or when `aj start`
/// asks on `socket`.
///
/// If waiting fails, this kills and reaps init, so that the caller can remove the run directory.
fn supervise(
    init: Pid,
    signals: &RuntimeSignals,
    channel: UnixStream,
    socket: Option<StartSocket>,
    on_startup: impl FnMut(Startup) -> Result<()>,
) -> Result<u8> {
    let result = wait_for_init(init, signals, channel, socket, on_startup);
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
    mut socket: Option<StartSocket>,
    mut on_startup: impl FnMut(Startup) -> Result<()>,
) -> Result<u8> {
    let start_at_once = socket.is_none();
    let mut channel = Some(channel);
    // Whether init is ready to execute the program.
    let mut created = false;
    // Whether init was told to execute the program.
    let mut started = false;
    // The `aj start` that waits until the program runs.
    let mut request = None;
    // The termination signal for which the runtime killed init.
    let mut termination_signal = None;
    loop {
        let mut fds = Vec::new();
        if let Some(channel) = &channel {
            fds.push(channel.as_fd());
        }
        // A request that comes earlier waits in the socket.
        if created && let Some(socket) = &socket {
            fds.push(socket.as_fd());
        }
        // While init starts, signals must still reach this loop. Otherwise Ctrl-C could not end
        // an init that hangs before exec.
        let event = if fds.is_empty() {
            Event::Signal(signals.wait()?)
        } else {
            signals.wait_or_readable(&fds)?
        };
        let signal = match event {
            Event::Signal(signal) => signal,
            // While the channel is open, it comes first.
            Event::Readable(0) if channel.is_some() => {
                let Some(open) = &channel else {
                    continue;
                };
                match startup::read_report(open)? {
                    Report::Ready => {
                        on_startup(Startup::Created)?;
                        created = true;
                        if start_at_once {
                            startup::start(open)?;
                            started = true;
                        }
                    }
                    report => {
                        channel = None;
                        conclude(conclusion(report, started), &mut on_startup, request.take())?;
                    }
                }
                continue;
            }
            Event::Readable(_) => {
                let Some(listening) = &socket else {
                    continue;
                };
                let pending = match listening.accept() {
                    Ok(Some(pending)) => pending,
                    Ok(None) => continue,
                    // A failed connection does not change the container.
                    Err(error) => {
                        report!("aj: warning: {error:#}");
                        continue;
                    }
                };
                // Without the channel, init has ended, and dropping the request tells the
                // client that the program did not start.
                let Some(open) = &channel else {
                    continue;
                };
                startup::start(open)?;
                started = true;
                request = Some(pending);
                if let Some(socket) = socket.take() {
                    socket.close();
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
                                conclude(
                                    conclusion(report, started),
                                    &mut on_startup,
                                    request.take(),
                                )?;
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

/// Pass `startup`, what the startup channel showed at its end, to `on_startup`, and answer
/// `request` with the result.
fn conclude(
    startup: Option<Startup>,
    on_startup: &mut impl FnMut(Startup) -> Result<()>,
    request: Option<StartRequest>,
) -> Result<()> {
    let (result, answer) = match startup {
        Some(Startup::Running) => match on_startup(Startup::Running) {
            Ok(()) => (Ok(()), Ok(())),
            Err(error) => {
                let message = format!("{error:#}");
                (Err(error), Err(message))
            }
        },
        Some(Startup::Failed(message)) => {
            (on_startup(Startup::Failed(message.clone())), Err(message))
        }
        // The channel ends without a created report. Dropping the request tells the client
        // that the program did not start.
        Some(Startup::Created) | None => return Ok(()),
    };
    if let Some(request) = request {
        request.answer(answer);
    }
    result
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
