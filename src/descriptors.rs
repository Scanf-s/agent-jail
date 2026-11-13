//! The descriptors that the runtime keeps from its caller and from the container program.

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::libc;
use std::fs;
use std::os::fd::RawFd;

/// The first descriptor after stdin, stdout, and stderr.
const FIRST_OTHER: libc::c_uint = 3;

/// Close every descriptor that the caller left open, except stdin, stdout, and stderr.
///
/// The runtime processes of a container live until it ends, after `aj create` returned. A
/// descriptor that they kept could hold a file lock of the caller, keep a pipe of the caller
/// from reaching its end, or keep a file system busy. pasta would also receive it.
///
/// Call this before the runtime opens a descriptor.
pub fn close_inherited() -> Result<()> {
    // SAFETY: the runtime has opened no descriptor yet, so nothing uses one that this closes.
    let result = unsafe { libc::syscall(libc::SYS_close_range, FIRST_OTHER, libc::c_uint::MAX, 0) };
    match Errno::result(result) {
        Ok(_) => Ok(()),
        // Linux 5.9 added close_range.
        Err(Errno::ENOSYS) => {
            for fd in open_descriptors(FIRST_OTHER)? {
                // SAFETY: as above. A descriptor that is not open any more, such as the one
                // of the directory that open_descriptors read, makes close fail harmlessly.
                unsafe { libc::close(fd) };
            }
            Ok(())
        }
        Err(errno) => Err(errno).context("close_range"),
    }
}

/// Mark every descriptor except stdin, stdout, and stderr close-on-exec.
///
/// A descriptor that refers to a host directory would let the program open host files outside
/// its root directory. The runtime closes the descriptors of its caller when it starts, and
/// this also keeps a descriptor that the runtime opened without close-on-exec from the program.
pub fn close_others_on_exec() -> Result<()> {
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only sets descriptor flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            FIRST_OTHER,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    match Errno::result(result) {
        Ok(_) => Ok(()),
        // Linux 5.9 added close_range, and Linux 5.11 added CLOSE_RANGE_CLOEXEC.
        Err(Errno::ENOSYS | Errno::EINVAL) => close_listed_on_exec(FIRST_OTHER),
        Err(errno) => Err(errno).context("close_range"),
    }
}

/// Like `close_others_on_exec`, for a kernel without CLOSE_RANGE_CLOEXEC.
fn close_listed_on_exec(first: libc::c_uint) -> Result<()> {
    for fd in open_descriptors(first)? {
        match fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)) {
            // read_dir has closed the descriptor of the directory that it read.
            Ok(_) | Err(Errno::EBADF) => {}
            Err(errno) => return Err(errno).with_context(|| format!("set FD_CLOEXEC on {fd}")),
        }
    }
    Ok(())
}

/// The descriptors from `first` on that `/proc/self/fd` lists.
fn open_descriptors(first: libc::c_uint) -> Result<Vec<RawFd>> {
    let mut fds = Vec::new();
    for entry in fs::read_dir("/proc/self/fd").context("read /proc/self/fd")? {
        let name = entry.context("read /proc/self/fd")?.file_name();
        let fd: RawFd = name
            .to_str()
            .and_then(|name| name.parse().ok())
            .with_context(|| format!("parse descriptor {name:?}"))?;
        if libc::c_uint::try_from(fd).is_ok_and(|fd| fd >= first) {
            fds.push(fd);
        }
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;
    use nix::unistd::close;

    fn has_cloexec(fd: RawFd) -> bool {
        let flags = fcntl(fd, FcntlArg::F_GETFD).unwrap();
        FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)
    }

    // The test process keeps its other descriptors, because only an exec closes them.
    #[test]
    fn marks_listed_descriptors_close_on_exec() {
        let fd = open("/dev/null", OFlag::O_RDONLY, Mode::empty()).unwrap();
        assert!(!has_cloexec(fd));

        close_listed_on_exec(FIRST_OTHER).unwrap();

        assert!(has_cloexec(fd));
        close(fd).unwrap();
    }
}
