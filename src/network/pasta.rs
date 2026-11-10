use anyhow::{Context, Result, bail};
use nix::unistd::Pid;
use std::io;
use std::process::Command;

/// The DNS server address inside the container. pasta forwards queries to this address to the
/// first resolver in the host's /etc/resolv.conf.
pub const DNS_FORWARD_ADDRESS: &str = "169.254.1.1";

/// Connect the network namespace of `pid` to the host network through pasta.
///
/// pasta returns after it configures the namespace and keeps running in the background.
/// It exits when `pid` exits, so `pid` must live as long as the container.
pub fn start_pasta(pid: Pid) -> Result<()> {
    let output = match Command::new("pasta").args(pasta_args(pid)).output() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "pasta is not installed; install the passt package, for example `sudo apt install passt`, \
             or run with --net none"
        ),
        result => result.context("run pasta")?,
    };
    // On success, stderr holds only notices such as the IPv6 warning that --quiet keeps.
    if !output.status.success() {
        bail!(
            "pasta failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn pasta_args(pid: Pid) -> Vec<String> {
    [
        "--config-net",
        "--quiet",
        // pasta forwards ports in both directions by default. Container ports would be published
        // on every host address, and container connections to localhost would reach the host.
        "-t",
        "none",
        "-u",
        "none",
        "-T",
        "none",
        "-U",
        "none",
        // Otherwise connections to the gateway address reach the host's loopback.
        "--no-map-gw",
        "--dns-forward",
        DNS_FORWARD_ADDRESS,
    ]
    .into_iter()
    .map(String::from)
    .chain(std::iter::once(pid.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_off_all_forwarding_and_targets_the_pid() {
        let args = pasta_args(Pid::from_raw(4321));
        for (option, value) in [
            ("-t", "none"),
            ("-u", "none"),
            ("-T", "none"),
            ("-U", "none"),
        ] {
            let position = args.iter().position(|arg| arg == option).unwrap();
            assert_eq!(args[position + 1], value, "{option}");
        }
        assert!(args.contains(&String::from("--no-map-gw")));
        assert_eq!(args.last().unwrap(), "4321");
    }
}
