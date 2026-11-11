use crate::cgroups;
use crate::container_id::ContainerId;
use crate::flock::{LockKind, lock, try_lock};
use crate::paths;
use crate::user::{Mapping, run_in_user_namespace};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// The file in a run directory that holds the path of the container cgroup.
const CGROUP_FILE: &str = "cgroup";

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
    /// Create the run directory of container `id`, whose cgroup is `cgroup`, under
    /// `$XDG_DATA_HOME/agent-jail/runs`.
    pub fn create(id: &ContainerId, cgroup: &Path) -> Result<Self> {
        Self::create_in(&runs_dir()?, id, cgroup)
    }

    fn create_in(runs: &Path, id: &ContainerId, cgroup: &Path) -> Result<Self> {
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
        let file = run_dir.path.join(CGROUP_FILE);
        fs::write(&file, cgroup.as_os_str().as_bytes())
            .with_context(|| format!("write {file:?}"))?;
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
///
/// Processes of the container may still use such a directory. They are ended first, because no
/// runtime process is left to wait for them or to clean up after them.
pub fn remove_stale(mapping: &Mapping) -> Result<()> {
    let Some(stale) = find_stale(&runs_dir()?)? else {
        return Ok(());
    };
    let mut result = Ok(());
    let mut ended = Vec::new();
    for (path, _lock) in &stale.dirs {
        match end_container(path) {
            Ok(()) => ended.push(path),
            // The directory stays, because processes that did not end still use its overlay.
            Err(error) => result = Err(error),
        }
    }
    if ended.is_empty() {
        return result;
    }
    run_in_user_namespace(mapping, || {
        let mut result = Ok(());
        // The locks stay held until the removal ends, so another runtime skips these directories.
        for path in ended {
            if let Err(error) = remove_dir(path) {
                result = Err(error);
            }
        }
        result
    })?;
    result
}

/// End the processes of the container whose run directory is `dir`.
///
/// Container init ends when its setup process ends, unless the program clears its parent death
/// signal, for example by changing its user or by running a set-user-ID program.
fn end_container(dir: &Path) -> Result<()> {
    let file = dir.join(CGROUP_FILE);
    let cgroup = match fs::read(&file) {
        // A run writes the file before it starts any process in the container.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => PathBuf::from(OsString::from_vec(
            result.with_context(|| format!("read {file:?}"))?,
        )),
    };
    if !cgroups::holds_processes(&cgroup)? {
        return Ok(());
    }
    cgroups::kill_all(&cgroup).with_context(|| format!("end the container of {dir:?}"))
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
        let cgroup = Path::new("/sys/fs/cgroup/app.slice/jail_x");
        let run_dir = RunDir::create_in(&runs.0, &id, cgroup).unwrap();
        assert_eq!(run_dir.path, runs.0.join(id.as_str()));
        for dir in [run_dir.upper(), run_dir.work(), run_dir.merged()] {
            assert!(dir.is_dir(), "{dir:?} is missing");
        }
        assert_eq!(
            fs::read_to_string(run_dir.path.join(CGROUP_FILE)).unwrap(),
            cgroup.to_str().unwrap()
        );

        run_dir.remove().unwrap();
        assert!(!run_dir.path.exists());
        // A second removal finds nothing to do.
        run_dir.remove().unwrap();
    }

    #[test]
    fn finds_only_run_directories_that_no_run_holds() {
        let runs = TempRuns::new();
        let live = RunDir::create_in(
            &runs.0,
            &ContainerId::parse("aaaaaaaaaaaaaaaa").unwrap(),
            Path::new("/no/cgroup"),
        )
        .unwrap();
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
    fn ends_only_a_container_whose_cgroup_holds_processes() {
        let runs = TempRuns::new();
        let dir = runs.0.join("box");
        fs::create_dir_all(&dir).unwrap();
        // A run that was killed before it wrote the file.
        end_container(&dir).unwrap();

        let cgroup = runs.0.join("jail_box");
        fs::create_dir(&cgroup).unwrap();
        fs::write(cgroup.join("cgroup.kill"), "").unwrap();
        fs::write(cgroup.join("cgroup.events"), "populated 0\n").unwrap();
        fs::write(dir.join(CGROUP_FILE), cgroup.to_str().unwrap()).unwrap();
        end_container(&dir).unwrap();
        assert_eq!(fs::read_to_string(cgroup.join("cgroup.kill")).unwrap(), "");

        fs::remove_dir_all(&cgroup).unwrap();
        end_container(&dir).unwrap();
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
