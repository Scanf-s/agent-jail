use super::env_arg::build_env;
use super::run_args::{resolve_workdir, resource_settings};
use super::volume_arg::parse_volume;
use crate::config::NetworkMode;
use crate::image;
use crate::sandbox::{SandboxName, SandboxRecord};
use crate::user::Mapping;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

/// The options of `aj sandbox create`. They describe the environment that every command of the
/// sandbox gets, so they follow the rules of `aj run`.
#[derive(Args, Debug)]
pub struct SandboxCreateArgs {
    /// Image: a name in the image store, such as `default`, or an image ID. The sandbox keeps
    /// the image that the name points to now.
    #[arg(long, default_value = "default")]
    pub image: String,

    /// Number of CPUs (e.g. 0.5 for half a core, 2.0 for two cores).
    #[arg(long, default_value_t = 1.0)]
    pub cpus: f64,

    /// Memory limit (bytes)
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    pub mem: u64,

    /// Maximum number of tasks (processes and threads).
    #[arg(long, default_value_t = 1024)]
    pub pids: u64,

    /// Bind mount a host file or directory. CONTAINER defaults to the host path. Repeatable.
    #[arg(short = 'v', long = "volume", value_name = "HOST[:CONTAINER][:ro]")]
    pub volumes: Vec<String>,

    /// Default working directory of commands [default: the current directory if a volume
    /// contains it, else /].
    #[arg(short = 'w', long)]
    pub workdir: Option<PathBuf>,

    /// Set an environment variable, or copy KEY from the host. Repeatable. Without it, commands
    /// receive only PATH, HOME, TERM, and the proxy variables.
    #[arg(long = "env", value_name = "KEY[=VALUE]")]
    pub envs: Vec<String>,

    /// Network mode of the sandbox.
    #[arg(long, value_enum, default_value_t = NetworkMode::Pasta)]
    pub net: NetworkMode,

    /// Sandbox name: 1 to 128 letters, digits, '_', '.', and '-' that start with a letter or
    /// digit.
    #[arg(value_parser = SandboxName::parse)]
    pub name: SandboxName,
}

impl SandboxCreateArgs {
    /// Validate the CLI input and convert it into the record of a new sandbox whose files use
    /// `mapping`.
    pub fn into_record(self, mapping: &Mapping) -> Result<SandboxRecord> {
        let resources = resource_settings(self.cpus, self.mem, self.pids)?;
        let image = image::resolve(&self.image)?;
        let volumes = self
            .volumes
            .iter()
            .map(|spec| parse_volume(spec))
            .collect::<Result<Vec<_>>>()?;
        let cwd = resolve_workdir(self.workdir, &volumes)?;
        let env = build_env(&self.envs, |key| {
            std::env::var_os(key).map(|value| value.to_string_lossy().into_owned())
        })?;
        Ok(SandboxRecord::new(
            &self.name, image, volumes, env, cwd, self.net, resources, mapping,
        ))
    }
}
