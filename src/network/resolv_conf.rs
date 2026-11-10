use super::pasta::DNS_FORWARD_ADDRESS;
use crate::config::Volume;
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const RESOLV_CONF: &str = "/etc/resolv.conf";

/// Point the container's resolver at pasta's DNS forwarder.
///
/// Call this after pivot_root, so the path refers to the container's root. A volume on
/// /etc/resolv.conf or on a directory above it keeps its own content, because writing there
/// would change host files.
pub fn write_resolv_conf(volumes: &[Volume]) -> Result<()> {
    let path = Path::new(RESOLV_CONF);
    if covered_by_volume(path, volumes) {
        return Ok(());
    }

    // The rootfs may have a symbolic link here, and its target may be missing or inside a
    // volume. Replacing the entry keeps the write inside the container's overlay.
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("remove {RESOLV_CONF}"));
        }
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .with_context(|| format!("create {RESOLV_CONF}"))?;
    writeln!(file, "nameserver {DNS_FORWARD_ADDRESS}")
        .with_context(|| format!("write {RESOLV_CONF}"))?;
    Ok(())
}

fn covered_by_volume(path: &Path, volumes: &[Volume]) -> bool {
    volumes
        .iter()
        .any(|volume| path.starts_with(&volume.destination))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn volume(destination: &str) -> Volume {
        Volume {
            source: PathBuf::from("/host"),
            destination: PathBuf::from(destination),
            read_only: false,
        }
    }

    #[test]
    fn skips_the_file_when_a_volume_covers_it() {
        let path = Path::new(RESOLV_CONF);
        for destination in ["/etc/resolv.conf", "/etc", "/"] {
            assert!(
                covered_by_volume(path, &[volume(destination)]),
                "{destination}"
            );
        }
        for destination in ["/etc/resolv", "/work", "/etc/resolv.conf.d"] {
            assert!(
                !covered_by_volume(path, &[volume(destination)]),
                "{destination}"
            );
        }
        assert!(!covered_by_volume(path, &[]));
    }
}
