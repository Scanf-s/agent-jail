//! Tests of the OCI lifecycle commands. They start real containers, so they need the
//! requirements in the README and a rootfs. They are ignored by default. Run them with
//! `cargo test -- --ignored`.
//!
//! The rootfs is `$AJ_TEST_ROOTFS`, or `~/.local/share/agent-jail/images/default` without it.

mod common;

use common::*;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getsid};
use serde_json::Value;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

const PROGRAM: [&str; 3] = ["/bin/sh", "-c", "echo started; exec sleep 60"];

fn init_of(state: &Value) -> Pid {
    Pid::from_raw(state["pid"].as_i64().unwrap() as i32)
}

#[test]
#[ignore = "starts a container"]
fn create_prepares_the_container_without_running_its_program() {
    let sandbox = Sandbox::new("create");
    let bundle = sandbox.bundle(&PROGRAM, "/");

    let created = sandbox.create(&bundle, "box");

    assert!(created.status.success(), "{}", created.stderr);
    let state = sandbox.state("box");
    assert_eq!(state["status"], "created");
    assert_eq!(state["bundle"], bundle.to_str().unwrap());
    let init = init_of(&state);
    assert!(is_running(init));
    // The container does not receive the signals of the caller's terminal.
    assert_ne!(getsid(Some(init)).unwrap(), getsid(None).unwrap());
    // The setup process keeps neither the streams nor the working directory of the caller.
    let setup = parent_of(init);
    assert_eq!(
        fs::read_link(format!("/proc/{setup}/cwd")).unwrap(),
        Path::new("/")
    );
    for stream in 0..=2 {
        let target = fs::read_link(format!("/proc/{setup}/fd/{stream}")).unwrap();
        assert_eq!(target, Path::new("/dev/null"), "stream {stream}");
    }
    let list = String::from_utf8(sandbox.aj(&["list"]).stdout).unwrap();
    assert!(list.lines().nth(1).unwrap().contains(" created "), "{list}");
    sleep(Duration::from_millis(200));
    assert_eq!(fs::read_to_string(&created.stdout).unwrap(), "");

    kill(init, Signal::SIGKILL).unwrap();
    wait_until_ended(init);
}

#[test]
#[ignore = "starts a container"]
fn the_setup_process_keeps_no_descriptor_of_the_caller() {
    let sandbox = Sandbox::new("create-fds");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    // A kept descriptor could, for example, hold a file lock of the caller until the container
    // ends.
    let inherited = sandbox.root.join("inherited");
    let file = File::create(&inherited).unwrap();
    let fd = file.as_raw_fd();

    let created = sandbox.create_with(&bundle, "box", |command| {
        // SAFETY: dup2 is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                nix::unistd::dup2(fd, 9)?;
                Ok(())
            });
        }
    });

    assert!(created.status.success(), "{}", created.stderr);
    let init = init_of(&sandbox.state("box"));
    let setup = parent_of(init);
    let kept: Vec<_> = fs::read_dir(format!("/proc/{setup}/fd"))
        .unwrap()
        .filter_map(|entry| fs::read_link(entry.unwrap().path()).ok())
        .collect();
    assert!(!kept.contains(&inherited), "{kept:?}");

    kill(init, Signal::SIGKILL).unwrap();
    wait_until_ended(init);
}

#[test]
#[ignore = "starts a container"]
fn a_created_container_stays_after_its_program_ended() {
    let sandbox = Sandbox::new("create-stays");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));
    let setup = parent_of(init);

    kill(init, Signal::SIGKILL).unwrap();

    let state = sandbox.wait_for_status("box", "stopped");
    assert_eq!(state.get("pid"), None);
    // OCI keeps the container until it is deleted, so its ID stays in use.
    let again = sandbox.create(&bundle, "box");
    assert!(!again.status.success());
    assert!(again.stderr.contains("already exists"), "{}", again.stderr);
    let states = sandbox.states();
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states[0]["autoDelete"], false);
    // The setup process removes the run directory and its socket when init ends.
    wait_until_ended(setup);
    assert_eq!(sandbox.run_dirs(), 0);
    let mut kept: Vec<_> = fs::read_dir(sandbox.root.join("state/box"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    kept.sort();
    assert_eq!(kept, ["state.json"]);
}

#[test]
#[ignore = "starts a container"]
fn a_created_container_ends_with_its_setup_process() {
    let sandbox = Sandbox::new("create-setup-killed");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));

    kill(parent_of(init), Signal::SIGKILL).unwrap();

    wait_until_ended(init);
    assert_eq!(sandbox.state("box")["status"], "stopped");
}

#[test]
#[ignore = "starts a container"]
fn a_failed_create_leaves_nothing() {
    let sandbox = Sandbox::new("create-fails");
    let bundle = sandbox.bundle(&PROGRAM, "/no/such/directory");

    let created = sandbox.create(&bundle, "box");

    assert_eq!(created.status.code(), Some(127));
    assert!(
        created
            .stderr
            .contains("container init failed: chdir to working directory /no/such/directory"),
        "{}",
        created.stderr
    );
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn start_runs_the_program_of_a_created_container() {
    let sandbox = Sandbox::new("start");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    let created = sandbox.create(&bundle, "box");
    assert!(created.status.success(), "{}", created.stderr);
    let init = init_of(&sandbox.state("box"));

    let started = sandbox.aj(&["start", "box"]);

    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    // `aj start` returns after the runtime recorded that the program runs.
    let state = sandbox.state("box");
    assert_eq!(state["status"], "running");
    assert_eq!(init_of(&state), init);
    let deadline = Instant::now() + TIMEOUT;
    while fs::read_to_string(&created.stdout).unwrap() != "started\n" {
        assert!(Instant::now() < deadline, "the program did not print");
        sleep(Duration::from_millis(20));
    }
    assert!(!sandbox.root.join("state/box/start.sock").exists());

    kill(init, Signal::SIGKILL).unwrap();
    sandbox.wait_for_status("box", "stopped");
}

#[test]
#[ignore = "starts a container"]
fn start_fails_without_effect_unless_the_container_is_created() {
    let sandbox = Sandbox::new("start-twice");
    let missing = sandbox.aj(&["start", "box"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no container"));

    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    assert!(sandbox.aj(&["start", "box"]).status.success());
    let init = init_of(&sandbox.state("box"));

    let again = sandbox.aj(&["start", "box"]);

    assert!(!again.status.success());
    let stderr = String::from_utf8_lossy(&again.stderr);
    assert!(stderr.contains("box is running, not created"), "{stderr}");
    assert_eq!(sandbox.state("box")["status"], "running");
    kill(init, Signal::SIGKILL).unwrap();
}

#[test]
#[ignore = "starts a container"]
fn only_one_of_two_starts_runs_the_program() {
    let sandbox = Sandbox::new("start-race");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));

    let outputs: Vec<_> = std::thread::scope(|scope| {
        let starts: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| sandbox.aj(&["start", "box"])))
            .collect();
        starts
            .into_iter()
            .map(|start| start.join().unwrap())
            .collect()
    });

    let succeeded = outputs.iter().filter(|output| output.status.success());
    assert_eq!(succeeded.count(), 1, "{outputs:?}");
    let failed = outputs
        .iter()
        .find(|output| !output.status.success())
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("box is running, not created") || stderr.contains("already starting"),
        "{stderr}"
    );
    kill(init, Signal::SIGKILL).unwrap();
}

#[test]
#[ignore = "starts a container"]
fn start_reports_a_program_that_cannot_run() {
    let sandbox = Sandbox::new("start-no-program");
    let bundle = sandbox.bundle(&["/no/such/program"], "/");
    assert!(sandbox.create(&bundle, "box").status.success());

    let started = sandbox.aj(&["start", "box"]);

    assert!(!started.status.success());
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(stderr.contains(r#"exec "/no/such/program""#), "{stderr}");
    sandbox.wait_for_status("box", "stopped");
}

#[test]
#[ignore = "starts a container"]
fn start_succeeds_for_a_program_that_ends_at_once() {
    let sandbox = Sandbox::new("start-true");
    let bundle = sandbox.bundle(&["/bin/true"], "/");
    for id in ["box1", "box2", "box3"] {
        assert!(sandbox.create(&bundle, id).status.success());

        let started = sandbox.aj(&["start", id]);

        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        sandbox.wait_for_status(id, "stopped");
    }
}

#[test]
#[ignore = "starts a container"]
fn a_started_container_ends_with_its_setup_process() {
    let sandbox = Sandbox::new("start-setup-killed");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    assert!(sandbox.aj(&["start", "box"]).status.success());
    let init = init_of(&sandbox.state("box"));

    kill(parent_of(init), Signal::SIGKILL).unwrap();

    wait_until_ended(init);
    assert_eq!(sandbox.state("box")["status"], "stopped");
}

#[test]
#[ignore = "starts a container"]
fn delete_removes_a_stopped_container() {
    let sandbox = Sandbox::new("delete");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));
    let setup = parent_of(init);
    kill(init, Signal::SIGKILL).unwrap();
    sandbox.wait_for_status("box", "stopped");

    let deleted = sandbox.aj(&["delete", "box"]);

    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    // delete waits until the setup process has finished its own cleanup.
    assert!(!is_running(setup));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
    let state = sandbox.aj(&["state", "box"]);
    assert!(!state.status.success());
    assert!(String::from_utf8_lossy(&state.stderr).contains("no container"));
    // The ID is free again.
    let again = sandbox.create(&bundle, "box");
    assert!(again.status.success(), "{}", again.stderr);
    let init = init_of(&sandbox.state("box"));
    kill(init, Signal::SIGKILL).unwrap();
    wait_until_ended(init);
}

#[test]
#[ignore = "starts a container"]
fn delete_fails_without_effect_unless_the_container_is_stopped() {
    let sandbox = Sandbox::new("delete-refused");
    let missing = sandbox.aj(&["delete", "box"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no container"));

    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));
    for status in ["created", "running"] {
        if status == "running" {
            assert!(sandbox.aj(&["start", "box"]).status.success());
        }

        let refused = sandbox.aj(&["delete", "box"]);

        assert!(!refused.status.success());
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(&format!("box is {status}")), "{stderr}");
        assert_eq!(sandbox.state("box")["status"], status);
        assert!(is_running(init));
    }
    kill(init, Signal::SIGKILL).unwrap();
    wait_until_ended(init);
}

#[test]
#[ignore = "starts a container"]
fn delete_force_ends_and_removes_a_running_container() {
    let sandbox = Sandbox::new("delete-force");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    assert!(sandbox.aj(&["start", "box"]).status.success());
    let init = init_of(&sandbox.state("box"));

    let deleted = sandbox.aj(&["delete", "--force", "box"]);

    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!is_running(init));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
}

#[test]
#[ignore = "starts a container"]
fn delete_force_ends_a_container_of_run() {
    let sandbox = Sandbox::new("delete-run");
    let mut aj = sandbox.spawn_with(
        &["--id", "box"],
        &["/bin/sh", "-c", "touch /out/ready && exec sleep 60"],
    );
    sandbox.wait_until_ready();
    sandbox.wait_until_running();

    let deleted = sandbox.aj(&["delete", "--force", "box"]);

    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    // The program ended on SIGKILL, and the run removed what it owned.
    assert_eq!(wait(&mut aj).code(), Some(137));
    assert_eq!(sandbox.states(), Vec::<Value>::new());
    assert_eq!(sandbox.run_dirs(), 0);
}
