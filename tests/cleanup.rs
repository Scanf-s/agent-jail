//! These tests start real containers, so they need the requirements in the README and a
//! rootfs. They are ignored by default. Run them with `cargo test -- --ignored`.
//!
//! The rootfs is `$AJ_TEST_ROOTFS`, or `~/.local/share/agent-jail/images/default` without it.
//! Each test uses its own `XDG_DATA_HOME`, so it never touches the user's run directories.

use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::unistd::Pid;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::thread::sleep;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);
// Creates a file that belongs to a subordinate ID, which the host user cannot remove.
const CREATE_SUBORDINATE_FILE: &str =
    "mkdir -p /tmp/x && chown 1000:1000 /tmp/x && touch /tmp/x/f && touch /out/ready";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("aj-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("out")).unwrap();
        Self { root }
    }

    /// Start `aj run` with `script` in its own process group, as a shell does for a job.
    fn spawn(&self, script: &str) -> Child {
        let rootfs = std::env::var_os("AJ_TEST_ROOTFS").map_or_else(
            || {
                PathBuf::from(std::env::var_os("HOME").unwrap())
                    .join(".local/share/agent-jail/images/default")
            },
            PathBuf::from,
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_aj"));
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .arg("run")
            .arg("--net")
            .arg("none")
            .arg("--rootfs")
            .arg(rootfs)
            .arg("-v")
            .arg(format!("{}:/out", self.root.join("out").display()))
            .args(["--", "/bin/sh", "-c", script])
            .process_group(0);
        // A shell without job control starts background jobs with SIGINT and SIGQUIT ignored.
        // The tests need the default actions that a terminal job has.
        // SAFETY: signal() is async-signal-safe, and SigDfl installs no handler.
        unsafe {
            command.pre_exec(|| {
                for default in [Signal::SIGINT, Signal::SIGQUIT] {
                    signal(default, SigHandler::SigDfl)?;
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.root.join("out/ready").exists() {
            assert!(Instant::now() < deadline, "the container did not start");
            sleep(Duration::from_millis(50));
        }
    }

    fn run_dirs(&self) -> usize {
        fs::read_dir(self.root.join("data/agent-jail/runs")).map_or(0, |entries| entries.count())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("aj did not exit");
        }
        sleep(Duration::from_millis(50));
    }
}

fn signal_group(child: &Child, signal: Signal) {
    kill(Pid::from_raw(-(child.id() as i32)), signal).unwrap();
}

#[test]
#[ignore = "starts a container"]
fn ctrl_c_ends_a_pid_1_program_and_removes_the_run_directory() {
    let sandbox = Sandbox::new("ctrl-c");
    let mut aj = sandbox.spawn(&format!("{CREATE_SUBORDINATE_FILE} && exec sleep 60"));
    sandbox.wait_until_ready();

    signal_group(&aj, Signal::SIGINT);

    assert_eq!(wait(&mut aj).code(), Some(130));
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn ctrl_c_reaches_a_program_that_handles_it() {
    let sandbox = Sandbox::new("trap");
    let mut aj = sandbox.spawn(
        "trap 'touch /out/caught' INT; touch /out/ready; \
         while [ ! -e /out/stop ]; do sleep 0.1; done",
    );
    sandbox.wait_until_ready();

    signal_group(&aj, Signal::SIGINT);
    let deadline = Instant::now() + TIMEOUT;
    while !sandbox.root.join("out/caught").exists() {
        assert!(
            Instant::now() < deadline,
            "the program did not receive SIGINT"
        );
        sleep(Duration::from_millis(50));
    }
    assert!(
        aj.try_wait().unwrap().is_none(),
        "Ctrl-C ended a program that handles it"
    );
    fs::write(sandbox.root.join("out/stop"), "").unwrap();

    assert_eq!(wait(&mut aj).code(), Some(0));
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn sigterm_to_the_runtime_ends_the_container() {
    let sandbox = Sandbox::new("sigterm");
    let mut aj = sandbox.spawn("touch /out/ready && exec sleep 60");
    sandbox.wait_until_ready();

    kill(Pid::from_raw(aj.id() as i32), Signal::SIGTERM).unwrap();

    assert_eq!(wait(&mut aj).code(), Some(143));
    assert_eq!(sandbox.run_dirs(), 0);
}
