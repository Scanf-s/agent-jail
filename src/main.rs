use anyhow::{Result, ensure};
use clap::Parser;
use nix::unistd::Uid;
use std::process::ExitCode;

mod cgroups;
mod cli;
mod config;
mod container;
mod filesystem;
mod ipc;
mod oci;
mod runtime;
mod user;

#[cfg(target_os = "linux")]
fn main() -> Result<ExitCode> {
    // Parse the CLI arguments.
    let cli = cli::Cli::parse();

    // The cgroup setup needs a cgroup that systemd delegates to a normal user.
    ensure!(
        !Uid::effective().is_root(),
        "container-runtime does not run as root; run it as a normal user without sudo"
    );

    match cli.command {
        // Create a new container and run the requested command inside it.
        cli::Command::Run(args) => runtime::Runtime::new(args.into_config()?)?.run(),
        // Validate the whole configuration before Runtime::new creates a cgroup.
        cli::Command::RunBundle { bundle } => {
            let config = oci::Bundle::load(&bundle)?.into_config()?;
            runtime::Runtime::new(config)?.run()
        }
    }
}
