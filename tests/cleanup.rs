//! These tests start real containers, so they need the requirements in the README and a
//! rootfs. They are ignored by default. Run them with `cargo test -- --ignored`.
//!
//! The rootfs is `$AJ_TEST_ROOTFS`, or `~/.local/share/agent-jail/images/default` without it.
//! Each test uses its own `XDG_DATA_HOME` and `--root`, so it never touches the user's run and
//! state directories, and tests that run at the same time do not clean up each other's.

use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::unistd::Pid;
use serde_json::Value;
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
        self.spawn_with(&[], &["/bin/sh", "-c", script])
    }

    /// Start `aj run` with `program` and its arguments, as `spawn` does.
    fn spawn_program(&self, program: &[&str]) -> Child {
        self.spawn_with(&[], program)
    }

    /// Start `aj run` with the options `run_args`, `program`, and its arguments.
    fn spawn_with(&self, run_args: &[&str], program: &[&str]) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aj"));
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .arg("--root")
            .arg(self.root.join("state"))
            .arg("run")
            .args(run_args)
            .arg("--net")
            .arg("none")
            .arg("--rootfs")
            .arg(rootfs())
            .arg("-v")
            .arg(format!("{}:/out", self.root.join("out").display()))
            .arg("--")
            .args(program)
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

    /// The content of each state file under `--root`.
    fn states(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.root.join("state")) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let dir = entry.unwrap().path();
                let state: Value =
                    serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
                assert_eq!(state["id"], dir.file_name().unwrap().to_str().unwrap());
                state
            })
            .collect()
    }
}

fn rootfs() -> PathBuf {
    std::env::var_os("AJ_TEST_ROOTFS").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local/share/agent-jail/images/default")
        },
        PathBuf::from,
    )
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

/// The only child process of `pid`.
fn only_child(pid: Pid) -> Pid {
    let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let children: Vec<&str> = children.split_whitespace().collect();
    assert_eq!(children.len(), 1, "process {pid} has children {children:?}");
    Pid::from_raw(children[0].parse().unwrap())
}

/// Whether `pid` runs. A zombie has ended and only waits for its parent to reap it.
fn is_running(pid: Pid) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state follows the command name, which is in parentheses and may contain spaces.
    let state = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .next();
    !matches!(state, Some("Z" | "X"))
}

#[test]
#[ignore = "starts a container"]
fn records_the_state_while_the_container_runs() {
    let sandbox = Sandbox::new("state");
    let mut aj = sandbox.spawn("touch /out/ready && while [ ! -e /out/stop ]; do sleep 0.1; done");
    sandbox.wait_until_ready();

    let init = only_child(only_child(Pid::from_raw(aj.id() as i32)));

    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["ociVersion"], "1.3.0");
    assert_eq!(states[0]["status"], "running");
    assert_eq!(states[0]["pid"], init.as_raw());
    assert!(states[0]["initStartTime"].as_u64().unwrap() > 0);
    assert_eq!(states[0]["bundle"], rootfs().to_str().unwrap());
    assert_eq!(states[0]["autoDelete"], true);

    fs::write(sandbox.root.join("out/stop"), "").unwrap();
    assert_eq!(wait(&mut aj).code(), Some(0));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
}

#[test]
#[ignore = "starts a container"]
fn rejects_an_id_that_a_container_uses() {
    let sandbox = Sandbox::new("same-id");
    let wait_for_stop = "touch /out/ready && while [ ! -e /out/stop ]; do sleep 0.1; done";
    let mut first = sandbox.spawn_with(&["--id", "box"], &["/bin/sh", "-c", wait_for_stop]);
    sandbox.wait_until_ready();

    let mut second = sandbox.spawn_with(&["--id", "box"], &["/bin/true"]);

    assert_eq!(wait(&mut second).code(), Some(1));
    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["status"], "running");
    fs::write(sandbox.root.join("out/stop"), "").unwrap();
    assert_eq!(wait(&mut first).code(), Some(0));
}

#[test]
#[ignore = "starts a container"]
fn accepts_an_id_that_looks_like_a_unit_name() {
    let sandbox = Sandbox::new("unit-id");
    let mut aj = sandbox.spawn_with(
        &["--id", "web.service"],
        &[
            "/bin/sh",
            "-c",
            "touch /out/ready && while [ ! -e /out/stop ]; do sleep 0.1; done",
        ],
    );
    sandbox.wait_until_ready();

    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["id"], "web.service");
    fs::write(sandbox.root.join("out/stop"), "").unwrap();
    assert_eq!(wait(&mut aj).code(), Some(0));
}

#[test]
#[ignore = "starts a container"]
fn reports_a_program_that_cannot_run() {
    let sandbox = Sandbox::new("no-program");
    let mut aj = sandbox.spawn_program(&["/no/such/program"]);

    assert_eq!(wait(&mut aj).code(), Some(127));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
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

#[test]
#[ignore = "starts a container"]
fn container_init_ends_when_only_the_setup_process_is_killed() {
    let sandbox = Sandbox::new("setup-killed");
    let mut aj = sandbox.spawn("touch /out/ready && exec sleep 60");
    sandbox.wait_until_ready();
    let setup = only_child(Pid::from_raw(aj.id() as i32));
    let init = only_child(setup);

    kill(setup, Signal::SIGKILL).unwrap();

    assert_eq!(wait(&mut aj).code(), Some(137));
    assert!(!is_running(init), "container init outlived the runtime");
    // Only a process in the user namespace can remove the overlay's work directory, so the
    // next run removes the run directory.
    let mut next = sandbox.spawn("true");
    assert_eq!(wait(&mut next).code(), Some(0));
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn the_next_run_removes_what_a_killed_run_left() {
    let sandbox = Sandbox::new("sigkill");
    let mut killed = sandbox.spawn(&format!("{CREATE_SUBORDINATE_FILE} && exec sleep 60"));
    sandbox.wait_until_ready();
    signal_group(&killed, Signal::SIGKILL);
    wait(&mut killed);
    assert_eq!(sandbox.run_dirs(), 1);

    let mut next = sandbox.spawn("true");

    assert_eq!(wait(&mut next).code(), Some(0));
    assert_eq!(sandbox.run_dirs(), 0);
}
