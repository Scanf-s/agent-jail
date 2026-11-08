use anyhow::{Context, Result, bail};
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
    pub rootfs: PathBuf,
}

impl PivotRoot {
    pub fn new(rootfs: PathBuf) -> Self {
        Self { rootfs }
    }

    /// Isolate the filesystem by pivoting into `rootfs`, then detach the old root.
    pub fn isolate_filesystem(&self) -> Result<()> {
        self.validate_rootfs()?;

        // Create new mount table for this new process
        self.set_private_mount_namespace()?;

        // Prepare a new mount point of rootfs to pass it on the pivot_root
        mount::<_, _, str, str>(
            Some(self.rootfs.as_os_str()),
            self.rootfs.as_os_str(),
            None,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None,
        )
        .with_context(|| format!("bind mount {:?} onto itself", self.rootfs.as_os_str()))?;

        // Mount necessary virtual filesystems
        self.mount_proc()?;
        // TODO: ...

        // Isolate filesystem with new configured mount table using pivot_root
        self.pivot_root()?;

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
        let proc_path = self.rootfs.join("proc");
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
        // Opened after the bind mount, so the handle refers to the new mount point.
        let new_root = open_dir_path(&self.rootfs)?;

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

    #[test]
    fn rejects_missing_rootfs_before_mount_operations() {
        let rootfs = std::env::temp_dir().join(format!("missing-rootfs-{:x}", random::<u64>()));
        let error = PivotRoot::new(rootfs.clone())
            .isolate_filesystem()
            .unwrap_err();
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
        let error = PivotRoot::new(path.clone())
            .isolate_filesystem()
            .unwrap_err();
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

        let error = PivotRoot::new(rootfs).validate_rootfs().unwrap_err();

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
        assert!(PivotRoot::new(path.clone()).validate_rootfs().is_ok());
        fs::remove_dir(path).unwrap();
    }
}
