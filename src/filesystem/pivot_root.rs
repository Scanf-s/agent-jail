use super::RunDir;
use super::dev::prepare_dev;
use super::volume::{attach_volumes, detach_volumes};
use crate::config::Volume;
use anyhow::{Context, Result, bail, ensure};
use nix::fcntl::OFlag;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::unistd::{chdir, fchdir, pivot_root};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct PivotRoot {
    /// The overlay's read-only lower directory.
    pub rootfs: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    /// The overlay mount point, which becomes the new root.
    merged: PathBuf,
    volumes: Vec<Volume>,
}

impl PivotRoot {
    pub fn new(rootfs: PathBuf, run_dir: &RunDir, volumes: &[Volume]) -> Self {
        Self {
            rootfs,
            upper: run_dir.upper(),
            work: run_dir.work(),
            merged: run_dir.merged(),
            volumes: volumes.to_vec(),
        }
    }

    /// Isolate the filesystem by pivoting into an overlay of `rootfs`, then detach the old root.
    pub fn isolate_filesystem(&self) -> Result<()> {
        self.validate_rootfs()?;

        // Create new mount table for this new process
        self.set_private_mount_namespace()?;

        // The overlay is a mount point of its own, as pivot_root requires.
        self.mount_overlay()?;

        // Mount necessary virtual filesystems
        self.mount_proc()?;
        prepare_dev(&self.merged)?;
        // Host paths are visible only before pivot_root, and destinations must be resolved
        // only after it.
        let volumes = detach_volumes(&self.volumes)?;

        // Isolate filesystem with new configured mount table using pivot_root
        self.pivot_root()?;
        attach_volumes(volumes)
    }

    fn mount_overlay(&self) -> Result<()> {
        let lower = std::path::absolute(&self.rootfs)
            .with_context(|| format!("resolve rootfs {:?}", self.rootfs))?;
        let options = overlay_options(&lower, &self.upper, &self.work)?;
        mount(
            Some("overlay"),
            &self.merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(options.as_str()),
        )
        .with_context(|| format!("mount overlay on {:?} with {options}", self.merged))?;
        Ok(())
    }

    fn validate_rootfs(&self) -> Result<()> {
        // Container init runs in a new user namespace, where root's capabilities do not apply to
        // directories with an unmapped owner. Only their permission bits for other users count.
        if let Err(error) = fs::metadata(&self.rootfs)
            && error.kind() == io::ErrorKind::PermissionDenied
        {
            bail!(
                "cannot reach rootfs {:?} from the container user namespace: \
                 every directory above it must allow search by other users ({error})",
                self.rootfs
            );
        }
        if !self.rootfs.is_dir() {
            bail!(
                "rootfs {:?} does not exist or is not a directory",
                self.rootfs
            );
        }
        Ok(())
    }

    // Create new namespace and mount table for the new process
    fn set_private_mount_namespace(&self) -> Result<()> {
        unshare(CloneFlags::CLONE_NEWNS).context("unshare(CLONE_NEWNS)")?;
        mount::<str, _, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)
            .context("mount / MS_REC|MS_PRIVATE")?;
        Ok(())
    }

    // create and mount new proc virtual filesystem
    fn mount_proc(&self) -> Result<()> {
        let proc_path = self.merged.join("proc");
        fs::create_dir_all(&proc_path)
            .with_context(|| format!("create {}", proc_path.display()))?;
        mount::<_, _, _, str>(
            Some("proc"),
            proc_path.as_path(),
            Some("proc"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None,
        )
        .with_context(|| format!("mount proc on {:?}", proc_path))?;
        Ok(())
    }

    fn pivot_root(&self) -> Result<()> {
        // After pivot_root, "/" no longer reaches the old root, so keep a handle to it.
        let old_root = open_dir_path(Path::new("/"))?;
        // Opened after the overlay mount, so the handle refers to the new mount point.
        let new_root = open_dir_path(&self.merged)?;

        // With "." for both arguments, the old root is stacked on top of the new root,
        // so rootfs needs no directory to receive it.
        fchdir(new_root.as_raw_fd()).context("fchdir to new root")?;
        pivot_root(".", ".").context("pivot_root(\".\", \".\")")?;

        // The kernel does not guarantee the working directory after pivot_root.
        fchdir(old_root.as_raw_fd()).context("fchdir to old root")?;
        umount2(".", MntFlags::MNT_DETACH).context("detach old root")?;

        chdir("/").context("chdir(\"/\") after pivot_root")?;
        Ok(())
    }
}

/// Build the overlay mount options.
///
/// The kernel splits mount options at commas, and overlayfs splits directory lists at colons
/// and treats backslashes as escapes, so paths with these characters cannot be passed.
fn overlay_options(lower: &Path, upper: &Path, work: &Path) -> Result<String> {
    let mut paths = Vec::new();
    for path in [lower, upper, work] {
        let text = path
            .to_str()
            .with_context(|| format!("overlay path {path:?} is not valid UTF-8"))?;
        ensure!(
            !text.contains([',', ':', '\\']),
            "overlay path {path:?} contains ',', ':', or '\\', which overlay mount options cannot hold"
        );
        paths.push(text);
    }
    Ok(format!(
        "lowerdir={},upperdir={},workdir={}",
        paths[0], paths[1], paths[2]
    ))
}

/// Open a directory only to refer to it, which needs no read permission.
fn open_dir_path(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_PATH | OFlag::O_DIRECTORY).bits())
        .open(path)
        .with_context(|| format!("open {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;
    use std::os::unix::fs::PermissionsExt;

    // Checks that fail before any mount do not need a real run directory.
    fn pivot(rootfs: PathBuf) -> PivotRoot {
        PivotRoot {
            rootfs,
            upper: PathBuf::from("/nonexistent/upper"),
            work: PathBuf::from("/nonexistent/work"),
            merged: PathBuf::from("/nonexistent/merged"),
            volumes: Vec::new(),
        }
    }

    #[test]
    fn builds_overlay_options_and_rejects_special_characters() {
        let options = overlay_options(
            Path::new("/images/alpine"),
            Path::new("/runs/1/upper"),
            Path::new("/runs/1/work"),
        )
        .unwrap();
        assert_eq!(
            options,
            "lowerdir=/images/alpine,upperdir=/runs/1/upper,workdir=/runs/1/work"
        );

        for lower in ["/images/a,b", "/images/a:b", "/images/a\\b"] {
            let error = overlay_options(Path::new(lower), Path::new("/u"), Path::new("/w"));
            assert!(error.is_err(), "accepted {lower}");
        }
    }

    #[test]
    fn rejects_missing_rootfs_before_mount_operations() {
        let rootfs = std::env::temp_dir().join(format!("missing-rootfs-{:x}", random::<u64>()));
        let error = pivot(rootfs.clone()).isolate_filesystem().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not exist or is not a directory")
        );
        assert!(!rootfs.exists());
    }

    #[test]
    fn rejects_regular_file_as_rootfs() {
        let path = std::env::temp_dir().join(format!("rootfs-file-{:x}", random::<u64>()));
        fs::write(&path, "not a directory").unwrap();
        let error = pivot(path.clone()).isolate_filesystem().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not exist or is not a directory")
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn explains_a_rootfs_that_cannot_be_reached() {
        // Root bypasses directory permissions on the host, so this case cannot be reproduced.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let parent = std::env::temp_dir().join(format!("rootfs-parent-{:x}", random::<u64>()));
        let rootfs = parent.join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        // Without the execute bit, nothing can search the parent directory.
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o600)).unwrap();

        let error = pivot(rootfs).validate_rootfs().unwrap_err();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&parent).unwrap();
        assert!(
            error.to_string().contains("cannot reach rootfs"),
            "{error:#}"
        );
    }

    #[test]
    fn accepts_an_existing_directory_as_rootfs() {
        let path = std::env::temp_dir().join(format!("rootfs-dir-{:x}", random::<u64>()));
        fs::create_dir(&path).unwrap();
        assert!(pivot(path.clone()).validate_rootfs().is_ok());
        fs::remove_dir(path).unwrap();
    }
}
