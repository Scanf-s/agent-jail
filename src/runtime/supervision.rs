//! Wait for runtime processes and handle container startup, signals, and exit.

use super::Mode;
use super::start::{StartRequest, StartSocket};
use super::startup::{self, Report};
use crate::ipc;
use crate::signals::{self, Event, RuntimeSignals, TERMINATION_SIGNALS};
use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
use nix::unistd::Pid;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// The result of waiting for the setup process.
pub(super) enum Outcome {
    /// `create` created the container.
    Created,
    /// The setup process ended with this exit code.
    Exited(u8),
}

/// Wait until the setup process `child` reports on `created` that the container is created,
/// in `Create` mode, or until it ends. `on_created` runs when the setup process reports. It
/// receives the termination signal that cancelled `aj create`, if one did.
///
/// The termination signals that usually reach only the runtime, for example from `kill` or a
/// process manager, go to the setup process. In `Create` mode, such a signal cancels the
/// create: this waits until the setup process ends, and the container does not stay.
pub(super) fn watch_setup_process(
    child: Pid,
    signals: &RuntimeSignals,
    created: OwnedFd,
    mode: Mode,
    mut on_created: impl FnMut(Option<Signal>),
) -> Result<Outcome> {
    let forwarded: &[Signal] = match mode {
        // A terminal sends SIGINT and SIGQUIT to the whole foreground process group, so the
        // setup process and the container already receive them.
        Mode::Run => &[Signal::SIGTERM, Signal::SIGHUP],
        // The setup process has a session of its own.
        Mode::Create => &TERMINATION_SIGNALS,
    };
    let mut created = Some(created);
    let mut cancelled = None;
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
                    on_created(cancelled);
                    if mode == Mode::Create && cancelled.is_none() {
                        return Ok(Outcome::Created);
                    }
                }
                continue;
            }
            // Returning early would remove the run directory while the container still uses it.
            Err(error) => {
                report!("aj: warning: {error:#}");
                let status = waitpid(child, None).context("waitpid failed")?;
                return Ok(Outcome::Exited(exit_status(status)?));
            }
        };
        match signal {
            Signal::SIGCHLD => {
                let status =
                    match waitpid(child, Some(WaitPidFlag::WNOHANG)).context("waitpid failed")? {
                        WaitStatus::StillAlive => continue,
                        status => status,
                    };
                // The setup process can report and end before this reads the report, for
                // example when `aj start` runs a short program at once. The created container
                // stays until it is deleted, as OCI requires.
                if mode == Mode::Create
                    && cancelled.is_none()
                    && let Some(pipe) = created.take()
                    && ipc::has_signaled(&pipe, "created")?
                {
                    return Ok(Outcome::Created);
                }
                let code = match cancelled {
                    Some(signal) => 128 + signal as u8,
                    None => exit_status(status)?,
                };
                return Ok(Outcome::Exited(code));
            }
            signal if forwarded.contains(&signal) => {
                let _ = kill(child, signal);
                if mode == Mode::Create {
                    cancelled.get_or_insert(signal);
                }
            }
            _ => {}
        }
    }
}

/// What container init did before its program ran.
pub(super) enum Startup {
    /// Init is ready to execute the program.
    Created,
    /// Init executed the program.
    Running,
    /// Init cannot run the program, for this reason.
    Failed(String),
}

/// Wait until container init ends, and return the exit code for the runtime.
///
/// Report startup changes through `on_startup`. After it accepts `Startup::Created`, start
/// the program immediately or wait for `aj start` on `socket`. If the process watched by
/// `runtime` ends, kill init.
///
/// If waiting fails, this kills and reaps init, so that the caller can remove the run directory.
pub(super) fn supervise(
    init: Pid,
    signals: &RuntimeSignals,
    channel: UnixStream,
    socket: Option<StartSocket>,
    runtime: Option<BorrowedFd>,
    on_startup: impl FnMut(Startup) -> Result<()>,
) -> Result<u8> {
    let supervisor = InitSupervisor {
        init,
        channel: Some(channel),
        start_at_once: socket.is_none(),
        socket,
        runtime,
        ready_to_start: false,
        start_sent: false,
        pending_start: None,
        termination_signal: None,
    };
    let result = supervisor.wait(signals, on_startup);
    if result.is_err() {
        let _ = kill(init, Signal::SIGKILL);
        let _ = waitpid(init, None);
    }
    result
}

#[derive(Clone, Copy)]
enum Wakeup {
    Signal(Signal),
    Startup,
    StartRequest,
    RuntimeExit,
}

struct InitSupervisor<'a> {
    init: Pid,
    channel: Option<UnixStream>,
    socket: Option<StartSocket>,
    runtime: Option<BorrowedFd<'a>>,
    start_at_once: bool,
    ready_to_start: bool,
    start_sent: bool,
    pending_start: Option<StartRequest>,
    termination_signal: Option<Signal>,
}

impl InitSupervisor<'_> {
    fn wait(
        mut self,
        signals: &RuntimeSignals,
        mut on_startup: impl FnMut(Startup) -> Result<()>,
    ) -> Result<u8> {
        loop {
            match self.next_event(signals)? {
                Wakeup::Startup => self.read_startup(&mut on_startup)?,
                Wakeup::StartRequest => self.accept_start()?,
                Wakeup::RuntimeExit => {
                    // The container of `aj run` must end with its runtime.
                    self.runtime = None;
                    let _ = kill(self.init, Signal::SIGKILL);
                }
                Wakeup::Signal(Signal::SIGCHLD) => {
                    if has_ended(self.init)? {
                        self.drain_startup(&mut on_startup)?;
                        let status = waitpid(self.init, None).context("waitpid(init) failed")?;
                        return exit_code_with_termination_signal(status, self.termination_signal);
                    }
                }
                Wakeup::Signal(signal) => self.forward_signal(signal),
            }
        }
    }

    fn next_event(&self, signals: &RuntimeSignals) -> Result<Wakeup> {
        let mut sources = Vec::new();
        let mut fds = Vec::new();
        // Read startup reports before accepting a start request.
        if let Some(channel) = &self.channel {
            sources.push(Wakeup::Startup);
            fds.push(channel.as_fd());
        }
        // An early request waits in the socket until init is ready.
        if self.ready_to_start
            && let Some(socket) = &self.socket
        {
            sources.push(Wakeup::StartRequest);
            fds.push(socket.as_fd());
        }
        if let Some(runtime) = self.runtime {
            sources.push(Wakeup::RuntimeExit);
            fds.push(runtime);
        }
        // Signals must still arrive while init is starting, so Ctrl-C can end it.
        let event = if fds.is_empty() {
            Event::Signal(signals.wait()?)
        } else {
            signals.wait_or_readable(&fds)?
        };
        Ok(match event {
            Event::Signal(signal) => Wakeup::Signal(signal),
            Event::Readable(index) => sources[index],
        })
    }

    fn read_startup(&mut self, on_startup: &mut impl FnMut(Startup) -> Result<()>) -> Result<()> {
        let Some(channel) = &self.channel else {
            return Ok(());
        };
        match startup::read_report(channel)? {
            Report::Ready => {
                on_startup(Startup::Created)?;
                self.ready_to_start = true;
                if self.start_at_once {
                    startup::start(channel)?;
                    self.start_sent = true;
                }
                Ok(())
            }
            report => self.finish_startup(report, on_startup),
        }
    }

    fn accept_start(&mut self) -> Result<()> {
        let Some(socket) = &self.socket else {
            return Ok(());
        };
        let pending = match socket.accept() {
            Ok(Some(pending)) => pending,
            Ok(None) => return Ok(()),
            // A failed connection does not change the container.
            Err(error) => {
                report!("aj: warning: {error:#}");
                return Ok(());
            }
        };
        // If init has ended, dropping the request tells the client that startup failed.
        let Some(channel) = &self.channel else {
            return Ok(());
        };
        startup::start(channel)?;
        self.start_sent = true;
        self.pending_start = Some(pending);
        if let Some(socket) = self.socket.take() {
            socket.close();
        }
        Ok(())
    }

    fn finish_startup(
        &mut self,
        report: Report,
        on_startup: &mut impl FnMut(Startup) -> Result<()>,
    ) -> Result<()> {
        self.channel = None;
        conclude(
            conclusion(report, self.start_sent),
            on_startup,
            self.pending_start.take(),
        )
    }

    fn drain_startup(&mut self, on_startup: &mut impl FnMut(Startup) -> Result<()>) -> Result<()> {
        // Init may end before its last report is read. Its closed channel cannot block here.
        if let Some(channel) = self.channel.take() {
            loop {
                match startup::read_report(&channel)? {
                    // The program cannot start after init has ended.
                    Report::Ready => {}
                    report => return self.finish_startup(report, on_startup),
                }
            }
        }
        Ok(())
    }

    fn forward_signal(&mut self, signal: Signal) {
        let default_action = match signals::uses_default_action(self.init, signal) {
            Ok(default_action) => default_action,
            // If the check fails, let the program handle the signal.
            Err(error) => {
                report!("aj: warning: pass {signal} on to the container: {error:#}");
                false
            }
        };
        if default_action {
            // PID 1 ignores these default actions, so the runtime ends it instead.
            self.termination_signal.get_or_insert(signal);
            let _ = kill(self.init, Signal::SIGKILL);
        } else if matches!(signal, Signal::SIGTERM | Signal::SIGHUP) {
            // The terminal does not send these to init, so the runtime passes them on.
            let _ = kill(self.init, signal);
        }
    }
}

/// Report the startup result and answer any pending `aj start` request.
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

/// Interpret the final report using whether init was told to start.
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
    use crate::container_id::ContainerId;
    use crate::state::{Record, StateDir};
    use nix::libc;
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::sys::signal::{SigSet, raise};
    use nix::unistd::{ForkResult, fork, pipe};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::PathBuf;
    use std::time::Duration;

    fn supervisor(channel: UnixStream, socket: Option<StartSocket>) -> InitSupervisor<'static> {
        InitSupervisor {
            init: Pid::this(),
            channel: Some(channel),
            start_at_once: socket.is_none(),
            socket,
            runtime: None,
            ready_to_start: false,
            start_sent: false,
            pending_start: None,
            termination_signal: None,
        }
    }

    fn assert_no_data(stream: &mut UnixStream) {
        stream.set_nonblocking(true).unwrap();
        let error = stream.read(&mut [0]).unwrap_err();
        stream.set_nonblocking(false).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn run_records_created_state_before_starting_the_program() {
        for record_fails in [false, true] {
            let (channel, mut init) = startup::channel().unwrap();
            let mut supervisor = supervisor(channel, None);
            init.write_all(b"R").unwrap();

            let result = supervisor.read_startup(&mut |event| {
                assert!(matches!(event, Startup::Created));
                assert_no_data(&mut init);
                ensure_recorded(record_fails)
            });

            assert_eq!(result.is_err(), record_fails);
            if record_fails {
                assert_no_data(&mut init);
            } else {
                init.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut message = [0];
                init.read_exact(&mut message).unwrap();
                assert_eq!(message, *b"G");
            }
        }
    }

    fn ensure_recorded(fails: bool) -> Result<()> {
        if fails {
            bail!("cannot record state");
        }
        Ok(())
    }

    #[test]
    fn create_acknowledges_start_only_after_recording_running_state() {
        for record_fails in [false, true] {
            let root =
                std::env::temp_dir().join(format!("aj-supervision-{:x}", rand::random::<u64>()));
            let id = ContainerId::parse("box").unwrap();
            let record = Record::creating(
                &id,
                PathBuf::from("/no/cgroup"),
                PathBuf::from("/bundle"),
                None,
            );
            let state_dir = StateDir::create(&root, &id, record).unwrap();
            let socket = StartSocket::listen(&state_dir).unwrap();
            let mut client = UnixStream::connect(state_dir.path().join("start.sock")).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            client.write_all(b"start\n").unwrap();
            let (channel, mut init) = startup::channel().unwrap();
            let mut supervisor = supervisor(channel, Some(socket));
            init.write_all(b"R").unwrap();

            supervisor
                .read_startup(&mut |event| {
                    assert!(matches!(event, Startup::Created));
                    Ok(())
                })
                .unwrap();
            assert_no_data(&mut init);
            supervisor.accept_start().unwrap();
            init.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut message = [0];
            init.read_exact(&mut message).unwrap();
            assert_eq!(message, *b"G");
            assert_no_data(&mut client);

            // A successful exec closes init's end of the startup channel.
            drop(init);
            let result = supervisor.read_startup(&mut |event| {
                assert!(matches!(event, Startup::Running));
                assert_no_data(&mut client);
                ensure_recorded(record_fails)
            });

            assert_eq!(result.is_err(), record_fails);
            let mut answer = String::new();
            client.read_to_string(&mut answer).unwrap();
            let expected = if record_fails {
                "error cannot record state\n"
            } else {
                "ok\n"
            };
            assert_eq!(answer, expected);
            drop(supervisor);
            drop(state_dir);
            fs::remove_dir(root).unwrap();
        }
    }

    /// Fork a setup process that reports that the container is created and then runs `then`.
    /// Returns its PID and the pipe of the report.
    fn report_created(then: fn()) -> (Pid, OwnedFd) {
        let (reader, writer) = pipe().unwrap();
        // SAFETY: the test process has other threads, so the child makes only
        // async-signal-safe calls.
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                let _ = ipc::send_ready(&writer, "created");
                then();
                unsafe { libc::_exit(0) }
            }
            ForkResult::Parent { child } => (child, reader),
        }
    }

    /// Wait until `child` ends, and then raise SIGCHLD in this thread. The kernel sends SIGCHLD
    /// to the process, so another thread of the tests can take it.
    fn report_end(child: Pid) {
        waitid(Id::Pid(child), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT).unwrap();
        raise(Signal::SIGCHLD).unwrap();
    }

    #[test]
    fn keeps_a_container_whose_setup_process_ended_before_the_report_was_read() {
        let signals = RuntimeSignals::block().unwrap();
        let (child, created) = report_created(|| {});
        report_end(child);

        let outcome = watch_setup_process(child, &signals, created, Mode::Create, |_| {});

        assert!(matches!(outcome.unwrap(), Outcome::Created));
    }

    #[test]
    fn a_termination_signal_cancels_create() {
        let signals = RuntimeSignals::block().unwrap();
        // The setup process inherits the blocked signals, so it waits for the forwarded one.
        let (child, created) = report_created(|| {
            let _ = SigSet::from(Signal::SIGINT).wait();
        });
        // Ctrl-C comes right after the report, before the runtime reads it.
        raise(Signal::SIGINT).unwrap();
        let mut report = [PollFd::new(created.as_fd(), PollFlags::POLLIN)];
        poll(&mut report, PollTimeout::NONE).unwrap();

        let outcome = watch_setup_process(child, &signals, created, Mode::Create, |_| {
            report_end(child)
        });

        assert!(matches!(outcome.unwrap(), Outcome::Exited(130)));
    }

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
