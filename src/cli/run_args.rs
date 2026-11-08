use crate::cgroups::{CgroupSettings, CpuMax};
use crate::config::{ContainerConfig, ProcessConfig};
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::num::NonZeroU64;
use std::path::PathBuf;

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Path to the rootfs directory (e.g. ./rootfs).
    pub rootfs: PathBuf,

    /// Number of CPUs (e.g. 0.5 for half a core, 2.0 for two cores).
    #[arg(long, default_value_t = 1.0)]
    pub cpus: f64,

    /// Memory limit (bytes)
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    pub mem: u64,

    /// Maximum number of tasks (processes and threads).
    #[arg(long, default_value_t = 1024)]
    pub pids: u64,

    /// Command to execute inside the container.
    pub cmd: String,

    /// Arguments to pass to the command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl RunArgs {
    /// Validate the CLI input and convert it into the runtime configuration.
    pub fn into_config(self) -> Result<ContainerConfig> {
        let resources = self.validate()?;

        let mut args = Vec::with_capacity(self.args.len() + 1);
        args.push(self.cmd);
        args.extend(self.args);

        Ok(ContainerConfig {
            rootfs: self.rootfs,
            process: ProcessConfig {
                args,
                // Pass the runtime's own environment so that `run` keeps inheriting it.
                env: std::env::vars_os()
                    .map(|(key, value)| {
                        format!("{}={}", key.to_string_lossy(), value.to_string_lossy())
                    })
                    .collect(),
                // pivot_root already leaves the process at the new root.
                cwd: PathBuf::from("/"),
                uid: 0,
                gid: 0,
            },
            resources,
        })
    }

    fn validate(&self) -> Result<CgroupSettings> {
        if !self.rootfs.is_dir() {
            bail!(
                "rootfs {:?} does not exist or is not a directory",
                self.rootfs
            );
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_cpu_limits() {
        let mut args = RunArgs {
            rootfs: PathBuf::from("/"),
            cpus: 1.0,
            mem: 1,
            pids: 1,
            cmd: String::from("true"),
            args: Vec::new(),
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
            rootfs: PathBuf::from("."),
            cpus: 1.0,
            mem: 1,
            pids: 1,
            cmd: String::from("/bin/sh"),
            args: vec![String::from("-c"), String::from("echo hello")],
        };

        let config = args.into_config().unwrap();
        assert_eq!(config.rootfs, PathBuf::from("."));
        assert_eq!(config.process.args, ["/bin/sh", "-c", "echo hello"]);
        assert_eq!(config.process.env.len(), std::env::vars_os().count());
        assert_eq!(config.process.cwd, PathBuf::from("/"));
        assert_eq!((config.process.uid, config.process.gid), (0, 0));
    }
}
