use super::subordinate::{
    CONTAINER_RANGE_SIZE, SubordinateRange, subordinate_gids, subordinate_uids,
};
use anyhow::{Context, Result, bail};
use nix::unistd::{Gid, Pid, Uid};
use std::io;
use std::process::Command;

/// One line of a UID or GID map: `count` container IDs starting at `container_id` map to
/// host IDs starting at `host_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    pub container_id: u32,
    pub host_id: u32,
    pub count: u32,
}

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
    /// The highest container ID that the maps cover.
    pub const MAX_CONTAINER_ID: u32 = CONTAINER_RANGE_SIZE;

    pub fn new(uid: Uid, gid: Gid, uids: SubordinateRange, gids: SubordinateRange) -> Self {
        Self {
            uid,
            gid,
            uids,
            gids,
        }
    }

    pub fn for_current_user() -> Result<Self> {
        let uid = Uid::effective();
        Ok(Self::new(
            uid,
            Gid::effective(),
            subordinate_uids(uid)?,
            subordinate_gids(uid)?,
        ))
    }

    pub fn uid_ranges(&self) -> [IdRange; 2] {
        ranges(self.uid.as_raw(), &self.uids)
    }

    pub fn gid_ranges(&self) -> [IdRange; 2] {
        ranges(self.gid.as_raw(), &self.gids)
    }

    /// Write the UID and GID maps of the user namespace that `pid` belongs to.
    ///
    /// Without privilege, the kernel lets a process map only its own IDs.
    /// newuidmap and newgidmap are setuid programs that also map the ranges in /etc/subuid and /etc/subgid.
    pub fn apply(&self, pid: Pid) -> Result<()> {
        run_map_tool("newuidmap", pid, &self.uid_ranges())?;
        run_map_tool("newgidmap", pid, &self.gid_ranges())
    }
}

fn ranges(host_id: u32, subordinate: &SubordinateRange) -> [IdRange; 2] {
    [
        IdRange {
            container_id: 0,
            host_id,
            count: 1,
        },
        IdRange {
            container_id: 1,
            host_id: subordinate.start,
            count: CONTAINER_RANGE_SIZE,
        },
    ]
}

/// Arguments for newuidmap or newgidmap: the PID, then `container-id host-id count` per line.
fn map_args(pid: Pid, ranges: &[IdRange]) -> Vec<String> {
    let lines = ranges
        .iter()
        .flat_map(|range| [range.container_id, range.host_id, range.count]);
    std::iter::once(pid.to_string())
        .chain(lines.map(|value| value.to_string()))
        .collect()
}

fn run_map_tool(tool: &str, pid: Pid, ranges: &[IdRange]) -> Result<()> {
    let output = match Command::new(tool).args(map_args(pid, ranges)).output() {
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
            map_args(Pid::from_raw(4321), &ranges(1000, &range)),
            ["4321", "0", "1000", "1", "1", "100000", "65536"]
        );
    }
}
