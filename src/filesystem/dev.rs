use anyhow::{Context, Result};
use nix::mount::{MsFlags, mount};
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;

/// Device nodes that common programs open. A user namespace cannot create device nodes, so the
/// container gets the host's nodes through bind mounts.
const DEVICES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

const LINKS: [(&str, &str); 5] = [
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
    ("ptmx", "pts/ptmx"),
];

/// Mount a new `/dev` under `root` with the devices, links, pseudo terminals, and shared memory
/// that programs expect.
pub fn prepare_dev(root: &Path) -> Result<()> {
    let dev = root.join("dev");
    fs::create_dir_all(&dev).with_context(|| format!("create {dev:?}"))?;
    mount_fs(
        "tmpfs",
        &dev,
        MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
        "mode=755,size=65536k",
    )?;

    for name in DEVICES {
        bind_device(&Path::new("/dev").join(name), &dev.join(name))?;
    }
    create_links(&dev)?;

    // newinstance keeps the container's terminals apart from the host's. GID 5 is the tty group.
    let pts = dev.join("pts");
    fs::create_dir(&pts).with_context(|| format!("create {pts:?}"))?;
    mount_fs(
        "devpts",
        &pts,
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620,gid=5",
    )?;

    let shm = dev.join("shm");
    fs::create_dir(&shm).with_context(|| format!("create {shm:?}"))?;
    mount_fs(
        "tmpfs",
        &shm,
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        "mode=1777,size=65536k",
    )
}

fn mount_fs(fs_type: &str, target: &Path, flags: MsFlags, options: &str) -> Result<()> {
    mount(Some(fs_type), target, Some(fs_type), flags, Some(options))
        .with_context(|| format!("mount {fs_type} on {target:?} with {options}"))
}

fn bind_device(source: &Path, target: &Path) -> Result<()> {
    // A bind mount needs an existing file to cover.
    File::create(target).with_context(|| format!("create {target:?}"))?;
    mount::<_, _, str, str>(Some(source), target, None, MsFlags::MS_BIND, None)
        .with_context(|| format!("bind mount {source:?} on {target:?}"))
}

fn create_links(dev: &Path) -> Result<()> {
    for (name, target) in LINKS {
        let link = dev.join(name);
        symlink(target, &link).with_context(|| format!("link {link:?} to {target}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;

    #[test]
    fn creates_the_standard_links() {
        let dev = std::env::temp_dir().join(format!("agent-jail-dev-{:x}", random::<u64>()));
        fs::create_dir(&dev).unwrap();
        create_links(&dev).unwrap();

        let target = |name: &str| fs::read_link(dev.join(name)).unwrap();
        assert_eq!(target("fd"), Path::new("/proc/self/fd"));
        assert_eq!(target("stderr"), Path::new("/proc/self/fd/2"));
        assert_eq!(target("ptmx"), Path::new("pts/ptmx"));
        fs::remove_dir_all(dev).unwrap();
    }
}
