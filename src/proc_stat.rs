use anyhow::{Context, Result};
use nix::unistd::Pid;
use std::fs;

/// When `pid` started, in clock ticks after boot.
///
/// A PID can belong to another process later, but the start time of a PID changes with it. So
/// the PID and the start time together name one process.
pub fn start_time(pid: Pid) -> Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    start_time_in(&stat).with_context(|| format!("parse {path}"))
}

fn start_time_in(stat: &str) -> Result<u64> {
    // The start time is field 22. Field 2 is the command name in parentheses, which can contain
    // spaces and parentheses itself, so the fields are counted after the last ')', from field 3.
    let (_, fields) = stat.rsplit_once(')').context("no command name")?;
    let start = fields
        .split_whitespace()
        .nth(22 - 3)
        .context("no start time")?;
    start
        .parse()
        .with_context(|| format!("parse start time {start:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "1234 (sleep) S 1 1234 1234 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 \
                        9150231 2453504 128 18446744073709551615";

    #[test]
    fn reads_the_start_time() {
        assert_eq!(start_time_in(STAT).unwrap(), 9_150_231);
    }

    #[test]
    fn reads_the_start_time_after_a_command_name_with_parentheses() {
        let stat = STAT.replace("(sleep)", "(a) S 1 (b c)");
        assert_eq!(start_time_in(&stat).unwrap(), 9_150_231);
    }

    #[test]
    fn rejects_a_short_line() {
        assert!(start_time_in("1234 (sleep) S 1").is_err());
        assert!(start_time_in("1234 sleep").is_err());
    }

    #[test]
    fn reads_the_start_time_of_this_process() {
        assert!(start_time(Pid::this()).unwrap() > 0);
    }
}
