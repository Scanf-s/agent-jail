use super::cgroup::{check_delegated, own_cgroup};
use crate::container_id::ContainerId;
use anyhow::{Context, Result};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// The scope name shows that the runtime started it, so a second failure stops instead of looping.
// It also carries the container ID across the exec into the scope.
const SCOPE_PREFIX: &str = "agent-jail-";
// systemd-run adds this only to a name without a unit type suffix. Without it, an ID such as
// `web.service` would name a service unit.
const SCOPE_SUFFIX: &str = ".scope";

/// Return the delegated cgroup that the runtime runs in, and the ID of the container.
///
/// If the runtime cannot use its current cgroup, this replaces the process with the same
/// command in a new systemd scope that is delegated to this user, and does not return.
pub fn enter_delegated_cgroup() -> Result<(PathBuf, ContainerId)> {
    let own = own_cgroup()?;
    let scope_id = container_id_of_scope(&own);
    let Err(reason) = check_delegated(&own, Pid::this()) else {
        return Ok((own, scope_id.unwrap_or_else(ContainerId::generate)));
    };
    if scope_id.is_some() {
        return Err(reason).context("the systemd scope that the runtime created is not usable");
    }

    let exe = std::env::current_exe().context("find the runtime executable")?;
    let unit = unit_name(&ContainerId::generate());
    let error = scope_command(&exe, std::env::args_os().skip(1), &unit).exec();
    Err(error).context(format!(
        "run the runtime again through systemd-run, because {reason:#}"
    ))
}

/// The name of the scope unit for container `id`.
fn unit_name(id: &ContainerId) -> String {
    format!("{SCOPE_PREFIX}{id}{SCOPE_SUFFIX}")
}

/// The container ID in the name of a scope that the runtime created.
fn container_id_of_scope(cgroup: &Path) -> Option<ContainerId> {
    let name = cgroup.file_name()?.to_str()?;
    ContainerId::parse(
        name.strip_prefix(SCOPE_PREFIX)?
            .strip_suffix(SCOPE_SUFFIX)?,
    )
    .ok()
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
    fn names_a_scope_unit_for_any_valid_id() {
        for id in ["0123456789abcdef", "web.service", "box"] {
            let id = ContainerId::parse(id).unwrap();
            let unit = unit_name(&id);
            assert_eq!(unit, format!("agent-jail-{id}.scope"));
            let cgroup = Path::new("/sys/fs/cgroup/app.slice").join(&unit);
            assert_eq!(container_id_of_scope(&cgroup), Some(id));
        }
    }

    #[test]
    fn ignores_scopes_that_the_runtime_did_not_create() {
        for other in [
            "/sys/fs/cgroup/init.scope",
            "/sys/fs/cgroup/app.slice/run-1a2b.scope",
            "/sys/fs/cgroup/app.slice/agent-jail-1a2b.service",
            "/sys/fs/cgroup/app.slice/agent-jail-..scope",
            "/sys/fs/cgroup/app.slice/agent-jail-.scope",
        ] {
            assert_eq!(container_id_of_scope(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn passes_the_original_arguments_after_the_separator() {
        let args = ["run", "./rootfs", "/bin/sh", "-c", "echo --unit=x"].map(OsString::from);
        let command = scope_command(Path::new("/opt/runtime"), args, "agent-jail-1");

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
                "--unit=agent-jail-1",
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
