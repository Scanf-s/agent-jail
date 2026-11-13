//! Process descriptors, which refer to one process even after another process gets its PID.

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::Pid;
use std::os::fd::{AsFd, FromRawFd, OwnedFd, RawFd};

/// Open a descriptor of the process `pid`. It is close-on-exec, and it becomes readable when
/// the process ends. Linux 5.3 added pidfd_open.
pub fn open(pid: Pid) -> Result<OwnedFd> {
    // SAFETY: pidfd_open only creates a descriptor.
    let result = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) };
    let fd = Errno::result(result).with_context(|| format!("pidfd_open({pid})"))?;
    // SAFETY: the kernel returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether the process of `pidfd` has ended.
pub fn has_ended(pidfd: &OwnedFd) -> Result<bool> {
    let mut polled = [PollFd::new(pidfd.as_fd(), PollFlags::POLLIN)];
    loop {
        match poll(&mut polled, PollTimeout::ZERO) {
            Err(Errno::EINTR) => continue,
            result => return Ok(result.context("poll a pidfd")? > 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_when_the_process_ends() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pidfd = open(Pid::from_raw(child.id() as i32)).unwrap();
        assert!(!has_ended(&pidfd).unwrap());

        child.kill().unwrap();
        child.wait().unwrap();

        assert!(has_ended(&pidfd).unwrap());
    }
}
