use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::unistd::Pid;
use std::fs;
use std::io;

/// When `pid` started, in clock ticks after boot.
///
/// A PID can belong to another process later, but the start time of a PID changes with it. So
/// the PID and the start time together name one process.
pub fn start_time(pid: Pid) -> Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    Ok(parse(&stat)
        .with_context(|| format!("parse {path}"))?
        .start_time)
}

/// Whether the process `pid` that started at `start_time` still runs.
///
/// A zombie counts as ended, because it only waits for its parent to collect its exit status.
pub fn is_running(pid: Pid, start_time: u64) -> Result<bool> {
    let path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&path) {
        Err(error) if is_gone(&error) => return Ok(false),
        result => result.with_context(|| format!("read {path}"))?,
    };
    let stat = parse(&stat).with_context(|| format!("parse {path}"))?;
    Ok(stat.start_time == start_time && !matches!(stat.state, "Z" | "X"))
}

/// Whether a read of `/proc/<pid>` failed because the process does not exist.
fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(Errno::ESRCH as i32)
}

/// The fields of `/proc/<pid>/stat` that the runtime uses.
#[derive(Debug, PartialEq, Eq)]
struct Stat<'a> {
    state: &'a str,
    start_time: u64,
}

fn parse(stat: &str) -> Result<Stat<'_>> {
    // Field 2 is the command name in parentheses, which can contain spaces and parentheses
    // itself, so the fields are counted after the last ')'. The state is field 3, and the
    // start time is field 22.
    let (_, fields) = stat.rsplit_once(')').context("no command name")?;
    let mut fields = fields.split_whitespace();
    let state = fields.next().context("no state")?;
    let start = fields.nth(22 - 4).context("no start time")?;
    let start_time = start
        .parse()
        .with_context(|| format!("parse start time {start:?}"))?;
    Ok(Stat { state, start_time })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "1234 (sleep) S 1 1234 1234 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 \
                        9150231 2453504 128 18446744073709551615";

    #[test]
    fn reads_the_state_and_the_start_time() {
        let expected = Stat {
            state: "S",
            start_time: 9_150_231,
        };
        assert_eq!(parse(STAT).unwrap(), expected);
    }

    #[test]
    fn reads_the_fields_after_a_command_name_with_parentheses() {
        let stat = STAT.replace("(sleep)", "(a) S 1 (b c)");
        assert_eq!(parse(&stat).unwrap().start_time, 9_150_231);
    }

    #[test]
    fn rejects_a_short_line() {
        assert!(parse("1234 (sleep) S 1").is_err());
        assert!(parse("1234 sleep").is_err());
    }

    #[test]
    fn tells_this_process_from_one_with_another_start_time() {
        let this = Pid::this();
        let start_time = start_time(this).unwrap();
        assert!(is_running(this, start_time).unwrap());
        assert!(!is_running(this, start_time + 1).unwrap());
    }

    #[test]
    fn reports_an_ended_process_as_not_running() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let start_time = start_time(pid).unwrap();
        child.wait().unwrap();
        assert!(!is_running(pid, start_time).unwrap());
    }
}
