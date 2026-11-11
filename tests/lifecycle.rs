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
use std::fs;
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
fn a_created_container_stays_after_its_program_ended() {
    let sandbox = Sandbox::new("create-stays");
    let bundle = sandbox.bundle(&PROGRAM, "/");
    assert!(sandbox.create(&bundle, "box").status.success());
    let init = init_of(&sandbox.state("box"));

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
    // The setup process removes the run directory when init ends.
    let deadline = Instant::now() + TIMEOUT;
    while sandbox.run_dirs() != 0 {
        assert!(Instant::now() < deadline, "the run directory stays");
        sleep(Duration::from_millis(20));
    }
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
