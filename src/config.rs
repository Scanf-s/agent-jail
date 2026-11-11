use crate::cgroups::CgroupSettings;
use std::collections::HashMap;
use std::path::PathBuf;

/// Settings the runtime needs to create and run a container.
pub struct ContainerConfig {
    /// Rootfs path on the host. A relative path is resolved from the current working directory.
    pub rootfs: PathBuf,
    /// The absolute path that the OCI state reports as the bundle. `run` has no bundle, so it
    /// reports the rootfs.
    pub bundle: PathBuf,
    /// The annotations of the OCI configuration, which the OCI state reports.
    pub annotations: Option<HashMap<String, String>>,
    pub process: ProcessConfig,
    pub resources: CgroupSettings,
    pub volumes: Vec<Volume>,
    pub network: NetworkMode,
}

/// How the container reaches the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NetworkMode {
    /// Outbound access through pasta. Host services that listen only on 127.0.0.1 stay
    /// unreachable, but services on other host addresses are reachable.
    Pasta,
    /// A new network namespace that has only the loopback interface.
    None,
}

/// A host file or directory that the container sees at `destination`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Absolute host path without symbolic links.
    pub source: PathBuf,
    /// Absolute path inside the container.
    pub destination: PathBuf,
    pub read_only: bool,
}

/// The program to run inside the container.
pub struct ProcessConfig {
    /// The program followed by its arguments, so `args[0]` is the program.
    pub args: Vec<String>,
    /// `KEY=VALUE` entries. The program receives only these variables.
    pub env: Vec<String>,
    /// Working directory inside the container.
    pub cwd: PathBuf,
    /// UID that runs the program inside the container.
    pub uid: u32,
    /// GID that runs the program inside the container.
    pub gid: u32,
    /// Supplementary groups of the program. The host user's groups are never kept.
    pub additional_gids: Vec<u32>,
}
