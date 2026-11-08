use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

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
    fn falls_back_to_home_for_a_missing_or_relative_xdg_data_home() {
        let expected = PathBuf::from("/home/jace/.local/share/agent-jail");
        assert_eq!(dir(None, Some("/home/jace")).unwrap(), expected);
        assert_eq!(dir(Some("data"), Some("/home/jace")).unwrap(), expected);
        assert!(dir(None, None).is_err());
    }
}
