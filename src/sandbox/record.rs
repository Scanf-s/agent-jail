use super::SandboxName;
use crate::cgroups::CgroupSettings;
use crate::config::{NetworkMode, Volume};
use crate::image::ImageId;
use crate::user::{IdRange, Mapping};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The content of `sandbox.json`: what every container of the sandbox needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRecord {
    /// The format of this file, so that a later version of aj can still read it.
    pub record_version: u32,
    pub name: String,
    /// The lower directory of the overlay. The upper directory holds only changes to it.
    pub image: ImageId,
    /// When the sandbox was created, in seconds after the Unix epoch.
    pub created: u64,
    pub volumes: Vec<Volume>,
    /// `KEY=VALUE` entries that each command receives unless it changes them.
    pub env: Vec<String>,
    /// The working directory of a command that names none.
    pub cwd: PathBuf,
    pub network: NetworkMode,
    pub resources: CgroupSettings,
    /// The ID maps that the files in the upper directory were written with. Other maps would
    /// show those files with other owners.
    pub uid_map: Vec<IdRange>,
    pub gid_map: Vec<IdRange>,
}

impl SandboxRecord {
    pub const VERSION: u32 = 1;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &SandboxName,
        image: ImageId,
        volumes: Vec<Volume>,
        env: Vec<String>,
        cwd: PathBuf,
        network: NetworkMode,
        resources: CgroupSettings,
        mapping: &Mapping,
    ) -> Self {
        Self {
            record_version: Self::VERSION,
            name: name.as_str().to_owned(),
            image,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            volumes,
            env,
            cwd,
            network,
            resources,
            uid_map: mapping.uid_ranges().to_vec(),
            gid_map: mapping.gid_ranges().to_vec(),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::user::SubordinateRange;
    use nix::unistd::{Gid, Uid};

    pub fn mapping(start: u32) -> Mapping {
        let range = || SubordinateRange {
            start,
            count: 65536,
        };
        Mapping::new(Uid::from_raw(1000), Gid::from_raw(1000), range(), range())
    }

    pub fn record(name: &str, mapping: &Mapping) -> SandboxRecord {
        SandboxRecord::new(
            &SandboxName::parse(name).unwrap(),
            ImageId::parse(&format!("sha256-{}", "a".repeat(64))).unwrap(),
            Vec::new(),
            vec![String::from("PATH=/bin")],
            PathBuf::from("/"),
            NetworkMode::Pasta,
            CgroupSettings {
                cpu: None,
                memory_max: Some(1),
                pids_max: Some(1),
            },
            mapping,
        )
    }
}
