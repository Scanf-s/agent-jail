//! Pipes used to synchronize the parent and setup child during startup.
//!
//! Parent -> child: cgroup ready. Child -> parent: user namespace ready.
//! Parent -> child: UID/GID mapping ready. Child -> parent: network namespace ready.
//! Parent -> child: network ready. Child -> parent: container created.
//!
//! The pipes are close-on-exec, so neither pasta nor the container program receives them.

mod child_channels;
mod parent_channels;
mod startup_channels;

pub(crate) use child_channels::ChildChannels;
use parent_channels::ParentChannels;
pub(crate) use startup_channels::StartupChannels;

use anyhow::{Context, Result, bail, ensure};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd::{read, write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

const READY: u8 = 1;

pub(crate) fn send_ready(fd: &OwnedFd, stage: &str) -> Result<()> {
    loop {
        match write(fd, &[READY]) {
            Ok(1) => return Ok(()),
            Ok(count) => bail!("sent {count} bytes for {stage}; expected one"),
            // Retry if an OS signal interrupts the write() systemcall, before writing something
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error).with_context(|| format!("signal {stage} ready")),
        }
    }
}

pub(crate) fn wait_ready(fd: &OwnedFd, stage: &str) -> Result<()> {
    ensure!(
        read_ready(fd, stage)?,
        "peer closed {stage} pipe before signaling"
    );
    Ok(())
}

/// Like `read_ready`, but return false at once if the peer has not signaled yet.
pub(crate) fn has_signaled(fd: &OwnedFd, stage: &str) -> Result<bool> {
    let mut polled = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    loop {
        match poll(&mut polled, PollTimeout::ZERO) {
            Err(Errno::EINTR) => continue,
            result => result.with_context(|| format!("check {stage}"))?,
        };
        break;
    }
    // A closed write end also shows as readable, and read_ready then returns false.
    if polled[0].revents().is_some_and(|events| !events.is_empty()) {
        read_ready(fd, stage)
    } else {
        Ok(false)
    }
}

/// Like `wait_ready`, but return false if the peer closed the pipe before it signaled.
pub(crate) fn read_ready(fd: &OwnedFd, stage: &str) -> Result<bool> {
    let mut byte = [0u8; 1];
    loop {
        match read(fd.as_raw_fd(), &mut byte) {
            Ok(1) if byte[0] == READY => return Ok(true),
            Ok(1) => bail!("unexpected {stage} signal: {}", byte[0]),
            Ok(0) => return Ok(false),
            Ok(count) => bail!("read {count} bytes for {stage}; expected one"),
            // Retry if an OS signal interrupts the read() systemcall, before reading something
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error).with_context(|| format!("wait for {stage}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::unistd::pipe;

    #[test]
    fn writer_closed_pipe_first_before_sending_something() {
        let (reader, writer) = pipe().unwrap();
        drop(writer);

        let error = wait_ready(&reader, "test").unwrap_err();
        assert!(error.to_string().contains("closed test pipe"));
    }

    #[test]
    fn wait_ready_receives_signal() {
        let (reader, writer) = pipe().unwrap();
        write(&writer, &[1]).unwrap();
        drop(writer);

        wait_ready(&reader, "test").unwrap();
    }

    #[test]
    fn checks_for_a_signal_without_waiting() {
        let (reader, writer) = pipe().unwrap();
        assert!(!has_signaled(&reader, "test").unwrap());

        write(&writer, &[1]).unwrap();
        assert!(has_signaled(&reader, "test").unwrap());

        drop(writer);
        assert!(!has_signaled(&reader, "test").unwrap());
    }

    #[test]
    fn wait_ready_rejects_unexpected_signal() {
        let (reader, writer) = pipe().unwrap();
        write(&writer, &[2]).unwrap();
        drop(writer);

        let error = wait_ready(&reader, "test").unwrap_err();
        assert!(error.to_string().contains("unexpected test signal"));
    }
}
