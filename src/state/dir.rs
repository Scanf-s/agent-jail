use super::Record;
use crate::container_id::ContainerId;
use crate::flock::{LockKind, lock, try_lock};
use anyhow::{Context, Result, anyhow};
use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "state.json";
const TEMPORARY_FILE: &str = "state.json.tmp";

/// The directory `<root>/<id>` that holds the state file of one container.
///
/// Dropping it removes the directory, so a run that fails while it starts leaves nothing
/// behind. Only the runtime process that created it drops it, because the setup process and
/// container init end with exit or exec.
pub struct StateDir {
    path: PathBuf,
    /// What the state file holds now.
    record: Record,
    // An exclusive flock shows that a runtime process of the container is alive, as for the
    // run directory. The forked processes share this open file, and the kernel releases the
    // lock when the last of them ends, even when they are killed.
    _lock: File,
}

impl StateDir {
    /// Create the state directory of container `id` under `root`, and write `record` to it.
    pub fn create(root: &Path, id: &ContainerId, record: Record) -> Result<Self> {
        // The directory holds paths of the user's files, so only the user may read it.
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .with_context(|| format!("create {root:?}"))?;
        // The cleanup of stale state directories locks `root` exclusively. While this shared
        // lock is held, it cannot find the new directory before the directory is locked and
        // holds a state file.
        let _creating =
            lock(open(root)?, LockKind::Shared).with_context(|| format!("lock {root:?}"))?;
        let path = root.join(id.as_str());
        match fs::create_dir(&path) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(in_use(root, id));
            }
            result => result.with_context(|| format!("create {path:?}"))?,
        }
        let lock = try_lock(open(&path)?, LockKind::Exclusive)
            .with_context(|| format!("lock {path:?}"))?
            .with_context(|| format!("another process holds the lock of {path:?}"))?;

        let dir = Self {
            path,
            record,
            _lock: lock,
        };
        dir.write()?;
        Ok(dir)
    }

    /// Fail if container `id` has a state directory under `root`.
    ///
    /// `create` makes the final check. This earlier one can run before the runtime runs itself
    /// again in a systemd scope. Otherwise systemd-run could fail first, on the scope of the
    /// container that uses the ID, with a message that does not name the cause.
    pub fn check_unused(root: &Path, id: &ContainerId) -> Result<()> {
        let path = root.join(id.as_str());
        match fs::symlink_metadata(&path) {
            Ok(_) => Err(in_use(root, id)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("check {path:?}")),
        }
    }

    /// Apply `change` to the record and write it to the state file.
    pub fn update(&mut self, change: impl FnOnce(&mut Record)) -> Result<()> {
        change(&mut self.record);
        self.write()
    }

    /// Replace the state file with the record.
    ///
    /// The new content goes to a temporary file first. rename replaces the state file at once,
    /// so a reader sees either the old or the new record, never a part of one.
    fn write(&self) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(&self.record).context("serialize the container state")?;
        let temporary = self.path.join(TEMPORARY_FILE);
        fs::write(&temporary, json).with_context(|| format!("write {temporary:?}"))?;
        let state = self.path.join(STATE_FILE);
        fs::rename(&temporary, &state).with_context(|| format!("replace {state:?}"))
    }

    /// Remove the directory. A directory that is already gone counts as removed.
    pub fn remove(&self) -> Result<()> {
        match fs::remove_dir_all(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("remove state directory {:?}", self.path))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("open {path:?}"))
}

fn in_use(root: &Path, id: &ContainerId) -> anyhow::Error {
    anyhow!("a container with ID {id} already exists in {root:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::ContainerState;
    use rand::random;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("agent-jail-state-{:x}", random::<u64>())))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(id: &ContainerId) -> Record {
        Record::creating(id, PathBuf::from("/bundle"), None)
    }

    fn read(path: &Path) -> Record {
        serde_json::from_slice(&fs::read(path.join(STATE_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn writes_the_record_and_each_change() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box1").unwrap();
        let mut expected = record(&id);

        let mut dir = StateDir::create(&root.0, &id, expected.clone()).unwrap();
        assert_eq!(read(&dir.path), expected);

        dir.update(|record| {
            record.oci.set_status(ContainerState::Running);
        })
        .unwrap();
        expected.oci.set_status(ContainerState::Running);
        assert_eq!(read(&dir.path), expected);
        assert!(!dir.path.join(TEMPORARY_FILE).exists());
    }

    #[test]
    fn rejects_an_id_that_is_in_use() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box1").unwrap();
        let first = StateDir::create(&root.0, &id, record(&id)).unwrap();

        let error = StateDir::create(&root.0, &id, record(&id)).err().unwrap();

        assert!(error.to_string().contains("already exists"), "{error:#}");
        assert_eq!(read(&first.path).oci.id(), "box1");
    }

    #[test]
    fn checks_whether_an_id_is_unused() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box1").unwrap();
        StateDir::check_unused(&root.0, &id).unwrap();

        let _dir = StateDir::create(&root.0, &id, record(&id)).unwrap();

        let error = StateDir::check_unused(&root.0, &id).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
        let other = ContainerId::parse("box2").unwrap();
        StateDir::check_unused(&root.0, &other).unwrap();
    }

    #[test]
    fn holds_the_lock_and_removes_the_directory_when_dropped() {
        let root = TempRoot::new();
        let id = ContainerId::parse("box1").unwrap();
        let dir = StateDir::create(&root.0, &id, record(&id)).unwrap();
        let path = dir.path.clone();

        let probe = try_lock(File::open(&path).unwrap(), LockKind::Shared).unwrap();
        assert!(
            probe.is_none(),
            "another process could lock a live state directory"
        );

        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn creates_the_root_for_the_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempRoot::new();
        let id = ContainerId::parse("box1").unwrap();

        let _dir = StateDir::create(&root.0, &id, record(&id)).unwrap();

        let mode = fs::metadata(&root.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
