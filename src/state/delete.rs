use super::Record;
use super::query::{lock_held, read};
use crate::cgroups;
use crate::container_id::ContainerId;
use crate::flock::{LockKind, lock, try_lock};
use anyhow::{Context, Result, bail, ensure};
use oci_spec::runtime::ContainerState;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long `delete` waits for the setup process to finish its own cleanup after init ended.
/// It removes the run directory, which can hold many installed files.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Delete container `id` under `root`: end its processes and remove its state.
///
/// Without `force`, only a stopped container can be deleted, as OCI requires. If a process does
/// not end, the state stays, so that the container stays visible and a later delete can retry.
pub fn delete(root: &Path, id: &ContainerId, force: bool) -> Result<()> {
    let record = read(root, id)?;
    let status = *record.oci.status();
    ensure!(
        force || status == ContainerState::Stopped,
        "container {id} is {status}; only a stopped container can be deleted, or use --force"
    );
    // A stopped container can still hold processes, for example one in uninterruptible sleep
    // while init ended.
    cgroups::kill_all(&record.cgroup)
        .with_context(|| format!("end the processes of container {id}, so its state stays"))?;

    let dir = root.join(id.as_str());
    wait_for_runtime(&dir, id)?;
    remove(root, &dir, id, &record)
}

/// Wait until no runtime process of the container holds the lock of `dir`.
///
/// After init ends, the setup process records that the container stopped and removes the run
/// directory before it exits. The runtime of `aj run` also removes the state directory itself.
fn wait_for_runtime(dir: &Path, id: &ContainerId) -> Result<()> {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match lock_held(dir) {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(error) if is_not_found(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
        if Instant::now() > deadline {
            bail!(
                "container {id} still cleans up after {CLEANUP_TIMEOUT:?}; run aj delete again later"
            );
        }
        sleep(Duration::from_millis(20));
    }
}

/// Remove the state directory `dir` of the container that `record` describes.
fn remove(root: &Path, dir: &Path, id: &ContainerId, record: &Record) -> Result<()> {
    // A runtime creates a state directory only while it holds a shared lock on the root, so no
    // new container can take the ID while this holds the exclusive lock.
    let _root = lock(open(root)?, LockKind::Exclusive).with_context(|| format!("lock {root:?}"))?;
    let file = match File::open(dir) {
        // The run of an `aj run` container removed it.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result.with_context(|| format!("open {dir:?}"))?,
    };
    let Some(_dir) =
        try_lock(file, LockKind::Exclusive).with_context(|| format!("lock {dir:?}"))?
    else {
        bail!("a new container with ID {id} started while the old one was deleted");
    };
    // Between the wait and the lock, the run of the old container can remove the directory, and
    // a new container can take the ID and end again. Each container has its own cgroup.
    let current = Record::load(dir)?;
    if current.cgroup != record.cgroup {
        bail!("a new container with ID {id} started while the old one was deleted");
    }
    fs::remove_dir_all(dir).with_context(|| format!("remove state directory {dir:?}"))
}

fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("open {path:?}"))
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{STATE_FILE, StateDir};
    use rand::random;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("agent-jail-delete-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Leave a state directory of container `id` with `status`, as a runtime that ended would.
    fn leave(root: &Path, id: &ContainerId, status: ContainerState, cgroup: &str) {
        let mut record = Record::creating(id, PathBuf::from(cgroup), PathBuf::from("/"), None);
        record.oci.set_status(status);
        record.auto_delete = false;
        let mut dir = StateDir::create(root, id, record).unwrap();
        dir.keep();
    }

    #[test]
    fn removes_a_stopped_container() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box").unwrap();
        leave(&root.0, &id, ContainerState::Stopped, "/no/cgroup");

        delete(&root.0, &id, false).unwrap();

        assert!(!root.0.join("box").exists());
    }

    #[test]
    fn fails_for_an_unknown_container() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box").unwrap();
        let error = delete(&root.0, &id, false).unwrap_err();
        assert!(error.to_string().contains("no container"), "{error:#}");
    }

    #[test]
    fn keeps_the_directory_of_a_new_container_with_the_same_id() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box").unwrap();
        leave(&root.0, &id, ContainerState::Stopped, "/new/cgroup");
        let old = Record::creating(&id, PathBuf::from("/old/cgroup"), PathBuf::from("/"), None);

        let error = remove(&root.0, &root.0.join("box"), &id, &old).unwrap_err();

        assert!(error.to_string().contains("new container"), "{error:#}");
        assert!(root.0.join("box").join(STATE_FILE).exists());
    }
}
