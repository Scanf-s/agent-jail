use super::cgroup::{check_delegated, own_cgroup};
use crate::container_id::ContainerId;
use anyhow::{Context, Result, bail};
use nix::unistd::Pid;
use rand::random;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// The scope name shows that the runtime started it, so a second failure stops instead of looping.
// It also carries the container ID across the exec into the scope.
const SCOPE_PREFIX: &str = "agent-jail-";
// systemd-run guesses the unit type from the end of a name that has no type. With an explicit
// type, the name never depends on that guess, whatever characters the ID contains.
const SCOPE_SUFFIX: &str = ".scope";

/// Return the delegated cgroup that the runtime runs in, and the ID of the container:
/// `requested`, or a new ID without it.
///
/// If the runtime cannot use its current cgroup, this replaces the process with the same
/// command in a new systemd scope that is delegated to this user, and does not return.
pub fn enter_delegated_cgroup(requested: Option<&ContainerId>) -> Result<(PathBuf, ContainerId)> {
    let own = own_cgroup()?;
    let scope_id = container_id_of_scope(&own);
    let Err(reason) = check_delegated(&own, Pid::this()) else {
        return Ok((own, choose_id(requested, scope_id)?));
    };
    if scope_id.is_some() {
        return Err(reason).context("the systemd scope that the runtime created is not usable");
    }

    let exe = std::env::current_exe().context("find the runtime executable")?;
    let id = requested.cloned().unwrap_or_else(ContainerId::generate);
    let unit = unit_name(&id);
    let error = scope_command(&exe, std::env::args_os().skip(1), &unit).exec();
    Err(error).context(format!(
        "run the runtime again through systemd-run, because {reason:#}"
    ))
}

/// The ID of a container in the delegated cgroup: the requested ID, the ID in the name of the
/// scope that the runtime created, or a new ID.
fn choose_id(
    requested: Option<&ContainerId>,
    scope_id: Option<ContainerId>,
) -> Result<ContainerId> {
    match (requested, scope_id) {
        (Some(requested), Some(scope_id)) if *requested != scope_id => {
            bail!("the systemd scope belongs to container {scope_id}, not to {requested}")
        }
        (Some(requested), _) => Ok(requested.clone()),
        (None, Some(scope_id)) => Ok(scope_id),
        (None, None) => Ok(ContainerId::generate()),
    }
}

/// A new name for a scope unit of container `id`: `agent-jail-<id>-<8 hex digits>.scope`.
///
/// systemd keeps a unit for a short time after its processes end. A name made from the ID
/// alone would make a run fail when it uses the ID of a run that just ended. The state
/// directory, not the unit name, keeps two containers from using the same ID.
fn unit_name(id: &ContainerId) -> String {
    format!("{SCOPE_PREFIX}{id}-{:08x}{SCOPE_SUFFIX}", random::<u32>())
}

/// The container ID in the name of a scope that the runtime created.
fn container_id_of_scope(cgroup: &Path) -> Option<ContainerId> {
    let name = cgroup.file_name()?.to_str()?;
    let name = name
        .strip_prefix(SCOPE_PREFIX)?
        .strip_suffix(SCOPE_SUFFIX)?;
    // An ID can contain '-', but the random part after the last '-' cannot.
    let (id, random_part) = name.rsplit_once('-')?;
    let is_random_part = random_part.len() == 8
        && random_part
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !is_random_part {
        return None;
    }
    ContainerId::parse(id).ok()
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
        for id in ["0123456789abcdef", "web.service", "box", "my-box-2"] {
            let id = ContainerId::parse(id).unwrap();
            let unit = unit_name(&id);
            assert!(unit.starts_with(&format!("agent-jail-{id}-")), "{unit}");
            assert!(unit.ends_with(".scope"), "{unit}");
            let cgroup = Path::new("/sys/fs/cgroup/app.slice").join(&unit);
            assert_eq!(container_id_of_scope(&cgroup), Some(id));
        }
    }

    #[test]
    fn names_each_scope_of_one_id_differently() {
        let id = ContainerId::parse("box").unwrap();
        assert_ne!(unit_name(&id), unit_name(&id));
    }

    #[test]
    fn prefers_the_requested_id_but_rejects_another_scope() {
        let id = |value| ContainerId::parse(value).unwrap();

        assert_eq!(choose_id(Some(&id("box")), None).unwrap(), id("box"));
        assert_eq!(
            choose_id(Some(&id("box")), Some(id("box"))).unwrap(),
            id("box")
        );
        assert_eq!(choose_id(None, Some(id("box"))).unwrap(), id("box"));
        assert_eq!(choose_id(None, None).unwrap().as_str().len(), 16);

        let error = choose_id(Some(&id("box")), Some(id("other"))).unwrap_err();
        assert!(
            error.to_string().contains("belongs to container other"),
            "{error:#}"
        );
    }

    #[test]
    fn ignores_scopes_that_the_runtime_did_not_create() {
        for other in [
            "/sys/fs/cgroup/init.scope",
            "/sys/fs/cgroup/app.slice/run-1a2b.scope",
            "/sys/fs/cgroup/app.slice/agent-jail-1a2b-0123abcd.service",
            "/sys/fs/cgroup/app.slice/agent-jail-1a2b.scope",
            "/sys/fs/cgroup/app.slice/agent-jail-1a2b-0123ABCD.scope",
            "/sys/fs/cgroup/app.slice/agent-jail-1a2b-0123abc.scope",
            "/sys/fs/cgroup/app.slice/agent-jail-..-0123abcd.scope",
            "/sys/fs/cgroup/app.slice/agent-jail--0123abcd.scope",
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
