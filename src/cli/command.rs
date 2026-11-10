use clap::{Parser, Subcommand};
use std::path::PathBuf;

use super::RunArgs;

/// Top-level parsed CLI command.
#[derive(Parser, Debug)]
#[command(
    name = "aj",
    version,
    about = "An isolated environment where AI agents can install dependencies and run experiments without root privileges"
)]
pub struct Cli {
    /// The subcommand to dispatch (run, ...).
    #[command(subcommand)]
    pub command: Command,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a command inside an isolated rootfs.
    Run(RunArgs),
    /// Run the program described by an OCI bundle.
    RunBundle {
        /// Path to the bundle directory that contains config.json.
        bundle: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkMode;

    fn run_args(args: &[&str]) -> RunArgs {
        let cli = Cli::try_parse_from(args).unwrap();
        let Command::Run(run) = cli.command else {
            panic!("expected the run command");
        };
        run
    }

    #[test]
    fn parses_run_bundle() {
        let cli = Cli::try_parse_from(["aj", "run-bundle", "./bundle"]).unwrap();
        let Command::RunBundle { bundle } = cli.command else {
            panic!("expected the run-bundle command");
        };
        assert_eq!(bundle, PathBuf::from("./bundle"));
        assert!(Cli::try_parse_from(["aj", "run-bundle"]).is_err());
    }

    #[test]
    fn parses_run_with_default_limits() {
        let args = run_args(&["aj", "run", "/bin/sh"]);
        assert_eq!(args.rootfs, None);
        assert_eq!(args.cmd, "/bin/sh");
        assert!(args.args.is_empty());
        assert_eq!(args.cpus, 1.0);
        assert_eq!(args.mem, 512 * 1024 * 1024);
        assert_eq!(args.pids, 1024);
        assert_eq!(args.net, NetworkMode::Pasta);
    }

    #[test]
    fn parses_limits_and_hyphenated_command_arguments() {
        let args = run_args(&[
            "aj",
            "run",
            "--cpus",
            "0.5",
            "--mem",
            "1048576",
            "--pids",
            "64",
            "--rootfs",
            "./rootfs",
            "--",
            "/bin/sh",
            "-c",
            "echo hello",
        ]);
        assert_eq!(args.rootfs, Some(PathBuf::from("./rootfs")));
        assert_eq!(args.cmd, "/bin/sh");
        assert_eq!(args.cpus, 0.5);
        assert_eq!(args.mem, 1_048_576);
        assert_eq!(args.pids, 64);
        assert_eq!(args.args, ["-c", "echo hello"]);
    }

    #[test]
    fn parses_net() {
        let args = run_args(&["aj", "run", "--net", "none", "/bin/sh"]);
        assert_eq!(args.net, NetworkMode::None);
        let args = run_args(&["aj", "run", "--net", "pasta", "/bin/sh"]);
        assert_eq!(args.net, NetworkMode::Pasta);
        assert!(Cli::try_parse_from(["aj", "run", "--net", "bridge", "/bin/sh"]).is_err());
    }

    #[test]
    fn parses_repeated_env_options() {
        let args = run_args(&["aj", "run", "--env", "A=1", "--env", "B", "/bin/sh", "-e"]);
        assert_eq!(args.envs, ["A=1", "B"]);
        // Commands such as `node -e` keep their own -e.
        assert_eq!(args.args, ["-e"]);
    }

    #[test]
    fn rejects_missing_run_inputs_and_invalid_numeric_limits() {
        assert!(Cli::try_parse_from(["aj"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run", "--rootfs", "./rootfs"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run", "--mem", "invalid", "/bin/sh"]).is_err());
    }
}
