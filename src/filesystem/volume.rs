use crate::config::Volume;
use anyhow::{Context, Result};
use nix::libc;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A copy of a volume's host mount that is not attached anywhere yet.
pub struct DetachedVolume {
    tree: OwnedFd,
    destination: PathBuf,
    is_dir: bool,
}

/// Copy the mount of each volume source while host paths are still visible.
///
/// Call this before pivot_root, and attach the copies with `attach_volumes` after it.
pub fn detach_volumes(volumes: &[Volume]) -> Result<Vec<DetachedVolume>> {
    volumes
        .iter()
        .map(|volume| {
            let source = &volume.source;
            let is_dir = fs::metadata(source)
                .with_context(|| format!("read volume source {source:?}"))?
                .is_dir();
            let tree = open_tree_copy(source)
                .with_context(|| format!("copy the mount of volume source {source:?}"))?;
            if volume.read_only {
                set_read_only(&tree)
                    .with_context(|| format!("make volume {source:?} read-only"))?;
            }
            Ok(DetachedVolume {
                tree,
                destination: volume.destination.clone(),
                is_dir,
            })
        })
        .collect()
}

/// Attach the copies at their destinations inside the container.
///
/// Call this after pivot_root. Then "/" is the container's root, so a symbolic link in the
/// rootfs resolves inside the container and cannot point a mount at a host path.
pub fn attach_volumes(volumes: Vec<DetachedVolume>) -> Result<()> {
    for volume in volumes {
        let destination = &volume.destination;
        create_mount_point(destination, volume.is_dir)
            .with_context(|| format!("create volume mount point {destination:?}"))?;
        attach(&volume.tree, destination)
            .with_context(|| format!("mount volume on {destination:?}"))?;
    }
    Ok(())
}

fn create_mount_point(destination: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        return fs::create_dir_all(destination);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(destination)
        .map(drop)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn check(result: libc::c_long) -> io::Result<libc::c_long> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// open_tree with OPEN_TREE_CLONE: a recursive copy of the mount at `path`.
fn open_tree_copy(path: &Path) -> io::Result<OwnedFd> {
    let path = c_path(path)?;
    let flags =
        libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | libc::AT_RECURSIVE as libc::c_uint;
    // SAFETY: `path` is a valid C string that outlives the call.
    let fd =
        check(unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, path.as_ptr(), flags) })?;
    // SAFETY: open_tree returned a new file descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

fn set_read_only(tree: &OwnedFd) -> io::Result<()> {
    // SAFETY: mount_attr is plain data, and zero means "change nothing" for every field.
    let mut attr: libc::mount_attr = unsafe { std::mem::zeroed() };
    attr.attr_set = libc::MOUNT_ATTR_RDONLY;
    // SAFETY: the empty path and `attr` are valid for the duration of the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_RECURSIVE,
            &attr as *const libc::mount_attr,
            size_of::<libc::mount_attr>(),
        )
    })?;
    Ok(())
}

fn attach(tree: &OwnedFd, destination: &Path) -> io::Result<()> {
    let destination = c_path(destination)?;
    // SAFETY: both paths are valid C strings that outlive the call.
    check(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })?;
    Ok(())
}
