//! Helpers for the tests that start real containers.
//!
//! Each test uses its own `XDG_DATA_HOME` and `--root`, so it never touches the user's run and
//! state directories, and tests that run at the same time do not clean up each other's.

// Each test file uses only some of the helpers.
#![allow(dead_code)]

use nix::sys::signal::{SigHandler, Signal, kill, signal};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Sandbox {
    pub root: PathBuf,
}

impl Sandbox {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("aj-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("out")).unwrap();
        Self { root }
    }

    /// Start `aj run` with `script` in its own process group, as a shell does for a job.
    pub fn spawn(&self, script: &str) -> Child {
        self.spawn_with(&[], &["/bin/sh", "-c", script])
    }

    /// Start `aj run` with the options `run_args`, `program`, and its arguments.
    pub fn spawn_with(&self, run_args: &[&str], program: &[&str]) -> Child {
        self.run_command(run_args, program).spawn().unwrap()
    }

    /// Run `aj run` as `spawn_with` does, wait for it, and return its output.
    pub fn run_output(&self, run_args: &[&str], program: &[&str]) -> Output {
        self.run_command(run_args, program).output().unwrap()
    }

    pub fn run_command(&self, run_args: &[&str], program: &[&str]) -> Command {
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
    pub fn aj(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aj"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .arg("--root")
            .arg(self.root.join("state"))
            .args(args)
            .output()
            .unwrap()
    }

    /// Write an OCI bundle whose program is `args`, running in `cwd`, and return its directory.
    pub fn bundle(&self, args: &[&str], cwd: &str) -> PathBuf {
        let dir = self.root.join("bundle");
        fs::create_dir_all(&dir).unwrap();
        let config = json!({
            "ociVersion": "1.3.0",
            "root": { "path": rootfs() },
            "process": {
                "user": { "uid": 0, "gid": 0 },
                "args": args,
                "env": ["PATH=/bin:/usr/bin:/sbin:/usr/sbin"],
                "cwd": cwd,
            },
            "mounts": [{
                "destination": "/proc",
                "type": "proc",
                "source": "proc",
                "options": ["nosuid", "nodev", "noexec"],
            }],
            "linux": {
                "namespaces": [
                    { "type": "user" },
                    { "type": "pid" },
                    { "type": "network" },
                    { "type": "mount" },
                    { "type": "cgroup" },
                ],
            },
        });
        fs::write(dir.join("config.json"), config.to_string()).unwrap();
        dir
    }

    /// Run `aj create` with `bundle` and `id`, and wait for it.
    ///
    /// The container program keeps the standard streams of `aj create`. A pipe would reach EOF
    /// only when the program ends, so the streams go to files in the sandbox.
    pub fn create(&self, bundle: &Path, id: &str) -> Created {
        self.create_with(bundle, id, |_| {})
    }

    /// Like `create`, but let `configure` change the command first.
    pub fn create_with(
        &self,
        bundle: &Path,
        id: &str,
        configure: impl FnOnce(&mut Command),
    ) -> Created {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("create-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let stdout = self.root.join(format!("{name}.stdout"));
        let stderr = self.root.join(format!("{name}.stderr"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_aj"));
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .arg("--root")
            .arg(self.root.join("state"))
            .arg("create")
            .arg("--bundle")
            .arg(bundle)
            .arg(id)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        configure(&mut command);
        let mut aj = command.spawn().unwrap();
        let status = wait(&mut aj);
        Created {
            status,
            stdout,
            stderr: fs::read_to_string(stderr).unwrap(),
        }
    }

    /// The OCI state that `aj state` prints for container `id`.
    pub fn state(&self, id: &str) -> Value {
        let output = self.aj(&["state", id]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Wait until `aj state` shows `status` for container `id`, and return that state.
    pub fn wait_for_status(&self, id: &str, status: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.state(id);
            if state["status"] == status {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "container {id} never showed {status}: {state}"
            );
            sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_until_ready(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.root.join("out/ready").exists() {
            assert!(Instant::now() < deadline, "the container did not start");
            sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the setup process records that the program runs. The program can write
    /// its ready file before that.
    pub fn wait_until_running(&self) {
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

    pub fn run_dirs(&self) -> usize {
        fs::read_dir(self.root.join("data/agent-jail/runs")).map_or(0, |entries| entries.count())
    }

    /// The content of each state file under `--root`.
    pub fn states(&self) -> Vec<Value> {
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

/// What `Sandbox::create` saw of `aj create`.
pub struct Created {
    pub status: ExitStatus,
    /// The file that receives the standard output of the container program.
    pub stdout: PathBuf,
    pub stderr: String,
}

pub fn rootfs() -> PathBuf {
    std::env::var_os("AJ_TEST_ROOTFS").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local/share/agent-jail/images/default")
        },
        PathBuf::from,
    )
}

impl Drop for Sandbox {
    /// Remove the sandbox. A killed run leaves the overlay work directory without permissions,
    /// and the setup process of a container that a test ended may still remove its run
    /// directory, so this retries for a short time.
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::remove_dir_all(&self.root).is_err()
            && self.root.exists()
            && Instant::now() < deadline
        {
            allow_removal(&self.root);
            sleep(Duration::from_millis(50));
        }
    }
}

/// Give the owner full permissions on `dir` and on every directory below it.
fn allow_removal(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            allow_removal(&entry.path());
        }
    }
}

pub fn wait(child: &mut Child) -> ExitStatus {
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

pub fn signal_group(child: &Child, signal: Signal) {
    kill(Pid::from_raw(-(child.id() as i32)), signal).unwrap();
}

/// The parent process of `pid`.
pub fn parent_of(pid: Pid) -> Pid {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // The fields after the command name, which is in parentheses, are the state and the parent.
    let parent = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(1)
        .unwrap();
    Pid::from_raw(parent.parse().unwrap())
}

/// The only child process of `pid`.
pub fn only_child(pid: Pid) -> Pid {
    let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let children: Vec<&str> = children.split_whitespace().collect();
    assert_eq!(children.len(), 1, "process {pid} has children {children:?}");
    Pid::from_raw(children[0].parse().unwrap())
}

pub fn wait_until_ended(pid: Pid) {
    let deadline = Instant::now() + TIMEOUT;
    while is_running(pid) {
        assert!(Instant::now() < deadline, "process {pid} still runs");
        sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` runs. A zombie has ended and only waits for its parent to reap it.
pub fn is_running(pid: Pid) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state follows the command name, which is in parentheses and may contain spaces.
    let state = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .next();
    !matches!(state, Some("Z" | "X"))
}
