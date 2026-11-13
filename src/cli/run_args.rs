use super::env_arg::build_env;
use super::volume_arg::{default_workdir, parse_volume};
use crate::cgroups::{CgroupSettings, CpuMax};
use crate::config::{ContainerConfig, NetworkMode, ProcessConfig};
use crate::container_id::ContainerId;
use crate::paths;
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use std::num::NonZeroU64;
use std::path::PathBuf;

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Container ID: 1 to 128 letters, digits, '_', '.', and '-' that start with a letter or
    /// digit [default: 16 random hexadecimal digits].
    #[arg(long, value_parser = ContainerId::parse)]
    pub id: Option<ContainerId>,

    /// Rootfs directory [default: $XDG_DATA_HOME/agent-jail/images/default].
    #[arg(long)]
    pub rootfs: Option<PathBuf>,

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

    /// Working directory inside the container [default: the current directory if a volume
    /// contains it, else /].
    #[arg(short = 'w', long)]
    pub workdir: Option<PathBuf>,

    /// Set an environment variable, or copy KEY from the host. Repeatable. Without it, the
    /// container receives only PATH, HOME, TERM, and the proxy variables.
    #[arg(long = "env", value_name = "KEY[=VALUE]")]
    pub envs: Vec<String>,

    /// Network mode of the container.
    #[arg(long, value_enum, default_value_t = NetworkMode::Pasta)]
    pub net: NetworkMode,

    /// Command to execute inside the container, and its arguments.
    // One positional, because clap stops parsing aj's options only after the last positional
    // gets a value. Without allow_hyphen_values, a mistyped option before the program fails.
    #[arg(value_name = "COMMAND", required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl RunArgs {
    /// Validate the CLI input and convert it into the runtime configuration.
    pub fn into_config(self) -> Result<ContainerConfig> {
        let resources = self.validate()?;
        let rootfs = resolve_rootfs(self.rootfs)?;
        let bundle = std::path::absolute(&rootfs)
            .with_context(|| format!("resolve rootfs path {rootfs:?}"))?;
        let volumes = self
            .volumes
            .iter()
            .map(|spec| parse_volume(spec))
            .collect::<Result<Vec<_>>>()?;
        let cwd = match self.workdir {
            Some(workdir) => {
                ensure!(
                    workdir.is_absolute(),
                    "--workdir must be an absolute path inside the container but got {workdir:?}"
                );
                workdir
            }
            None => {
                let host_cwd = std::env::current_dir().context("read the current directory")?;
                default_workdir(&host_cwd, &volumes)
            }
        };

        let env = build_env(&self.envs, |key| {
            std::env::var_os(key).map(|value| value.to_string_lossy().into_owned())
        })?;

        Ok(ContainerConfig {
            id: self.id,
            rootfs,
            bundle,
            annotations: None,
            process: ProcessConfig {
                args: self.command,
                env,
                cwd,
                uid: 0,
                gid: 0,
                additional_gids: Vec::new(),
            },
            resources,
            volumes,
            network: self.net,
        })
    }

    fn validate(&self) -> Result<CgroupSettings> {
        if !self.cpus.is_finite() || self.cpus <= 0.0 {
            bail!("--cpus must be finite and positive but got {}", self.cpus);
        }

        let host_cpus = num_cpus::get() as f64;
        if self.cpus > host_cpus {
            bail!(
                "--cpus must be less than or equal to `max_cpus`. you have only {} cpus available",
                host_cpus
            );
        }

        let cpu_quota_us = (self.cpus * CpuMax::DEFAULT_PERIOD_US as f64) as u64;
        let cpu_quota_us =
            NonZeroU64::new(cpu_quota_us).context("--cpus is too small for a nonzero CPU quota")?;

        Ok(CgroupSettings {
            cpu: Some(CpuMax {
                quota_us: cpu_quota_us,
                period_us: CpuMax::DEFAULT_PERIOD_US,
            }),
            memory_max: Some(self.mem),
            pids_max: Some(self.pids),
        })
    }
}

fn resolve_rootfs(requested: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(rootfs) = requested {
        ensure!(
            rootfs.is_dir(),
            "rootfs {rootfs:?} does not exist or is not a directory"
        );
        return Ok(rootfs);
    }
    let rootfs = paths::default_rootfs()?;
    ensure!(
        rootfs.is_dir(),
        "no rootfs at {rootfs:?}; fetch one with ./scripts/fetch-rootfs.sh, or name another with --rootfs"
    );
    Ok(rootfs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_cpu_limits() {
        let mut args = RunArgs {
            id: None,
            rootfs: Some(PathBuf::from("/")),
            cpus: 1.0,
            mem: 1,
            pids: 1,
            volumes: Vec::new(),
            workdir: None,
            envs: Vec::new(),
            net: NetworkMode::None,
            command: vec![String::from("true")],
        };

        assert!(args.validate().is_ok());
        for cpus in [f64::NAN, f64::INFINITY, 0.0, -1.0, 0.000001] {
            args.cpus = cpus;
            assert!(args.validate().is_err(), "accepted {cpus}");
        }
    }

    #[test]
    fn converts_cli_input_into_container_config() {
        let args = RunArgs {
            id: Some(ContainerId::parse("box").unwrap()),
            rootfs: Some(PathBuf::from(".")),
            cpus: 1.0,
            mem: 1,
            pids: 1,
            volumes: Vec::new(),
            workdir: None,
            envs: Vec::new(),
            net: NetworkMode::None,
            command: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("echo hello"),
            ],
        };

        let config = args.into_config().unwrap();
        assert_eq!(config.rootfs, PathBuf::from("."));
        assert_eq!(config.id, ContainerId::parse("box").ok());
        assert_eq!(config.process.args, ["/bin/sh", "-c", "echo hello"]);
        assert!(config.process.env.contains(&String::from("HOME=/root")));
        assert_eq!(config.process.cwd, PathBuf::from("/"));
        assert_eq!((config.process.uid, config.process.gid), (0, 0));
    }
}
