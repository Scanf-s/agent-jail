use crate::cgroups::Cgroup;
use crate::config::ContainerConfig;
use crate::container;
use crate::filesystem::PivotRoot;
use crate::user::Mapping;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Gid, Pid, Uid, chdir, fork, setgid, setuid};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

const CGROUP_READY: u8 = 1;
const USER_NAMESPACE_READY: u8 = 2;
const MAPPING_READY: u8 = 3;

pub struct Runtime {
    config: ContainerConfig,
    cgroup: Cgroup,
    parent_end: Option<UnixStream>,
    child_end: Option<UnixStream>,
}

impl Runtime {
    pub fn new(config: ContainerConfig) -> Result<Self> {
        let cgroup = Cgroup::new()?;
        cgroup.configure(&config.resources)?;

        // ipc setup for the parent and the child process communication
        let (parent_end, child_end) = UnixStream::pair()?;

        Ok(Self {
            config,
            cgroup,
            parent_end: Some(parent_end),
            child_end: Some(child_end),
        })
    }

    pub fn run(mut self) -> Result<ExitCode> {
        match unsafe { fork() }.context("fork failed")? {
            ForkResult::Parent { child } => self.run_parent(child),
            ForkResult::Child => self.run_setup_process(),
        }
    }

    fn run_parent(&mut self, child: Pid) -> Result<ExitCode> {
        drop(self.child_end.take());
        let mut stream = self.parent_end.take().context("parent_end already used")?;

        if let Err(error) = self.setup_parent(child, &mut stream) {
            let _ = kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(error).context("container setup failed; child killed");
        }

        let status = waitpid(child, None).context("waitpid failed")?;
        Ok(ExitCode::from(exit_status(status)?))
    }

    fn setup_parent(&mut self, child: Pid, stream: &mut UnixStream) -> Result<()> {
        // The setup process waits until it belongs to the cgroup.
        self.cgroup
            .add_pid(child)
            .context("add setup child to cgroup")?;
        stream
            .write_all(&[CGROUP_READY])
            .context("send cgroup ready")?;

        // UID/GID maps can be written after the child creates its user namespace.
        expect(stream, USER_NAMESPACE_READY, "user namespace")?;
        Mapping::new(
            child,
            Uid::from_raw(self.config.id_mapping.host_uid),
            Gid::from_raw(self.config.id_mapping.host_gid),
        )
        .map()
        .context("map container UID/GID")?;
        stream
            .write_all(&[MAPPING_READY])
            .context("send mapping ready")?;
        Ok(())
    }

    fn run_setup_process(&mut self) -> ! {
        if let Err(error) = self.setup_process() {
            eprintln!("container-runtime: setup process failed: {error:#}");
            std::process::exit(127);
        }
        unreachable!("setup process returned without exiting");
    }

    fn setup_process(&mut self) -> Result<()> {
        drop(self.parent_end.take());
        let mut stream = self.child_end.take().context("child_end already used")?;
        expect(&mut stream, CGROUP_READY, "cgroup")?;

        // call unshare to isolate the user namespace
        unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER)")?;
        stream
            .write_all(&[USER_NAMESPACE_READY])
            .context("send user namespace ready")?;

        expect(&mut stream, MAPPING_READY, "mapping")?;
        drop(stream);
        self.launch_init()
    }

    fn launch_init(&self) -> Result<()> {
        // The next fork places container init at PID 1 in the new PID namespace.
        unshare(CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNET)
            .context("unshare(CLONE_NEWPID | CLONE_NEWNET)")?;

        match unsafe { fork() }.context("fork (setup) failed")? {
            ForkResult::Parent { child } => {
                let status = waitpid(child, None).context("waitpid(child) failed")?;
                std::process::exit(i32::from(exit_status(status)?));
            }
            ForkResult::Child => {
                if let Err(error) = self.run_init() {
                    eprintln!("container-runtime: container init failed: {error:#}");
                    std::process::exit(127);
                }
                unreachable!("container init returned without executing command");
            }
        }
    }

    fn run_init(&self) -> Result<()> {
        // file system isolation
        PivotRoot::new(self.config.rootfs.clone()).isolate_filesystem()?;

        let process = &self.config.process;
        chdir(&process.cwd)
            .with_context(|| format!("chdir to working directory {}", process.cwd.display()))?;
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

fn expect(stream: &mut UnixStream, expected: u8, stage: &str) -> Result<()> {
    let mut byte = [0u8; 1];
    stream
        .read_exact(&mut byte)
        .with_context(|| format!("wait for {stage}"))?;
    if byte[0] != expected {
        bail!("unexpected {stage} message: {}", byte[0]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expect_accepts_the_expected_message() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[CGROUP_READY]).unwrap();

        expect(&mut reader, CGROUP_READY, "cgroup").unwrap();
    }

    #[test]
    fn expect_rejects_an_unexpected_message() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[MAPPING_READY]).unwrap();

        let error = expect(&mut reader, CGROUP_READY, "cgroup").unwrap_err();
        assert!(error.to_string().contains("unexpected cgroup message: 3"));
    }

    #[test]
    fn expect_fails_when_peer_closes_before_sending() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        drop(writer);

        let error = expect(&mut reader, CGROUP_READY, "cgroup").unwrap_err();
        assert!(error.to_string().contains("wait for cgroup"));
    }

    #[test]
    fn expect_reads_buffered_messages_one_at_a_time() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&[CGROUP_READY, MAPPING_READY]).unwrap();

        expect(&mut reader, CGROUP_READY, "cgroup").unwrap();
        expect(&mut reader, MAPPING_READY, "mapping").unwrap();
    }
}
