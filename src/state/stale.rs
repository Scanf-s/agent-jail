use super::{Record, STATE_FILE};
use crate::cgroups;
use crate::flock::{LockKind, try_lock};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs::{self, DirEntry, File};
use std::io;
use std::path::{Path, PathBuf};

/// Remove the state directories under `root` that no runtime process holds and that nothing
/// needs any more.
///
/// A killed run cannot remove its own state directory. When the user lingers, systemd clears
/// `$XDG_RUNTIME_DIR` only at reboot, so these directories would stay until then.
pub fn remove_stale(root: &Path) -> Result<()> {
    let Some(mut scan) = find_stale(root)? else {
        return Ok(());
    };
    for (path, _lock) in &scan.stale {
        match fs::remove_dir_all(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => scan
                .errors
                .push(anyhow::Error::new(error).context(format!("remove {path:?}"))),
            _ => {}
        }
    }
    if scan.errors.is_empty() {
        return Ok(());
    }
    let messages: Vec<String> = scan
        .errors
        .iter()
        .map(|error| format!("{error:#}"))
        .collect();
    bail!("{}", messages.join("; "))
}

/// The state directories that a cleanup found, with the locks that it holds until it ends.
struct Scan {
    // A runtime creates a state directory only while it holds a shared lock on the root. While
    // this exclusive lock is held, no runtime can create a directory at the path of one that
    // this cleanup removes.
    _root_lock: File,
    /// The stale directories, each with its exclusive lock.
    stale: Vec<(PathBuf, File)>,
    /// Errors for single directories. Those directories stay, and the cleanup goes on.
    errors: Vec<anyhow::Error>,
}

/// Lock and find the state directories to remove. Returns `None` if there is no root, or if a
/// runtime is creating a state directory now.
fn find_stale(root: &Path) -> Result<Option<Scan>> {
    let root_file = match File::open(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result.with_context(|| format!("open {root:?}"))?,
    };
    // Waiting for a runtime that creates its state directory could block for as long as that
    // runtime is stopped, so this run skips the cleanup instead.
    let Some(root_lock) =
        try_lock(root_file, LockKind::Exclusive).with_context(|| format!("lock {root:?}"))?
    else {
        return Ok(None);
    };

    let mut scan = Scan {
        _root_lock: root_lock,
        stale: Vec::new(),
        errors: Vec::new(),
    };
    for entry in fs::read_dir(root).with_context(|| format!("read {root:?}"))? {
        match lock_if_stale(entry) {
            Ok(Some(found)) => scan.stale.push(found),
            Ok(None) => {}
            Err(error) => scan.errors.push(error),
        }
    }
    Ok(Some(scan))
}

/// Lock and return the directory of `entry` if it is a stale state directory.
fn lock_if_stale(entry: io::Result<DirEntry>) -> Result<Option<(PathBuf, File)>> {
    let entry = entry.context("read the state root")?;
    // file_type does not follow symbolic links, so only real directories are removed.
    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
        return Ok(None);
    }
    let path = entry.path();
    let dir = match File::open(&path) {
        // The run removed its own directory after read_dir listed it.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result.with_context(|| format!("open {path:?}"))?,
    };
    // A held lock shows that a runtime process of the container is alive.
    let Some(lock) =
        try_lock(dir, LockKind::Exclusive).with_context(|| format!("lock {path:?}"))?
    else {
        return Ok(None);
    };
    Ok(is_stale(&path)?.then_some((path, lock)))
}

/// Whether the state directory `dir`, which no runtime process holds, can go.
fn is_stale(dir: &Path) -> Result<bool> {
    let path = dir.join(STATE_FILE);
    let json = match fs::read(&path) {
        // A run that was killed while it created the directory left no state file. No runtime
        // process will write one, and the directory would only keep its name in use.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        result => result.with_context(|| format!("read {path:?}"))?,
    };
    // The runtime replaces the file at once, so a broken file is not a write in progress.
    let Ok(value) = serde_json::from_slice::<Value>(&json) else {
        return Ok(true);
    };
    // A later version of the runtime may keep containers that this version would remove.
    let version = value.get("stateVersion").and_then(Value::as_u64);
    if version.is_some_and(|version| version > u64::from(Record::VERSION)) {
        return Ok(false);
    }
    let Ok(record) = serde_json::from_value::<Record>(value) else {
        return Ok(true);
    };
    // OCI keeps a container that `create` made until `delete`, even after its program ended.
    // A process that the runtime could not end, for example one in uninterruptible sleep, keeps
    // the cgroup populated. Until it ends, the state shows the container and keeps its ID in use.
    Ok(record.auto_delete && !record.init_runs()? && !cgroups::holds_processes(&record.cgroup)?)
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
            let mut record = Record::creating(
                &id,
                PathBuf::from("/no/cgroup"),
                PathBuf::from("/bundle"),
                None,
            );
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
            Record::creating(
                &live,
                PathBuf::from("/no/cgroup"),
                PathBuf::from("/bundle"),
                None,
            ),
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
        // A run killed while it created its state directory leaves it under a temporary name.
        fs::create_dir(root.0.join(".creating-0123456789abcdef")).unwrap();
        root.leave("broken", |_| {});
        fs::write(root.0.join("broken").join(STATE_FILE), "{").unwrap();
        root.leave("later-version", |_| {});
        let later = root.0.join("later-version").join(STATE_FILE);
        let mut json: Value = serde_json::from_slice(&fs::read(&later).unwrap()).unwrap();
        json["stateVersion"] = (Record::VERSION + 1).into();
        fs::write(&later, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::write(root.0.join("note"), "").unwrap();

        let scan = find_stale(&root.0).unwrap().unwrap();

        assert_eq!(
            names(&scan.stale),
            [
                ".creating-0123456789abcdef",
                "broken",
                "creating",
                "killed",
                "no-state"
            ]
        );
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        // While the first cleanup holds the root lock, a second one skips the cleanup.
        assert!(find_stale(&root.0).unwrap().is_none());
    }

    #[test]
    fn goes_on_after_a_directory_that_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses file permissions, so this case cannot be reproduced.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let root = TempRoot::new();
        root.leave("killed", |record| running(record, 1));
        let locked_out = root.0.join("locked-out");
        fs::create_dir(&locked_out).unwrap();
        fs::set_permissions(&locked_out, fs::Permissions::from_mode(0o000)).unwrap();

        let error = remove_stale(&root.0).unwrap_err();

        fs::set_permissions(&locked_out, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(error.to_string().contains("locked-out"), "{error:#}");
        assert!(!root.0.join("killed").exists());
        assert!(locked_out.exists());
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
    fn keeps_a_container_whose_cgroup_holds_processes() {
        let root = TempRoot::new();
        // The runtime could not end the processes before init was recorded.
        let cgroups = TempRoot::new();
        let cgroup = cgroups.0.join("jail_unended");
        fs::create_dir(&cgroup).unwrap();
        fs::write(cgroup.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        root.leave("unended", |record| record.cgroup = cgroup.clone());

        remove_stale(&root.0).unwrap();
        assert!(root.0.join("unended").exists());

        fs::write(cgroup.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        remove_stale(&root.0).unwrap();
        assert!(!root.0.join("unended").exists());
    }

    #[test]
    fn skips_the_cleanup_while_a_state_directory_is_created() {
        let root = TempRoot::new();
        root.leave("creating", |_| {});
        let _creating = lock(File::open(&root.0).unwrap(), LockKind::Shared).unwrap();

        assert!(find_stale(&root.0).unwrap().is_none());
    }

    #[test]
    fn finds_nothing_without_a_root() {
        let root = TempRoot::new();
        assert!(find_stale(&root.0.join("missing")).unwrap().is_none());
    }
}
