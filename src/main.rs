use anyhow::{Result, ensure};
use clap::Parser;
use nix::unistd::Uid;
use std::process::ExitCode;

mod cgroups;
mod cli;
mod config;
mod container;
mod container_id;
mod filesystem;
mod flock;
mod ipc;
mod network;
mod oci;
mod paths;
mod runtime;
mod signals;
mod user;

#[cfg(target_os = "linux")]
fn main() -> Result<ExitCode> {
    let cli = cli::Cli::parse();

    // The cgroup setup needs a cgroup that systemd delegates to a normal user.
    ensure!(
        !Uid::effective().is_root(),
        "aj does not run as root; run it as a normal user without sudo"
    );

    let mapping = user::Mapping::for_current_user()?;

    // A run that was killed cannot remove its own run directory, so each command removes the
    // directories that such runs left behind.
    if let Err(error) = filesystem::remove_stale_run_dirs(&mapping) {
        eprintln!("aj: warning: remove stale run directories: {error:#}");
    }

    match cli.command {
        cli::Command::Run(args) => runtime::Runtime::new(args.into_config()?, mapping)?.run(),
        // Validate the whole configuration before Runtime::new creates a cgroup.
        cli::Command::RunBundle { bundle } => {
            let config = oci::Bundle::load(&bundle)?.into_config(&mapping)?;
            runtime::Runtime::new(config, mapping)?.run()
        }
    }
}
