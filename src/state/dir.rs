use super::{Record, STATE_FILE};
use crate::container_id::ContainerId;
use crate::flock::{LockKind, lock, try_lock};
use anyhow::{Context, Result, anyhow};
use nix::errno::Errno;
use nix::fcntl::{RenameFlags, renameat2};
use nix::unistd::Pid;
use rand::random;
use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const TEMPORARY_FILE: &str = "state.json.tmp";

/// The directory `<root>/<id>` that holds the state file of one container.
///
/// Dropping it in the process that created it removes the directory, so a run that fails while
/// it starts leaves nothing behind. `keep` stops that.
pub struct StateDir {
    path: PathBuf,
    /// What the state file holds now.
    record: Record,
    /// The process that created the directory. The setup process and container init have a
    /// copy after fork, and a panic there would drop it while the container still runs.
    owner: Pid,
    keep: bool,
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
        // lock is held, it cannot find the new directory before the directory is locked.
        let _creating =
            lock(open(root)?, LockKind::Shared).with_context(|| format!("lock {root:?}"))?;

        // The directory gets the name of the ID only after it has its lock and its state file,
        // so a reader never finds it without them. No ID starts with '.'.
        let temporary = root.join(format!(".creating-{:016x}", random::<u64>()));
        fs::create_dir(&temporary).with_context(|| format!("create {temporary:?}"))?;
        let lock = match lock_new_dir(&temporary) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir(&temporary);
                return Err(error);
            }
        };
        // From here on, dropping `dir` removes the directory if a later step fails.
        let mut dir = Self {
            path: temporary,
            record,
            owner: Pid::this(),
            keep: false,
            _lock: lock,
        };
        dir.write()?;

        let path = root.join(id.as_str());
        match renameat2(None, &dir.path, None, &path, RenameFlags::RENAME_NOREPLACE) {
            Err(Errno::EEXIST) => return Err(in_use(root, id)),
            result => result.with_context(|| format!("rename {:?} to {path:?}", dir.path))?,
        }
        dir.path = path;
        Ok(dir)
    }

    /// Leave the directory in place when this is dropped.
    pub fn keep(&mut self) {
        self.keep = true;
    }

    /// Fail if container `id` has a state directory under `root`.
    ///
    /// `create` makes the final check. This earlier one can run before the runtime starts a
    /// systemd scope for a container that cannot start anyway.
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
        if !self.keep && Pid::this() == self.owner {
            let _ = self.remove();
        }
    }
}

fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("open {path:?}"))
}

/// Lock the directory `path` that this process just created.
fn lock_new_dir(path: &Path) -> Result<File> {
    try_lock(open(path)?, LockKind::Exclusive)
        .with_context(|| format!("lock {path:?}"))?
        .with_context(|| format!("another process holds the lock of {path:?}"))
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
        Record::load(path).unwrap()
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
        // The failed run leaves no directory under a temporary name.
        assert_eq!(entries(&root.0), ["box1"]);
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
    fn stays_when_kept_or_dropped_in_another_process() {
        let root = TempRoot::new();
        let kept = ContainerId::parse("kept").unwrap();
        let mut dir = StateDir::create(&root.0, &kept, record(&kept)).unwrap();
        dir.keep();
        drop(dir);
        assert!(root.0.join("kept").exists());

        let copied = ContainerId::parse("copied").unwrap();
        let mut dir = StateDir::create(&root.0, &copied, record(&copied)).unwrap();
        // A forked process has another PID than the process that created the directory.
        dir.owner = Pid::from_raw(dir.owner.as_raw() + 1);
        drop(dir);
        assert!(root.0.join("copied").exists());
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
