use crate::container_id::ContainerId;
use crate::paths;
use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directories that one run uses for its overlay root filesystem.
///
/// The container writes into `upper`, so the rootfs itself never changes.
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    /// Create the run directory of container `id` under `$XDG_DATA_HOME/agent-jail/runs`.
    pub fn create(id: &ContainerId) -> Result<Self> {
        Self::create_in(&paths::data_dir()?.join("runs"), id)
    }

    fn create_in(runs: &Path, id: &ContainerId) -> Result<Self> {
        fs::create_dir_all(runs).with_context(|| format!("create {runs:?}"))?;
        let path = runs.join(id.as_str());
        fs::create_dir(&path).with_context(|| format!("create {path:?}"))?;
        let run_dir = Self { path };
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
        match fs::remove_dir_all(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("remove run directory {:?}", self.path))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;

    #[test]
    fn creates_and_removes_the_overlay_directories() {
        let runs = std::env::temp_dir().join(format!("agent-jail-runs-{:x}", random::<u64>()));
        let run_dir = RunDir::create_in(&runs, &ContainerId::generate()).unwrap();
        for dir in [run_dir.upper(), run_dir.work(), run_dir.merged()] {
            assert!(dir.is_dir(), "{dir:?} is missing");
        }

        run_dir.remove().unwrap();
        assert!(!run_dir.path.exists());
        // A second removal finds nothing to do.
        run_dir.remove().unwrap();
        fs::remove_dir(runs).unwrap();
    }
}
