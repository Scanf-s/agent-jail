//! The socket over which container init reports to the setup process before its program runs.
//!
//! Init writes `E` and an error message if it cannot run the program. Its end of the socket is
//! close-on-exec, so when the program runs, the setup process reads EOF instead.

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

const FAILED: u8 = b'E';

/// What container init reported.
#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    /// Init closed its end: it executed the program, or it ended without a report.
    Closed,
    /// Init cannot run the program, for this reason.
    Failed(String),
}

/// Create the socket. The setup process keeps the first end, and init keeps the second.
pub fn channel() -> Result<(UnixStream, UnixStream)> {
    // Both ends are close-on-exec.
    UnixStream::pair().context("create the startup channel")
}

/// Send `error` as the reason why init cannot run the program.
pub fn send_failure(channel: &mut UnixStream, error: &anyhow::Error) -> std::io::Result<()> {
    let mut message = vec![FAILED];
    message.extend_from_slice(format!("{error:#}").as_bytes());
    channel.write_all(&message)
}

/// Read the next report from `channel`, which is readable.
pub fn read_report(channel: &mut UnixStream) -> Result<Report> {
    let mut kind = [0];
    if channel
        .read(&mut kind)
        .context("read the startup channel")?
        == 0
    {
        return Ok(Report::Closed);
    }
    match kind[0] {
        // Init ends after this report, so the message ends at EOF.
        FAILED => {
            let mut message = Vec::new();
            channel
                .read_to_end(&mut message)
                .context("read the startup channel")?;
            Ok(Report::Failed(
                String::from_utf8_lossy(&message).into_owned(),
            ))
        }
        other => bail!("unexpected report {other} on the startup channel"),
    }
}

/// Whether the other end of `channel` is closed.
pub fn peer_closed(channel: &UnixStream) -> Result<bool> {
    let mut fds = [PollFd::new(channel.as_fd(), PollFlags::empty())];
    poll(&mut fds, PollTimeout::ZERO).context("poll the startup channel")?;
    Ok(fds[0]
        .revents()
        .is_some_and(|events| events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn reads_a_failure_and_its_message() {
        let (mut setup, mut init) = channel().unwrap();
        send_failure(&mut init, &anyhow!("no program").context("exec")).unwrap();
        drop(init);

        assert_eq!(
            read_report(&mut setup).unwrap(),
            Report::Failed("exec: no program".to_owned())
        );
    }

    #[test]
    fn reads_a_closed_end() {
        let (mut setup, init) = channel().unwrap();
        assert!(!peer_closed(&setup).unwrap());

        drop(init);

        assert!(peer_closed(&setup).unwrap());
        assert_eq!(read_report(&mut setup).unwrap(), Report::Closed);
    }

    #[test]
    fn rejects_an_unknown_report() {
        let (mut setup, mut init) = channel().unwrap();
        init.write_all(b"?").unwrap();

        assert!(read_report(&mut setup).is_err());
    }
}
