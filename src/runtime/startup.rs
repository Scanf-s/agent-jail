//! The socket between the setup process and container init before the program runs.
//!
//! When the container is ready, init writes `R` and waits. The setup process records that the
//! container is created and writes `G`, and init executes the program. If init cannot run the
//! program, it writes `E` and an error message instead. Its end of the socket is close-on-exec,
//! so when the program runs, the setup process reads EOF.

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const READY: u8 = b'R';
const START: u8 = b'G';
const FAILED: u8 = b'E';

/// What container init reported.
#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    /// Init is ready to execute the program, and waits until the setup process starts it.
    Ready,
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

/// Report from init that the container is ready, and wait until the setup process starts the
/// program.
pub fn wait_for_start(mut channel: &UnixStream) -> Result<()> {
    channel
        .write_all(&[READY])
        .context("report that the container is ready")?;
    let mut message = [0];
    let count = channel
        .read(&mut message)
        .context("wait for the start of the program")?;
    ensure!(count == 1, "the setup process has ended");
    ensure!(
        message[0] == START,
        "unexpected message {} on the startup channel",
        message[0]
    );
    Ok(())
}

/// Tell init to execute the program.
pub fn start(mut channel: &UnixStream) -> Result<()> {
    channel
        .write_all(&[START])
        .context("start the container program")
}

/// Send `error` as the reason why init cannot run the program.
pub fn send_failure(mut channel: &UnixStream, error: &anyhow::Error) -> std::io::Result<()> {
    let mut message = vec![FAILED];
    message.extend_from_slice(format!("{error:#}").as_bytes());
    channel.write_all(&message)
}

/// Read the next report from `channel`, which is readable.
pub fn read_report(mut channel: &UnixStream) -> Result<Report> {
    let mut kind = [0];
    if channel
        .read(&mut kind)
        .context("read the startup channel")?
        == 0
    {
        return Ok(Report::Closed);
    }
    match kind[0] {
        READY => Ok(Report::Ready),
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::thread;

    #[test]
    fn starts_init_after_it_is_ready() {
        let (setup, init) = channel().unwrap();
        let init = thread::spawn(move || wait_for_start(&init));

        assert_eq!(read_report(&setup).unwrap(), Report::Ready);
        start(&setup).unwrap();

        init.join().unwrap().unwrap();
    }

    #[test]
    fn init_fails_when_the_setup_process_ends() {
        let (setup, init) = channel().unwrap();
        let init = thread::spawn(move || wait_for_start(&init));

        assert_eq!(read_report(&setup).unwrap(), Report::Ready);
        drop(setup);

        let error = init.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("has ended"), "{error:#}");
    }

    #[test]
    fn reads_a_failure_and_its_message() {
        let (setup, init) = channel().unwrap();
        send_failure(&init, &anyhow!("no program").context("exec")).unwrap();
        drop(init);

        assert_eq!(
            read_report(&setup).unwrap(),
            Report::Failed("exec: no program".to_owned())
        );
    }

    #[test]
    fn reads_a_closed_end() {
        let (setup, init) = channel().unwrap();
        drop(init);

        assert_eq!(read_report(&setup).unwrap(), Report::Closed);
    }

    #[test]
    fn rejects_an_unknown_report() {
        let (setup, mut init) = channel().unwrap();
        init.write_all(b"?").unwrap();

        assert!(read_report(&setup).is_err());
    }
}
