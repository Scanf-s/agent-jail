use anyhow::{Context, Result, ensure};
use clap::Parser;
use nix::unistd::Uid;
use std::path::Path;
use std::process::ExitCode;

#[macro_use]
mod report;

mod cgroups;
mod cli;
mod config;
mod container;
mod container_id;
mod descriptors;
mod filesystem;
mod flock;
mod ipc;
mod network;
mod oci;
mod paths;
mod proc_stat;
mod runtime;
mod signals;
mod state;
mod user;

#[cfg(target_os = "linux")]
fn main() -> Result<ExitCode> {
    descriptors::close_inherited().context("close the descriptors of the caller")?;
    let cli = cli::Cli::parse();

    // The cgroup setup needs a cgroup that systemd delegates to a normal user.
    ensure!(
        !Uid::effective().is_root(),
        "aj does not run as root; run it as a normal user without sudo"
    );

    let state_root = paths::state_root(cli.root.as_deref())?;

    match cli.command {
        cli::Command::Run(args) => {
            let mapping = prepare_new_container(&state_root)?;
            runtime::Runtime::new(args.into_config()?, mapping, &state_root)?.run()
        }
        // Validate the whole configuration before Runtime::new creates a cgroup.
        cli::Command::RunBundle { bundle } => {
            let mapping = prepare_new_container(&state_root)?;
            let config = oci::Bundle::load(&bundle)?.into_config(&mapping)?;
            runtime::Runtime::new(config, mapping, &state_root)?.run()
        }
        cli::Command::Create { bundle, id } => {
            let mapping = prepare_new_container(&state_root)?;
            let mut config = oci::Bundle::load(&bundle)?.into_config(&mapping)?;
            config.id = Some(id);
            runtime::Runtime::new(config, mapping, &state_root)?.create()
        }
        cli::Command::Start { id } => {
            runtime::start(&state_root, &id)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::State { id } => {
            let record = state::read(&state_root, &id)?;
            let json = serde_json::to_string_pretty(&record.oci).context("serialize the state")?;
            println!("{json}");
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::List => {
            print!("{}", state::table(&state::list(&state_root)?));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Remove what killed runs left behind, and return the ID mapping for a new container.
///
/// Only commands that create a container clean up. A command that reads the state must not
/// remove the record of a killed container before it reports that container.
fn prepare_new_container(state_root: &Path) -> Result<user::Mapping> {
    let mapping = user::Mapping::for_current_user()?;
    // A killed run cannot remove its own run directory and state directory.
    if let Err(error) = filesystem::remove_stale_run_dirs(&mapping) {
        report!("aj: warning: remove stale run directories: {error:#}");
    }
    if let Err(error) = state::remove_stale(state_root) {
        report!("aj: warning: remove stale state directories: {error:#}");
    }
    Ok(mapping)
}
