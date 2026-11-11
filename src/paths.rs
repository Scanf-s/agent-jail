use anyhow::{Context, Result, ensure};
use nix::unistd::Uid;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The directory that holds one state directory for each container: `requested` from
/// `--root`, or `$XDG_RUNTIME_DIR/agent-jail`.
pub fn state_root(requested: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = requested {
        return std::path::absolute(root).with_context(|| format!("resolve --root {root:?}"));
    }
    let runtime_dir =
        runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR"), Uid::current(), |dir| {
            dir.is_dir()
        })?;
    Ok(runtime_dir.join("agent-jail"))
}

fn runtime_dir_from(
    xdg_runtime_dir: Option<OsString>,
    uid: Uid,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    // The XDG Base Directory specification says to ignore a relative path.
    if let Some(dir) = xdg_runtime_dir.map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir);
    }
    // systemd creates this directory for each user who logs in, and sets XDG_RUNTIME_DIR to it.
    let fallback = PathBuf::from(format!("/run/user/{uid}"));
    ensure!(
        is_dir(&fallback),
        "XDG_RUNTIME_DIR is not set and {fallback:?} does not exist; choose a state directory with --root"
    );
    Ok(fallback)
}

/// The directory that holds agent-jail's images and runs, `$XDG_DATA_HOME/agent-jail`.
pub fn data_dir() -> Result<PathBuf> {
    data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// The rootfs that `run` uses unless `--rootfs` names another one.
pub fn default_rootfs() -> Result<PathBuf> {
    Ok(data_dir()?.join("images/default"))
}

fn data_dir_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    // The XDG Base Directory specification says to ignore a relative path.
    let data_home = match xdg_data_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => PathBuf::from(home.context("neither XDG_DATA_HOME nor HOME is set")?)
            .join(".local/share"),
    };
    Ok(data_home.join("agent-jail"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(xdg: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
        data_dir_from(xdg.map(OsString::from), home.map(OsString::from))
    }

    #[test]
    fn prefers_an_absolute_xdg_data_home() {
        assert_eq!(
            dir(Some("/data"), Some("/home/jace")).unwrap(),
            PathBuf::from("/data/agent-jail")
        );
    }

    #[test]
    fn uses_an_absolute_xdg_runtime_dir() {
        let dir = runtime_dir_from(Some("/run/user/7".into()), Uid::from_raw(501), |_| false);
        assert_eq!(dir.unwrap(), PathBuf::from("/run/user/7"));
    }

    #[test]
    fn falls_back_to_the_runtime_dir_of_the_user() {
        let uid = Uid::from_raw(501);
        let exists = |dir: &Path| dir == Path::new("/run/user/501");
        for xdg in [None, Some(OsString::from("relative"))] {
            assert_eq!(
                runtime_dir_from(xdg, uid, exists).unwrap(),
                PathBuf::from("/run/user/501")
            );
        }
        let error = runtime_dir_from(None, uid, |_| false).unwrap_err();
        assert!(error.to_string().contains("--root"), "{error:#}");
    }

    #[test]
    fn makes_a_requested_root_absolute() {
        let root = state_root(Some(Path::new("state"))).unwrap();
        assert_eq!(root, std::env::current_dir().unwrap().join("state"));
    }

    #[test]
    fn falls_back_to_home_for_a_missing_or_relative_xdg_data_home() {
        let expected = PathBuf::from("/home/jace/.local/share/agent-jail");
        assert_eq!(dir(None, Some("/home/jace")).unwrap(), expected);
        assert_eq!(dir(Some("data"), Some("/home/jace")).unwrap(), expected);
        assert!(dir(None, None).is_err());
    }
}
