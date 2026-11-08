use super::cgroup::{check_delegated, own_cgroup};
use anyhow::{Context, Result};
use nix::unistd::Pid;
use rand::random;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// The scope name shows that the runtime started it, so a second failure stops instead of looping.
const SCOPE_PREFIX: &str = "container-runtime-";

/// Return the delegated cgroup that the runtime runs in.
///
/// If the runtime cannot use its current cgroup, this replaces the process with the same
/// command in a new systemd scope that is delegated to this user, and does not return.
pub fn enter_delegated_cgroup() -> Result<PathBuf> {
    let own = own_cgroup()?;
    let Err(reason) = check_delegated(&own, Pid::this()) else {
        return Ok(own);
    };
    if started_by_runtime(&own) {
        return Err(reason).context("the systemd scope that the runtime created is not usable");
    }

    let exe = std::env::current_exe().context("find the runtime executable")?;
    let unit = format!("{SCOPE_PREFIX}{:x}", random::<u64>());
    let error = scope_command(&exe, std::env::args_os().skip(1), &unit).exec();
    Err(error).context(format!(
        "run the runtime again through systemd-run, because {reason:#}"
    ))
}

fn started_by_runtime(cgroup: &Path) -> bool {
    cgroup
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(SCOPE_PREFIX) && name.ends_with(".scope"))
}

/// Build a command that runs `exe` with `args` in a new scope named `unit`.
///
/// `systemd-run --scope` replaces itself with the command, so the working directory, standard
/// streams, environment, and exit code stay the same.
fn scope_command(exe: &Path, args: impl IntoIterator<Item = OsString>, unit: &str) -> Command {
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args(["--property", "Delegate=yes"])
        .arg(format!("--unit={unit}"))
        .arg("--")
        .arg(exe)
        .args(args);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_a_scope_that_the_runtime_created() {
        let ours = Path::new("/sys/fs/cgroup/app.slice/container-runtime-1a2b.scope");
        assert!(started_by_runtime(ours));
        assert!(!started_by_runtime(Path::new("/sys/fs/cgroup/init.scope")));
        assert!(!started_by_runtime(Path::new(
            "/sys/fs/cgroup/app.slice/run-1a2b.scope"
        )));
    }

    #[test]
    fn passes_the_original_arguments_after_the_separator() {
        let args = ["run", "./rootfs", "/bin/sh", "-c", "echo --unit=x"].map(OsString::from);
        let command = scope_command(Path::new("/opt/runtime"), args, "container-runtime-1");

        assert_eq!(command.get_program(), "systemd-run");
        let actual: Vec<_> = command.get_args().collect();
        assert_eq!(
            actual,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--property",
                "Delegate=yes",
                "--unit=container-runtime-1",
                "--",
                "/opt/runtime",
                "run",
                "./rootfs",
                "/bin/sh",
                "-c",
                "echo --unit=x",
            ]
        );
    }
}
