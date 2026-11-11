use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::libc;
use nix::unistd::execvpe;
use std::ffi::CString;
use std::fs;
use std::os::fd::RawFd;

/// Replace the current process with `args[0]`, passing `args` as argv and `env` as the environment.
/// Returns only on failure.
///
/// The program receives only stdin, stdout, and stderr. Other descriptors stay open until the
/// exec succeeds, so the caller can still report a failure through them.
///
/// A program name without '/' is searched in the runtime's own PATH, not in the PATH from `env`,
/// because libc's execvpe reads PATH from the calling process.
pub fn exec(args: &[String], env: &[String]) -> Result<()> {
    let program = args.first().context("no program to execute")?;
    let c_args = to_cstrings(args).context("argument contains a nul byte")?;
    let c_env = to_cstrings(env).context("environment variable contains a nul byte")?;
    close_on_exec_from(3).context("keep descriptors from the program")?;

    let Err(error) = execvpe(&c_args[0], &c_args, &c_env);
    Err(error).with_context(|| format!("exec {program:?}"))
}

fn to_cstrings(values: &[String]) -> Result<Vec<CString>, std::ffi::NulError> {
    values
        .iter()
        .map(|value| CString::new(value.as_str()))
        .collect()
}

/// Mark every descriptor from `first` on close-on-exec.
///
/// The runtime inherits descriptors that its caller left open. One that refers to a host
/// directory would let the program open host files outside its root directory.
fn close_on_exec_from(first: RawFd) -> Result<()> {
    let first = libc::c_uint::try_from(first).context("negative descriptor")?;
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only sets descriptor flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    match Errno::result(result) {
        Ok(_) => Ok(()),
        // Linux 5.9 added close_range, and Linux 5.11 added CLOSE_RANGE_CLOEXEC.
        Err(Errno::ENOSYS | Errno::EINVAL) => close_listed_on_exec(first),
        Err(errno) => Err(errno).context("close_range"),
    }
}

/// Like `close_on_exec_from`, for a kernel without CLOSE_RANGE_CLOEXEC.
fn close_listed_on_exec(first: libc::c_uint) -> Result<()> {
    let mut fds = Vec::new();
    for entry in fs::read_dir("/proc/self/fd").context("read /proc/self/fd")? {
        let name = entry.context("read /proc/self/fd")?.file_name();
        let fd: RawFd = name
            .to_str()
            .and_then(|name| name.parse().ok())
            .with_context(|| format!("parse descriptor {name:?}"))?;
        fds.push(fd);
    }
    for fd in fds {
        if libc::c_uint::try_from(fd).is_ok_and(|fd| fd < first) {
            continue;
        }
        match fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)) {
            // read_dir has closed the descriptor of the directory that it read.
            Ok(_) | Err(Errno::EBADF) => {}
            Err(errno) => return Err(errno).with_context(|| format!("set FD_CLOEXEC on {fd}")),
        }
    }
    Ok(())
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

        close_listed_on_exec(3).unwrap();

        assert!(has_cloexec(fd));
        close(fd).unwrap();
    }
}
