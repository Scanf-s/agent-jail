use super::Record;
use crate::flock::{LockKind, try_lock};
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Remove the state directories under `root` that no runtime process holds and that nothing
/// needs any more.
///
/// A killed run cannot remove its own state directory. When the user lingers, systemd clears
/// `$XDG_RUNTIME_DIR` only at reboot, so these directories would stay until then.
pub fn remove_stale(root: &Path) -> Result<()> {
    let mut result = Ok(());
    // The locks stay held until the removal ends, so another runtime skips these directories.
    for (path, _lock) in find_stale(root)? {
        match fs::remove_dir_all(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                result = Err(error).with_context(|| format!("remove state directory {path:?}"));
            }
            _ => {}
        }
    }
    result
}

/// Lock and return the state directories to remove.
fn find_stale(root: &Path) -> Result<Vec<(PathBuf, File)>> {
    let root_file = match File::open(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.with_context(|| format!("open {root:?}"))?,
    };
    // A runtime that is creating its state directory holds a shared lock. Waiting for it could
    // block for as long as that runtime is stopped, so this run skips the cleanup instead.
    let Some(_no_creation) =
        try_lock(root_file, LockKind::Exclusive).with_context(|| format!("lock {root:?}"))?
    else {
        return Ok(Vec::new());
    };

    let mut stale = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read {root:?}"))? {
        let entry = entry.with_context(|| format!("read {root:?}"))?;
        // file_type does not follow symbolic links, so only real directories are removed.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let dir = match File::open(&path) {
            // The run removed its own directory after read_dir listed it.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            result => result.with_context(|| format!("open {path:?}"))?,
        };
        // A held lock shows that a runtime process of the container is alive.
        let Some(lock) =
            try_lock(dir, LockKind::Exclusive).with_context(|| format!("lock {path:?}"))?
        else {
            continue;
        };
        if is_stale(&path) {
            stale.push((path, lock));
        }
    }
    Ok(stale)
}

/// Whether the state directory `dir`, which no runtime process holds, can go.
fn is_stale(dir: &Path) -> bool {
    // No runtime process will write a missing or broken state file again. The directory would
    // only keep its ID from being used.
    let Ok(record) = Record::load(dir) else {
        return true;
    };
    // OCI keeps a container that `create` made until `delete`, even after its program ended.
    // If the check of init fails, the directory stays, because init may still run.
    record.auto_delete && record.init_runs().is_ok_and(|runs| !runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_id::ContainerId;
    use crate::flock::lock;
    use crate::proc_stat;
    use crate::state::{STATE_FILE, StateDir};
    use nix::unistd::Pid;
    use oci_spec::runtime::ContainerState;
    use rand::random;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("agent-jail-stale-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        /// Write a state directory as a killed runtime would leave it.
        fn leave(&self, name: &str, change: impl FnOnce(&mut Record)) {
            let id = ContainerId::parse(name).unwrap();
            let mut record = Record::creating(&id, PathBuf::from("/bundle"), None);
            change(&mut record);
            let dir = self.0.join(name);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join(STATE_FILE), serde_json::to_vec(&record).unwrap()).unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(stale: &[(PathBuf, File)]) -> Vec<String> {
        let mut names: Vec<String> = stale
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Make `record` name a running init: this test process, with its start time plus `offset`.
    fn running(record: &mut Record, offset: u64) {
        let this = Pid::this();
        record
            .oci
            .set_status(ContainerState::Running)
            .set_pid(Some(this.as_raw()));
        record.init_start_time = Some(proc_stat::start_time(this).unwrap() + offset);
    }

    #[test]
    fn finds_what_killed_runs_left() {
        let root = TempRoot::new();
        let live = ContainerId::parse("live").unwrap();
        let _live = StateDir::create(
            &root.0,
            &live,
            Record::creating(&live, PathBuf::from("/bundle"), None),
        )
        .unwrap();
        // An init with another start time is a later process that reuses the PID.
        root.leave("killed", |record| running(record, 1));
        root.leave("creating", |_| {});
        root.leave("init-runs", |record| running(record, 0));
        root.leave("created-by-create", |record| {
            running(record, 1);
            record.auto_delete = false;
        });
        fs::create_dir(root.0.join("no-state")).unwrap();
        root.leave("broken", |_| {});
        fs::write(root.0.join("broken").join(STATE_FILE), "{").unwrap();
        fs::write(root.0.join("note"), "").unwrap();

        let stale = find_stale(&root.0).unwrap();

        assert_eq!(names(&stale), ["broken", "creating", "killed", "no-state"]);
        // While the first cleanup holds its locks, a second one finds nothing.
        assert!(find_stale(&root.0).unwrap().is_empty());
    }

    #[test]
    fn removes_the_stale_directories() {
        let root = TempRoot::new();
        root.leave("killed", |record| running(record, 1));
        root.leave("init-runs", |record| running(record, 0));

        remove_stale(&root.0).unwrap();

        assert!(!root.0.join("killed").exists());
        assert!(root.0.join("init-runs").exists());
    }

    #[test]
    fn skips_the_cleanup_while_a_state_directory_is_created() {
        let root = TempRoot::new();
        root.leave("creating", |_| {});
        let _creating = lock(File::open(&root.0).unwrap(), LockKind::Shared).unwrap();

        assert!(find_stale(&root.0).unwrap().is_empty());
    }

    #[test]
    fn finds_nothing_without_a_root() {
        let root = TempRoot::new();
        assert!(find_stale(&root.0.join("missing")).unwrap().is_empty());
    }
}
