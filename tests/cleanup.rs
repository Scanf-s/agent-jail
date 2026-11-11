//! These tests start real containers, so they need the requirements in the README and a
//! rootfs. They are ignored by default. Run them with `cargo test -- --ignored`.
//!
//! The rootfs is `$AJ_TEST_ROOTFS`, or `~/.local/share/agent-jail/images/default` without it.
//! Each test uses its own `XDG_DATA_HOME` and `--root`, so it never touches the user's run and
//! state directories, and tests that run at the same time do not clean up each other's.

use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
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

    /// Start `aj run` with the options `run_args`, `program`, and its arguments.
    fn spawn_with(&self, run_args: &[&str], program: &[&str]) -> Child {
        self.run_command(run_args, program).spawn().unwrap()
    }

    /// Run `aj run` as `spawn_with` does, wait for it, and return its output.
    fn run_output(&self, run_args: &[&str], program: &[&str]) -> Output {
        self.run_command(run_args, program).output().unwrap()
    }

    fn run_command(&self, run_args: &[&str], program: &[&str]) -> Command {
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
        command
    }

    /// Run `aj` with `args` against the state directories of this sandbox, and wait for it.
    fn aj(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aj"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .arg("--root")
            .arg(self.root.join("state"))
            .args(args)
            .output()
            .unwrap()
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.root.join("out/ready").exists() {
            assert!(Instant::now() < deadline, "the container did not start");
            sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the setup process records that the program runs. The program can write
    /// its ready file before that.
    fn wait_until_running(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self
            .states()
            .iter()
            .any(|state| state["status"] == "running")
        {
            assert!(Instant::now() < deadline, "the state never showed running");
            sleep(Duration::from_millis(20));
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
            .map(|entry| entry.unwrap().path())
            // A runtime creates its state directory under a temporary name that starts with '.'.
            .filter(|dir| !dir.file_name().unwrap().to_str().unwrap().starts_with('.'))
            .map(|dir| {
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

fn wait_until_ended(pid: Pid) {
    let deadline = Instant::now() + TIMEOUT;
    while is_running(pid) {
        assert!(Instant::now() < deadline, "process {pid} still runs");
        sleep(Duration::from_millis(20));
    }
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
    sandbox.wait_until_running();

    let init = only_child(only_child(Pid::from_raw(aj.id() as i32)));

    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["ociVersion"], "1.3.0");
    assert_eq!(states[0]["status"], "running");
    assert_eq!(states[0]["pid"], init.as_raw());
    assert!(states[0]["initStartTime"].as_u64().unwrap() > 0);
    let cgroup = PathBuf::from(states[0]["cgroup"].as_str().unwrap());
    assert!(cgroup.join("cgroup.events").is_file(), "{cgroup:?}");
    assert_eq!(states[0]["bundle"], rootfs().to_str().unwrap());
    assert_eq!(states[0]["autoDelete"], true);

    fs::write(sandbox.root.join("out/stop"), "").unwrap();
    assert_eq!(wait(&mut aj).code(), Some(0));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
}

#[test]
#[ignore = "starts a container"]
fn state_and_list_report_a_running_and_a_killed_container() {
    let sandbox = Sandbox::new("state-list");
    let mut aj = sandbox.spawn_with(
        &["--id", "box"],
        &["/bin/sh", "-c", "touch /out/ready && exec sleep 60"],
    );
    sandbox.wait_until_ready();
    sandbox.wait_until_running();
    let init = only_child(only_child(Pid::from_raw(aj.id() as i32)));

    let state: Value = serde_json::from_slice(&sandbox.aj(&["state", "box"]).stdout).unwrap();
    assert_eq!(
        state,
        json!({
            "ociVersion": "1.3.0",
            "id": "box",
            "status": "running",
            "pid": init.as_raw(),
            "bundle": rootfs(),
        })
    );
    let list = String::from_utf8(sandbox.aj(&["list"]).stdout).unwrap();
    let row = format!("box  {:<7}  running ", init.as_raw());
    assert!(list.lines().nth(1).unwrap().starts_with(&row), "{list}");

    // A killed runtime leaves running in the state file.
    signal_group(&aj, Signal::SIGKILL);
    wait(&mut aj);

    let state: Value = serde_json::from_slice(&sandbox.aj(&["state", "box"]).stdout).unwrap();
    assert_eq!(state["status"], "stopped");
    assert_eq!(state.get("pid"), None);
    let list = String::from_utf8(sandbox.aj(&["list"]).stdout).unwrap();
    assert!(
        list.lines()
            .nth(1)
            .unwrap()
            .starts_with("box  -        stopped "),
        "{list}"
    );

    let missing = sandbox.aj(&["state", "other"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no container"));
}

#[test]
#[ignore = "starts a container"]
fn rejects_an_id_that_a_container_uses() {
    let sandbox = Sandbox::new("same-id");
    let wait_for_stop = "touch /out/ready && while [ ! -e /out/stop ]; do sleep 0.1; done";
    let mut first = sandbox.spawn_with(&["--id", "box"], &["/bin/sh", "-c", wait_for_stop]);
    sandbox.wait_until_ready();
    sandbox.wait_until_running();

    let second = sandbox.run_output(&["--id", "box"], &["/bin/true"]);

    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("a container with ID box already exists"),
        "{stderr}"
    );
    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["status"], "running");
    fs::write(sandbox.root.join("out/stop"), "").unwrap();
    assert_eq!(wait(&mut first).code(), Some(0));
}

#[test]
#[ignore = "starts a container"]
fn runs_the_same_id_again_right_after_it_ends() {
    let sandbox = Sandbox::new("same-id-again");
    // systemd keeps the scope of a run for a short time after the run ends.
    for _ in 0..10 {
        let mut aj = sandbox.spawn_with(&["--id", "again"], &["/bin/true"]);
        assert_eq!(wait(&mut aj).code(), Some(0));
    }
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
fn passes_only_the_standard_streams_to_the_program() {
    let sandbox = Sandbox::new("fds");
    // A descriptor that the caller of aj leaves open must not reach the program, because it
    // refers to a host file.
    let host_file = File::open("/etc/hostname").unwrap();
    let host_fd = host_file.as_raw_fd();
    // With a redirection, the shell would keep a copy of its stdout, so ls prints to stdout.
    let mut command = sandbox.run_command(&[], &["/bin/sh", "-c", "ls /proc/1/fd; true"]);
    // SAFETY: dup2 is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            nix::unistd::dup2(host_fd, 9)?;
            Ok(())
        });
    }

    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(0));
    let fds = String::from_utf8(output.stdout).unwrap();
    assert_eq!(fds.split_whitespace().collect::<Vec<_>>(), ["0", "1", "2"]);
}

#[test]
#[ignore = "starts a container"]
fn reports_a_program_that_cannot_run() {
    let sandbox = Sandbox::new("no-program");

    let output = sandbox.run_output(&[], &["/no/such/program"]);

    assert_eq!(output.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(r#"container init failed: exec "/no/such/program""#),
        "{stderr}"
    );
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn reports_a_program_that_cannot_run_when_stderr_is_closed() {
    let sandbox = Sandbox::new("closed-stderr");
    // The reader of the pipe has ended, as with `aj run ... 2>&1 | head -1`.
    let (reader, writer) = nix::unistd::pipe().unwrap();
    drop(reader);

    let mut aj = sandbox
        .run_command(&[], &["/no/such/program"])
        .stderr(Stdio::from(writer))
        .spawn()
        .unwrap();

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
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    // Only a process in the user namespace can remove the overlay's work directory, so the
    // next run removes the run directory.
    let mut next = sandbox.spawn("true");
    assert_eq!(wait(&mut next).code(), Some(0));
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn moves_the_setup_process_out_of_the_container_cgroup() {
    let sandbox = Sandbox::new("setup-cgroup");
    let mut aj = sandbox.spawn("touch /out/ready && exec sleep 60");
    sandbox.wait_until_ready();
    sandbox.wait_until_running();
    let setup = only_child(Pid::from_raw(aj.id() as i32));
    let init = only_child(setup);

    // The runtime moves the setup process after init exists, which can take a moment.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let cgroup = fs::read_to_string(format!("/proc/{setup}/cgroup")).unwrap();
        if cgroup.trim_end().ends_with("/runtime") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the setup process stays in {cgroup}"
        );
        sleep(Duration::from_millis(20));
    }
    let container = PathBuf::from(sandbox.states()[0]["cgroup"].as_str().unwrap());
    let procs = fs::read_to_string(container.join("cgroup.procs")).unwrap();
    assert_eq!(
        procs.split_whitespace().collect::<Vec<_>>(),
        [init.to_string()]
    );

    kill(Pid::from_raw(aj.id() as i32), Signal::SIGTERM).unwrap();
    assert_eq!(wait(&mut aj).code(), Some(143));
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn container_init_ends_when_the_runtime_and_the_setup_process_are_killed() {
    let sandbox = Sandbox::new("all-killed");
    let mut aj = sandbox.spawn("touch /out/ready && exec sleep 60");
    sandbox.wait_until_ready();
    sandbox.wait_until_running();
    let setup = only_child(Pid::from_raw(aj.id() as i32));
    let init = only_child(setup);

    // Without the runtime, nothing writes cgroup.kill after the setup process ends.
    kill(Pid::from_raw(aj.id() as i32), Signal::SIGKILL).unwrap();
    wait(&mut aj);
    kill(setup, Signal::SIGKILL).unwrap();

    wait_until_ended(init);
}

#[test]
#[ignore = "starts a container"]
fn the_next_run_ends_a_container_that_changed_its_user() {
    let sandbox = Sandbox::new("changed-user");
    // Changing the user clears the parent death signal of container init.
    let mut aj = sandbox.spawn("touch /out/ready && exec su -s /bin/sh nobody -c 'exec sleep 60'");
    sandbox.wait_until_ready();
    let setup = only_child(Pid::from_raw(aj.id() as i32));
    let init = only_child(setup);
    let deadline = Instant::now() + TIMEOUT;
    while fs::read_to_string(format!("/proc/{init}/comm")).unwrap() != "sleep\n" {
        assert!(
            Instant::now() < deadline,
            "the program did not change its user"
        );
        sleep(Duration::from_millis(20));
    }
    kill(Pid::from_raw(aj.id() as i32), Signal::SIGKILL).unwrap();
    wait(&mut aj);
    kill(setup, Signal::SIGKILL).unwrap();
    sleep(Duration::from_millis(200));
    assert!(
        is_running(init),
        "the parent death signal ended the program"
    );

    let mut next = sandbox.spawn("true");

    assert_eq!(wait(&mut next).code(), Some(0));
    wait_until_ended(init);
    assert_eq!(sandbox.run_dirs(), 0);
    assert_eq!(sandbox.states(), Vec::<Value>::new());
}

#[test]
#[ignore = "starts a container"]
fn the_next_run_removes_what_a_killed_run_left() {
    let sandbox = Sandbox::new("sigkill");
    let mut killed = sandbox.spawn(&format!("{CREATE_SUBORDINATE_FILE} && exec sleep 60"));
    sandbox.wait_until_ready();
    sandbox.wait_until_running();
    signal_group(&killed, Signal::SIGKILL);
    wait(&mut killed);
    assert_eq!(sandbox.run_dirs(), 1);
    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["status"], "running");

    let mut next = sandbox.spawn("true");

    assert_eq!(wait(&mut next).code(), Some(0));
    assert_eq!(sandbox.run_dirs(), 0);
    assert_eq!(sandbox.states(), Vec::<Value>::new());
}

#[test]
#[ignore = "starts a container"]
fn a_killed_run_does_not_keep_its_id_in_use() {
    let sandbox = Sandbox::new("reuse-id");
    let mut killed = sandbox.spawn_with(
        &["--id", "box"],
        &["/bin/sh", "-c", "touch /out/ready && exec sleep 60"],
    );
    sandbox.wait_until_ready();
    signal_group(&killed, Signal::SIGKILL);
    wait(&mut killed);

    let mut next = sandbox.spawn_with(&["--id", "box"], &["/bin/true"]);

    assert_eq!(wait(&mut next).code(), Some(0));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
}
