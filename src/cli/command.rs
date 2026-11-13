use clap::{Parser, Subcommand};
use std::path::PathBuf;

use super::RunArgs;
use crate::container_id::ContainerId;

/// Top-level parsed CLI command.
#[derive(Parser, Debug)]
#[command(
    name = "aj",
    version,
    about = "An isolated environment where AI agents can install dependencies and run experiments without root privileges"
)]
pub struct Cli {
    /// Directory for container state [default: $XDG_RUNTIME_DIR/agent-jail].
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,

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
    /// Create a container from an OCI bundle. Its program waits until `start`.
    Create {
        /// Path to the bundle directory that contains config.json.
        #[arg(long, value_name = "DIR", default_value = ".")]
        bundle: PathBuf,
        /// Container ID.
        #[arg(value_parser = ContainerId::parse)]
        id: ContainerId,
    },
    /// Start the program of a created container.
    Start {
        /// Container ID.
        #[arg(value_parser = ContainerId::parse)]
        id: ContainerId,
    },
    /// Print the OCI state of a container as JSON.
    State {
        /// Container ID.
        #[arg(value_parser = ContainerId::parse)]
        id: ContainerId,
    },
    /// List containers and their current status.
    List,
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
    fn parses_the_container_id_and_the_state_root() {
        let cli = Cli::try_parse_from(["aj", "run", "--root", "/state", "--id", "box", "/bin/sh"])
            .unwrap();
        assert_eq!(cli.root, Some(PathBuf::from("/state")));
        let Command::Run(args) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(args.id.unwrap().as_str(), "box");

        let error = Cli::try_parse_from(["aj", "run", "--id", "my+box", "/bin/sh"]).unwrap_err();
        assert!(
            error.to_string().contains("invalid container ID"),
            "{error}"
        );
    }

    #[test]
    fn parses_create() {
        let cli = Cli::try_parse_from(["aj", "create", "--bundle", "./box", "box1"]).unwrap();
        let Command::Create { bundle, id } = cli.command else {
            panic!("expected the create command");
        };
        assert_eq!(bundle, PathBuf::from("./box"));
        assert_eq!(id.as_str(), "box1");

        // As in the OCI runtime command line, the bundle is the current directory by default.
        let cli = Cli::try_parse_from(["aj", "create", "box1"]).unwrap();
        let Command::Create { bundle, .. } = cli.command else {
            panic!("expected the create command");
        };
        assert_eq!(bundle, PathBuf::from("."));
        assert!(Cli::try_parse_from(["aj", "create"]).is_err());
        assert!(Cli::try_parse_from(["aj", "create", "-box"]).is_err());
    }

    #[test]
    fn parses_start() {
        let cli = Cli::try_parse_from(["aj", "start", "box1"]).unwrap();
        let Command::Start { id } = cli.command else {
            panic!("expected the start command");
        };
        assert_eq!(id.as_str(), "box1");
        assert!(Cli::try_parse_from(["aj", "start"]).is_err());
    }

    #[test]
    fn parses_state_and_list() {
        let cli = Cli::try_parse_from(["aj", "state", "box"]).unwrap();
        let Command::State { id } = cli.command else {
            panic!("expected the state command");
        };
        assert_eq!(id.as_str(), "box");
        assert!(Cli::try_parse_from(["aj", "state"]).is_err());
        assert!(Cli::try_parse_from(["aj", "state", ".."]).is_err());

        let cli = Cli::try_parse_from(["aj", "--root", "/state", "list"]).unwrap();
        assert!(matches!(cli.command, Command::List));
        assert_eq!(cli.root, Some(PathBuf::from("/state")));
    }

    #[test]
    fn rejects_missing_run_inputs_and_invalid_numeric_limits() {
        assert!(Cli::try_parse_from(["aj"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run", "--rootfs", "./rootfs"]).is_err());
        assert!(Cli::try_parse_from(["aj", "run", "--mem", "invalid", "/bin/sh"]).is_err());
    }
}
