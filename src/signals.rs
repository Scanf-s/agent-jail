use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigHandler, SigSet, SigmaskHow, Signal, signal, sigprocmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::unistd::Pid;
use std::fs;
use std::os::fd::{AsFd, BorrowedFd};

/// Signals that a terminal, a process manager, or a user sends to end a program.
const TERMINATION_SIGNALS: [Signal; 4] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTERM,
];

/// The termination signals and SIGCHLD, which the runtime processes receive with `wait` instead of
/// letting the default action end them.
///
/// The setup process must outlive container init, because it is the only process that can
/// remove the run directory. With the default actions, Ctrl-C would end it first.
pub struct RuntimeSignals {
    set: SigSet,
}

impl RuntimeSignals {
    /// Block the signals in this process. Call this before fork, so that the child process
    /// starts with the signals already blocked.
    pub fn block() -> Result<Self> {
        let mut set = SigSet::empty();
        for signal in TERMINATION_SIGNALS {
            set.add(signal);
        }
        set.add(Signal::SIGCHLD);
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(&set), None).context("block signals")?;
        Ok(Self { set })
    }

    /// Wait until one of the blocked signals arrives.
    pub fn wait(&self) -> Result<Signal> {
        self.set.wait().context("wait for a signal")
    }

    /// Wait until one of the blocked signals arrives, or `fd` has data or reaches its end.
    pub fn wait_or_readable(&self, fd: BorrowedFd) -> Result<Event> {
        // A signalfd also reports the signals that were pending before it was created.
        let signal_fd =
            SignalFd::with_flags(&self.set, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)
                .context("create a signalfd")?;
        loop {
            let mut fds = [
                PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN),
                PollFd::new(fd, PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Err(Errno::EINTR) => continue,
                result => result.context("poll")?,
            };
            // A closed write end shows as POLLHUP instead of POLLIN.
            let [signal_ready, fd_ready] =
                fds.map(|fd| fd.revents().is_some_and(|events| !events.is_empty()));
            if signal_ready && let Some(info) = signal_fd.read_signal().context("read a signal")? {
                let signal = Signal::try_from(info.ssi_signo as i32).context("read a signal")?;
                return Ok(Event::Signal(signal));
            }
            if fd_ready {
                return Ok(Event::Readable);
            }
        }
    }
}

/// What `RuntimeSignals::wait_or_readable` waited for.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Signal(Signal),
    /// The file has data, or its write end was closed.
    Readable,
}

/// Give the program the signal state that a new process normally has.
///
/// The blocked signals and SIGPIPE, which the Rust runtime ignores, would otherwise stay so
/// after exec. With SIGPIPE ignored, a program such as `yes | head -1` reports a write error
/// instead of ending quietly.
pub fn reset_for_exec() -> Result<()> {
    // SAFETY: SigDfl installs no handler, so no handler code can run at an unsafe time.
    unsafe { signal(Signal::SIGPIPE, SigHandler::SigDfl) }.context("restore SIGPIPE")?;
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None).context("unblock signals")
}

/// Whether `pid` neither catches nor ignores `signal`.
///
/// The kernel does not deliver such a signal to PID 1 of a PID namespace, so a container
/// program that runs as PID 1 does not end on Ctrl-C by itself.
pub fn uses_default_action(pid: Pid, signal: Signal) -> Result<bool> {
    let path = format!("/proc/{pid}/status");
    let status = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    uses_default_action_in(&status, signal)
}

fn uses_default_action_in(status: &str, signal: Signal) -> Result<bool> {
    let bit = 1u64 << (signal as u32 - 1);
    for field in ["SigIgn", "SigCgt"] {
        if signal_mask(status, field)? & bit != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn signal_mask(status: &str, field: &str) -> Result<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .with_context(|| format!("no {field} line in the process status"))?;
    u64::from_str_radix(value.trim(), 16).with_context(|| format!("parse {field} {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(ignored: &str, caught: &str) -> String {
        format!("Name:\tsh\nSigBlk:\t0000000000000000\nSigIgn:\t{ignored}\nSigCgt:\t{caught}\n")
    }

    #[test]
    fn finds_signals_that_the_process_leaves_at_the_default_action() {
        // Bit 12 is SIGPIPE, and bit 1 is SIGINT.
        let ignores_pipe = status("0000000000001000", "0000000000000000");
        assert!(uses_default_action_in(&ignores_pipe, Signal::SIGINT).unwrap());
        assert!(!uses_default_action_in(&ignores_pipe, Signal::SIGPIPE).unwrap());

        let catches_int = status("0000000000000000", "0000000000000002");
        assert!(!uses_default_action_in(&catches_int, Signal::SIGINT).unwrap());
        assert!(uses_default_action_in(&catches_int, Signal::SIGTERM).unwrap());
    }

    // The signal mask belongs to a thread, so blocking signals here does not change other tests.
    #[test]
    fn waits_for_a_closed_write_end_or_a_signal() {
        let signals = RuntimeSignals::block().unwrap();
        let (reader, writer) = nix::unistd::pipe().unwrap();

        // raise sends the signal to this thread, which blocks it, so it stays pending.
        nix::sys::signal::raise(Signal::SIGTERM).unwrap();
        assert_eq!(
            signals.wait_or_readable(reader.as_fd()).unwrap(),
            Event::Signal(Signal::SIGTERM)
        );

        drop(writer);
        assert_eq!(
            signals.wait_or_readable(reader.as_fd()).unwrap(),
            Event::Readable
        );
    }

    #[test]
    fn rejects_a_status_without_signal_masks() {
        assert!(uses_default_action_in("Name:\tsh\n", Signal::SIGINT).is_err());
        let malformed = status("zz", "0000000000000000");
        assert!(uses_default_action_in(&malformed, Signal::SIGINT).is_err());
    }
}
