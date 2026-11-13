use crate::cgroups::{self, Cgroup};
use crate::config::{ContainerConfig, NetworkMode};
use crate::container;
use crate::filesystem::{PivotRoot, RunDir};
use crate::ipc::{ChildChannels, StartupChannels};
use crate::network;
use crate::pidfd;
use crate::proc_stat;
use crate::signals::{self, RuntimeSignals};
use crate::state::{Record, StateDir};
use crate::user::Mapping;
use anyhow::{Context, Result, ensure};
use nix::sched::{CloneFlags, unshare};
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::waitpid;
use nix::unistd::{
    ForkResult, Gid, Pid, Uid, chdir, dup2, fork, getppid, setgid, setgroups, setsid, setuid,
};
use oci_spec::runtime::ContainerState;
use start::StartSocket;
use std::fs::OpenOptions;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use supervision::{Outcome, Startup, supervise, watch_setup_process};

mod start;
mod startup;
mod supervision;

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
        let runtime = Pid::this();
        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => self.run_parent(child, &signals, mode),
            ForkResult::Child => self.run_setup_process(&signals, mode, runtime),
        }
    }

    fn run_parent(&mut self, child: Pid, signals: &RuntimeSignals, mode: Mode) -> Result<ExitCode> {
        let result = match self.wait_for_container(child, signals, mode) {
            Ok(Outcome::Created) => {
                // The setup process removes the run directory when init ends, also if init has
                // ended already. The state stays until the container is deleted. Dropping the
                // cgroup removes it only after init ended, because a cgroup that holds a
                // process cannot be removed.
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

        let cgroup = &self.cgroup;
        watch_setup_process(child, signals, created, mode, |cancelled| {
            // Init has its own cgroup now. In the container cgroup, the setup process would
            // count against the memory limit of the container, and the OOM killer could end it.
            let moved = cgroup.move_to_runtime(child);
            match cancelled {
                None => {
                    if let Err(error) = moved {
                        report!("aj: warning: {error:#}");
                    }
                }
                // The setup process created the container before it handled the signal. If a
                // quick `aj start` has run the program, the program may ignore the signal, so
                // this ends the container. A setup process that has ended cannot move, and
                // then nothing waits for this to end.
                Some(_) => {
                    if let Err(error) = cgroup.kill_all() {
                        report!("aj: warning: {error:#}");
                    }
                }
            }
        })
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

    /// Run the setup process, a child of the runtime process `runtime`.
    fn run_setup_process(&mut self, signals: &RuntimeSignals, mode: Mode, runtime: Pid) -> ! {
        if let Err(error) = self.setup_process(signals, mode, runtime) {
            report!("aj: setup process failed: {error:#}");
            std::process::exit(127);
        }
        unreachable!("setup process returned without exiting");
    }

    fn setup_process(&mut self, signals: &RuntimeSignals, mode: Mode, runtime: Pid) -> Result<()> {
        let runtime = match mode {
            // The container of `aj run` ends with the run, also when its runtime is killed.
            Mode::Run => Some(watch_runtime(runtime)?),
            // The container stays after `aj create` returns. In a session of its own, it does
            // not receive the signals of the caller's terminal, for example Ctrl-C for a later
            // command. Until then, the runtime passes those signals on.
            Mode::Create => {
                setsid().context("create a session for the container")?;
                None
            }
        };
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

        self.launch_init(signals, ipc, mode, runtime)
    }

    /// Fork container init and supervise it. `runtime` is a pidfd of the runtime process of
    /// `aj run`.
    fn launch_init(
        &mut self,
        signals: &RuntimeSignals,
        mut ipc: ChildChannels,
        mode: Mode,
        runtime: Option<OwnedFd>,
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
                let on_startup = |startup: Startup| match startup {
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
                };
                let watched = runtime.as_ref().map(AsFd::as_fd);
                let code = supervise(child, signals, channel, socket, watched, on_startup);
                self.record_stopped(code.as_ref().ok().copied());
                // Only a process in the user namespace can remove files of subordinate IDs.
                // The overlay is already gone, because init's mount namespace ended with it.
                if let Err(error) = self.run_dir.remove() {
                    report!("aj: warning: {error:#}");
                }
                let abandoned = match mode {
                    // Without its runtime, nothing else removes the state of `aj run`.
                    Mode::Run => runtime
                        .as_ref()
                        .is_some_and(|runtime| pidfd::has_ended(runtime).unwrap_or(false)),
                    // A failed `aj create` must leave no container, also when it ended before
                    // it could clean up.
                    Mode::Create => !reported,
                };
                if abandoned && let Err(error) = self.state_dir.remove() {
                    report!("aj: warning: {error:#}");
                }
                std::process::exit(i32::from(code?));
            }
            ForkResult::Child => {
                drop((channel, ipc, socket, runtime));
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
                // A container prepared by `create` stays until it is deleted.
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

/// Open a pidfd of `runtime`, the parent of this setup process.
fn watch_runtime(runtime: Pid) -> Result<OwnedFd> {
    let pidfd = pidfd::open(runtime)?;
    // If the runtime ended before pidfd_open, another process may have its PID now. While the
    // runtime lives, it stays the parent of this process.
    ensure!(getppid() == runtime, "the runtime ended");
    Ok(pidfd)
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
