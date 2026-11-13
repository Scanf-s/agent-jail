use super::STATE_FILE;
use crate::container_id::ContainerId;
use crate::oci::SUPPORTED_OCI_VERSION;
use crate::proc_stat;
use anyhow::{Context, Result};
use nix::unistd::Pid;
use oci_spec::runtime::{ContainerState, State};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The content of `state.json`: the OCI state of a container, and what the runtime needs to
/// check that state later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(flatten)]
    pub oci: State,
    /// The format of this file, so that a later runtime can still read files of this one.
    pub state_version: u32,
    /// When container init started, in clock ticks after boot, as `/proc/<pid>/stat` shows it.
    /// Together with the PID, it tells init apart from a later process that reuses the PID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_start_time: Option<u64>,
    /// The exit code of the container program, after it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
    /// The cgroup of the container. A process that the runtime could not end keeps it
    /// populated after init ended.
    pub cgroup: PathBuf,
    /// When the runtime created the container, in seconds after the Unix epoch.
    pub created: u64,
    /// Whether the runtime removes the container when its program ends, as `run` does. OCI
    /// keeps a container that `create` created until `delete`. Until then, the user does not
    /// know the container, so it is removed, too.
    pub auto_delete: bool,
}

impl Record {
    pub const VERSION: u32 = 1;

    /// The record of a container in the cgroup `cgroup` that the runtime is creating now.
    pub fn creating(
        id: &ContainerId,
        cgroup: PathBuf,
        bundle: PathBuf,
        annotations: Option<HashMap<String, String>>,
    ) -> Self {
        let mut oci = State::default();
        oci.set_version(SUPPORTED_OCI_VERSION.to_owned())
            .set_id(id.to_string())
            .set_status(ContainerState::Creating)
            .set_bundle(bundle)
            .set_annotations(annotations);
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self {
            oci,
            state_version: Self::VERSION,
            init_start_time: None,
            exit_code: None,
            cgroup,
            created,
            auto_delete: true,
        }
    }

    /// Read the state file in the state directory `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_FILE);
        let json = fs::read(&path).with_context(|| format!("read {path:?}"))?;
        serde_json::from_slice(&json).with_context(|| format!("parse {path:?}"))
    }

    /// Whether container init still runs. Until the container is created, the record names no
    /// init, so this returns false.
    pub fn init_runs(&self) -> Result<bool> {
        match (self.oci.pid(), self.init_start_time) {
            (Some(pid), Some(start_time)) => proc_stat::is_running(Pid::from_raw(*pid), start_time),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn record() -> Record {
        let annotations = HashMap::from([("purpose".to_owned(), "test".to_owned())]);
        Record::creating(
            &ContainerId::parse("box1").unwrap(),
            PathBuf::from("/sys/fs/cgroup/jail_box1"),
            PathBuf::from("/home/user/box"),
            Some(annotations),
        )
    }

    #[test]
    fn writes_the_oci_fields_at_the_top_level() {
        let mut record = record();
        record.created = 1_762_838_400;

        let json: Value = serde_json::to_value(&record).unwrap();

        assert_eq!(
            json,
            json!({
                "ociVersion": "1.3.0",
                "id": "box1",
                "status": "creating",
                "bundle": "/home/user/box",
                "annotations": { "purpose": "test" },
                "stateVersion": 1,
                "cgroup": "/sys/fs/cgroup/jail_box1",
                "created": 1_762_838_400,
                "autoDelete": true,
            })
        );
    }

    #[test]
    fn checks_init_by_its_pid_and_start_time() {
        let mut record = record();
        assert!(!record.init_runs().unwrap());

        let this = Pid::this();
        let start_time = proc_stat::start_time(this).unwrap();
        record.oci.set_pid(Some(this.as_raw()));
        record.init_start_time = Some(start_time);
        assert!(record.init_runs().unwrap());

        // A later process with the same PID has another start time.
        record.init_start_time = Some(start_time + 1);
        assert!(!record.init_runs().unwrap());
    }

    #[test]
    fn reads_what_it_writes() {
        let mut record = record();
        record
            .oci
            .set_status(ContainerState::Stopped)
            .set_pid(Some(48213));
        record.init_start_time = Some(9_150_231);
        record.exit_code = Some(130);

        let json = serde_json::to_string(&record).unwrap();

        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), record);
    }
}
