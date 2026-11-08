use crate::cgroups::{self, Cgroup};
use crate::config::ContainerConfig;
use crate::container;
use crate::filesystem::{PivotRoot, RunDir};
use crate::ipc::StartupChannels;
use crate::user::Mapping;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Gid, Pid, Uid, chdir, fork, setgid, setgroups, setuid};
use std::process::ExitCode;

pub struct Runtime {
    config: ContainerConfig,
    mapping: Mapping,
    cgroup: Cgroup,
    run_dir: RunDir,
    channels: Option<StartupChannels>,
}

impl Runtime {
    pub fn new(config: ContainerConfig, mapping: Mapping) -> Result<Self> {
        // This may replace the process, so it runs before the runtime opens any resource.
        let delegated = cgroups::enter_delegated_cgroup()?;
        let cgroup = Cgroup::new(&delegated)?;
        cgroup.configure(&config.resources)?;
        let run_dir = RunDir::create()?;

        // ipc setup for the parent and the child process communication
        let channels = StartupChannels::new()?;

        Ok(Self {
            config,
            mapping,
            cgroup,
            run_dir,
            channels: Some(channels),
        })
    }

    pub fn run(mut self) -> Result<ExitCode> {
        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => self.run_parent(child),
            ForkResult::Child => self.run_setup_process(),
        }
    }

    fn run_parent(&mut self, child: Pid) -> Result<ExitCode> {
        let result = self.wait_for_container(child);
        // The setup process removes the run directory. This covers a run that fails before that.
        if let Err(error) = self.run_dir.remove() {
            eprintln!("aj: warning: {error:#}");
        }
        result
    }

    fn wait_for_container(&mut self, child: Pid) -> Result<ExitCode> {
        if let Err(error) = self.setup_parent(child) {
            let _ = kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(error).context("container setup failed; child killed");
        }

        let status = waitpid(child, None).context("waitpid failed")?;
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
        Ok(())
    }

    fn run_setup_process(&mut self) -> ! {
        if let Err(error) = self.setup_process() {
            eprintln!("aj: setup process failed: {error:#}");
            std::process::exit(127);
        }
        unreachable!("setup process returned without exiting");
    }

    fn setup_process(&mut self) -> Result<()> {
        let channels = self
            .channels
            .take()
            .context("startup channels already used")?;
        let mut ipc = channels.for_child();
        ipc.wait_for_cgroup()?;

        // call unshare to isolate the user namespace
        unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER)")?;
        ipc.signal_user_namespace_ready()?;

        ipc.wait_for_mapping()?;
        self.launch_init()
    }

    fn launch_init(&self) -> Result<()> {
        // The next fork places container init at PID 1 in the new PID namespace.
        // This process already belongs to the container cgroup, so the new cgroup namespace
        // shows that cgroup as its root.
        unshare(CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWCGROUP)
            .context("unshare(CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWCGROUP)")?;

        match unsafe { fork() }.context("fork (setup) failed")? {
            ForkResult::Parent { child } => {
                let status = waitpid(child, None).context("waitpid(child) failed")?;
                // Only a process in the user namespace can remove files of subordinate IDs.
                // The overlay is already gone, because init's mount namespace ended with it.
                if let Err(error) = self.run_dir.remove() {
                    eprintln!("aj: warning: {error:#}");
                }
                std::process::exit(i32::from(exit_status(status)?));
            }
            ForkResult::Child => {
                if let Err(error) = self.run_init() {
                    eprintln!("aj: container init failed: {error:#}");
                    std::process::exit(127);
                }
                unreachable!("container init returned without executing command");
            }
        }
    }

    fn run_init(&self) -> Result<()> {
        // file system isolation
        PivotRoot::new(
            self.config.rootfs.clone(),
            &self.run_dir,
            &self.config.volumes,
        )
        .isolate_filesystem()?;

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

        container::exec(&process.args, &process.env)
    }
}

fn exit_status(status: WaitStatus) -> Result<u8> {
    match status {
        WaitStatus::Exited(_, code) => Ok(code as u8),
        WaitStatus::Signaled(_, signal, _) => Ok(128 + signal as u8),
        other => bail!("unexpected wait status: {other:?}"),
    }
}
