use crate::container_id::ContainerId;
use crate::flock::{LockKind, lock, try_lock};
use crate::paths;
use crate::user::{Mapping, run_in_user_namespace};
use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// The directories that one run uses for its overlay root filesystem.
///
/// The container writes into `upper`, so the rootfs itself never changes.
pub struct RunDir {
    path: PathBuf,
    // An exclusive flock on the directory shows that the run is alive. The forked processes
    // share this open file, and the kernel releases the lock when the last of them closes it or
    // ends, even when they are killed. `remove_stale` removes only directories whose lock is free.
    _lock: File,
}

impl RunDir {
    /// Create the run directory of container `id` under `$XDG_DATA_HOME/agent-jail/runs`.
    pub fn create(id: &ContainerId) -> Result<Self> {
        Self::create_in(&runs_dir()?, id)
    }

    fn create_in(runs: &Path, id: &ContainerId) -> Result<Self> {
        fs::create_dir_all(runs).with_context(|| format!("create {runs:?}"))?;
        // `remove_stale` locks `runs` exclusively, so it never finds the new directory in the
        // short time before the directory is locked.
        let _creating =
            lock(open(runs)?, LockKind::Shared).with_context(|| format!("lock {runs:?}"))?;
        let path = runs.join(id.as_str());
        match fs::create_dir(&path) {
            // A container with the same ID under another --root has no state directory under
            // this one, so only its run directory shows it.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                bail!("a container with ID {id} already uses the run directory {path:?}")
            }
            result => result.with_context(|| format!("create {path:?}"))?,
        }
        let lock = try_lock(open(&path)?, LockKind::Exclusive)
            .with_context(|| format!("lock {path:?}"))?
            .with_context(|| format!("another process holds the lock of {path:?}"))?;

        let run_dir = Self { path, _lock: lock };
        for dir in [run_dir.upper(), run_dir.work(), run_dir.merged()] {
            fs::create_dir(&dir).with_context(|| format!("create {dir:?}"))?;
        }
        Ok(run_dir)
    }

    /// The overlay's upper directory, which receives every change in the container.
    pub fn upper(&self) -> PathBuf {
        self.path.join("upper")
    }

    /// The overlay's work directory, which must be on the same filesystem as `upper`.
    pub fn work(&self) -> PathBuf {
        self.path.join("work")
    }

    /// The overlay mount point, which becomes the container's root directory.
    pub fn merged(&self) -> PathBuf {
        self.path.join("merged")
    }

    /// Remove the run directory. A directory that is already gone counts as removed.
    ///
    /// Files that container users other than root create belong to subordinate IDs, so only a
    /// process inside the container's user namespace can remove all of them.
    pub fn remove(&self) -> Result<()> {
        remove_dir(&self.path)
    }
}

/// Remove the run directories that no live run holds. A run that is killed, for example with
/// SIGKILL, cannot remove its own directory.
pub fn remove_stale(mapping: &Mapping) -> Result<()> {
    let Some(stale) = find_stale(&runs_dir()?)? else {
        return Ok(());
    };
    if stale.dirs.is_empty() {
        return Ok(());
    }
    run_in_user_namespace(mapping, || {
        let mut result = Ok(());
        // The locks stay held until the removal ends, so another runtime skips these directories.
        for (path, _lock) in &stale.dirs {
            if let Err(error) = remove_dir(path) {
                result = Err(error);
            }
        }
        result
    })
}

/// The run directories that a cleanup found, with the locks that it holds until it ends.
struct StaleRuns {
    // A runtime creates a run directory only while it holds a shared lock on `runs`. While this
    // exclusive lock is held, no runtime can create a directory at the path of one that the
    // cleanup removes.
    _runs_lock: File,
    /// The run directories whose lock was free, each with its exclusive lock.
    dirs: Vec<(PathBuf, File)>,
}

/// Lock and find the run directories whose lock is free. Returns `None` if there is no `runs`,
/// or if a runtime is creating a run directory now.
fn find_stale(runs: &Path) -> Result<Option<StaleRuns>> {
    let runs_file = match File::open(runs) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result.with_context(|| format!("open {runs:?}"))?,
    };
    // Waiting for a runtime that creates its run directory could block for as long as that
    // runtime is stopped, so this run skips the cleanup instead.
    let Some(runs_lock) =
        try_lock(runs_file, LockKind::Exclusive).with_context(|| format!("lock {runs:?}"))?
    else {
        return Ok(None);
    };

    let mut stale = Vec::new();
    for entry in fs::read_dir(runs).with_context(|| format!("read {runs:?}"))? {
        let entry = entry.with_context(|| format!("read {runs:?}"))?;
        let path = entry.path();
        if !paths::is_directory(&entry).with_context(|| format!("read {path:?}"))? {
            continue;
        }
        let dir = match File::open(&path) {
            // The run removed its own directory after read_dir listed it.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            result => result.with_context(|| format!("open {path:?}"))?,
        };
        if let Some(dir) =
            try_lock(dir, LockKind::Exclusive).with_context(|| format!("lock {path:?}"))?
        {
            stale.push((path, dir));
        }
    }
    Ok(Some(StaleRuns {
        _runs_lock: runs_lock,
        dirs: stale,
    }))
}

fn runs_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("runs"))
}

fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("open {path:?}"))
}

fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("remove run directory {path:?}"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;

    struct TempRuns(PathBuf);

    impl TempRuns {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("agent-jail-runs-{:x}", random::<u64>())))
        }

        fn names(stale: &[(PathBuf, File)]) -> Vec<String> {
            let mut names: Vec<String> = stale
                .iter()
                .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempRuns {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_and_removes_the_overlay_directories() {
        let runs = TempRuns::new();
        let id = ContainerId::generate();
        let run_dir = RunDir::create_in(&runs.0, &id).unwrap();
        assert_eq!(run_dir.path, runs.0.join(id.as_str()));
        for dir in [run_dir.upper(), run_dir.work(), run_dir.merged()] {
            assert!(dir.is_dir(), "{dir:?} is missing");
        }

        run_dir.remove().unwrap();
        assert!(!run_dir.path.exists());
        // A second removal finds nothing to do.
        run_dir.remove().unwrap();
    }

    #[test]
    fn finds_only_run_directories_that_no_run_holds() {
        let runs = TempRuns::new();
        let live =
            RunDir::create_in(&runs.0, &ContainerId::parse("aaaaaaaaaaaaaaaa").unwrap()).unwrap();
        fs::create_dir(runs.0.join("bbbbbbbbbbbbbbbb")).unwrap();
        fs::write(runs.0.join("note"), "").unwrap();

        let stale = find_stale(&runs.0).unwrap().unwrap();
        assert_eq!(TempRuns::names(&stale.dirs), ["bbbbbbbbbbbbbbbb"]);
        // While the first cleanup holds the lock of `runs`, a second one skips the cleanup.
        assert!(find_stale(&runs.0).unwrap().is_none());
        drop(stale);

        drop(live);
        let stale = find_stale(&runs.0).unwrap().unwrap();
        assert_eq!(
            TempRuns::names(&stale.dirs),
            ["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb"]
        );
    }

    #[test]
    fn skips_the_cleanup_while_a_run_directory_is_created() {
        let runs = TempRuns::new();
        fs::create_dir_all(runs.0.join("bbbbbbbbbbbbbbbb")).unwrap();
        let _creating = lock(File::open(&runs.0).unwrap(), LockKind::Shared).unwrap();

        assert!(find_stale(&runs.0).unwrap().is_none());
    }

    #[test]
    fn finds_nothing_without_a_runs_directory() {
        let runs = TempRuns::new();
        assert!(find_stale(&runs.0).unwrap().is_none());
    }
}
