use super::subordinate::{
    CONTAINER_RANGE_SIZE, SubordinateRange, subordinate_gids, subordinate_uids,
};
use anyhow::{Context, Result, bail};
use nix::unistd::{Gid, Pid, Uid};
use std::io;
use std::process::Command;

/// How container IDs map to host IDs.
///
/// Container root maps to the user who runs the runtime, so files that it creates stay
/// writable for that user. Container IDs 1 to 65536 map to the user's subordinate ranges.
pub struct Mapping {
    uid: Uid,
    gid: Gid,
    uids: SubordinateRange,
    gids: SubordinateRange,
}

impl Mapping {
    pub fn for_current_user() -> Result<Self> {
        let uid = Uid::effective();
        Ok(Self {
            uid,
            gid: Gid::effective(),
            uids: subordinate_uids(uid)?,
            gids: subordinate_gids(uid)?,
        })
    }

    /// Write the UID and GID maps of the user namespace that `pid` belongs to.
    ///
    /// Without privilege, the kernel lets a process map only its own IDs. 
    /// newuidmap and
    /// newgidmap are setuid programs that also map the ranges in /etc/subuid and /etc/subgid.
    pub fn apply(&self, pid: Pid) -> Result<()> {
        run_map_tool("newuidmap", pid, self.uid.as_raw(), &self.uids)?;
        run_map_tool("newgidmap", pid, self.gid.as_raw(), &self.gids)
    }
}

/// Arguments for newuidmap or newgidmap: the PID, then `container-id host-id count` per line.
fn map_args(pid: Pid, host_id: u32, range: &SubordinateRange) -> Vec<String> {
    let root = [0, host_id, 1];
    let others = [1, range.start, CONTAINER_RANGE_SIZE];
    std::iter::once(pid.to_string())
        .chain(root.iter().chain(&others).map(u32::to_string))
        .collect()
}

fn run_map_tool(tool: &str, pid: Pid, host_id: u32, range: &SubordinateRange) -> Result<()> {
    let output = match Command::new(tool)
        .args(map_args(pid, host_id, range))
        .output()
    {
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "{tool} is not installed; install the uidmap package, for example `sudo apt install uidmap`"
        ),
        result => result.with_context(|| format!("run {tool}"))?,
    };
    if !output.status.success() {
        bail!(
            "{tool} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_root_to_the_user_and_the_rest_to_the_range() {
        let range = SubordinateRange {
            start: 100000,
            count: 70000,
        };
        assert_eq!(
            map_args(Pid::from_raw(4321), 1000, &range),
            ["4321", "0", "1000", "1", "1", "100000", "65536"]
        );
    }
}
