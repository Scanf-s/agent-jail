use crate::cgroups::CgroupSettings;
use std::path::PathBuf;

/// Settings the runtime needs to create and run a container.
pub struct ContainerConfig {
    /// Rootfs path on the host. A relative path is resolved from the current working directory.
    pub rootfs: PathBuf,
    pub process: ProcessConfig,
    pub id_mapping: IdMapping,
    pub resources: CgroupSettings,
}

/// The program to run inside the container.
pub struct ProcessConfig {
    /// The program followed by its arguments, so `args[0]` is the program.
    pub args: Vec<String>,
    /// UID that runs the program inside the container.
    pub uid: u32,
    /// GID that runs the program inside the container.
    pub gid: u32,
}

/// Host IDs that the container's root user and group map to.
pub struct IdMapping {
    pub host_uid: u32,
    pub host_gid: u32,
}
