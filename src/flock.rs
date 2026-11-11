use nix::errno::Errno;
use nix::libc;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// The kind of flock lock. Many open files can hold a shared lock at the same time, but an
/// exclusive lock excludes every other lock.
#[derive(Debug, Clone, Copy)]
pub enum LockKind {
    Shared,
    Exclusive,
}

impl LockKind {
    fn operation(self) -> libc::c_int {
        match self {
            Self::Shared => libc::LOCK_SH,
            Self::Exclusive => libc::LOCK_EX,
        }
    }
}

/// Lock `file`, and wait while another open file holds a conflicting lock.
pub fn lock(file: File, kind: LockKind) -> io::Result<File> {
    flock(file, kind.operation())
}

/// Lock `file` without waiting. Returns `None` if another open file holds a conflicting lock.
pub fn try_lock(file: File, kind: LockKind) -> io::Result<Option<File>> {
    match flock(file, kind.operation() | libc::LOCK_NB) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        result => result.map(Some),
    }
}

/// Apply flock `operation` to `file`.
///
/// `nix::fcntl::Flock` calls LOCK_UN when it is dropped. That would release the lock for every
/// forked process that shares the open file. Without that call, the kernel releases the lock
/// only when the last descriptor that refers to the open file is closed.
fn flock(file: File, operation: libc::c_int) -> io::Result<File> {
    loop {
        // SAFETY: flock only uses the descriptor number, and `file` keeps the descriptor open.
        match Errno::result(unsafe { libc::flock(file.as_raw_fd(), operation) }) {
            Ok(_) => return Ok(file),
            Err(Errno::EINTR) => continue,
            Err(errno) => return Err(errno.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_locks_exclude_only_exclusive_locks() {
        let path =
            std::env::temp_dir().join(format!("agent-jail-flock-{:x}", rand::random::<u64>()));
        std::fs::write(&path, "").unwrap();
        let open = || File::open(&path).unwrap();

        let shared = lock(open(), LockKind::Shared).unwrap();
        assert!(try_lock(open(), LockKind::Shared).unwrap().is_some());
        assert!(try_lock(open(), LockKind::Exclusive).unwrap().is_none());
        drop(shared);

        let exclusive = try_lock(open(), LockKind::Exclusive).unwrap();
        assert!(exclusive.is_some());
        assert!(try_lock(open(), LockKind::Shared).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }
}
