use super::Record;
use crate::container_id::ContainerId;
use crate::flock::{LockKind, try_lock};
use crate::paths;
use anyhow::{Context, Result, bail};
use nix::libc;
use oci_spec::runtime::ContainerState;
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;

/// The record of container `id` under `root`, with the status that its processes show now.
pub fn read(root: &Path, id: &ContainerId) -> Result<Record> {
    let dir = root.join(id.as_str());
    if !dir.is_dir() {
        bail!("there is no container with ID {id} in {root:?}");
    }
    with_current_status(&dir, Record::load(&dir)?)
}

/// The records of all containers under `root`, as `read` returns them, oldest first.
///
/// A directory that its run removes while this reads it is left out. A directory that cannot
/// be read is left out with a warning.
pub fn list(root: &Path) -> Result<Vec<Record>> {
    let entries = match fs::read_dir(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.with_context(|| format!("read {root:?}"))?,
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {root:?}"))?;
        let dir = entry.path();
        // A directory that a runtime is creating has a temporary name, which is no valid ID.
        let is_id = entry
            .file_name()
            .to_str()
            .is_some_and(|name| ContainerId::parse(name).is_ok());
        match paths::is_directory(&entry) {
            Ok(true) if is_id => {}
            Ok(_) => continue,
            Err(error) => {
                report!("aj: warning: read {dir:?}: {error}");
                continue;
            }
        }
        match Record::load(&dir).and_then(|record| with_current_status(&dir, record)) {
            Ok(record) => records.push(record),
            Err(error) if is_not_found(&error) => {}
            Err(error) => report!("aj: warning: {error:#}"),
        }
    }
    records.sort_by(|a, b| (a.created, a.oci.id()).cmp(&(b.created, b.oci.id())));
    Ok(records)
}

/// Whether `error` comes from a file that does not exist.
fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

/// Replace the recorded status with the status that the processes show now.
///
/// A killed runtime cannot record that its container stopped, so a recorded status can be out
/// of date. OCI reports a PID only while the container is created or running.
fn with_current_status(dir: &Path, mut record: Record) -> Result<Record> {
    let status = current_status(*record.oci.status(), record.init_runs()?, lock_held(dir)?);
    record.oci.set_status(status);
    if status == ContainerState::Stopped {
        record.oci.set_pid(None);
    }
    Ok(record)
}

/// The status of a container whose state file shows `recorded`.
///
/// `init_runs` tells whether the init in the record still runs, and `lock_held` whether a
/// runtime process of the container is alive.
fn current_status(recorded: ContainerState, init_runs: bool, lock_held: bool) -> ContainerState {
    match recorded {
        // The record names no init yet, so only the runtime processes show that it is alive.
        ContainerState::Creating if lock_held => ContainerState::Creating,
        // Init waits for its setup process to start the program, and fails when that process
        // ends. Until then, the container cannot start.
        ContainerState::Created if init_runs && lock_held => ContainerState::Created,
        // Once init has ended, the program has ended, even if the runtime still cleans up.
        ContainerState::Running if init_runs => ContainerState::Running,
        _ => ContainerState::Stopped,
    }
}

/// Whether a runtime process of the container in `dir` holds its exclusive lock.
///
/// The check takes a shared lock, which never excludes another check, so two commands that
/// check at the same time both see the truth.
pub(super) fn lock_held(dir: &Path) -> Result<bool> {
    let file = File::open(dir).with_context(|| format!("open {dir:?}"))?;
    let probe = try_lock(file, LockKind::Shared).with_context(|| format!("lock {dir:?}"))?;
    Ok(probe.is_none())
}

/// Format `records` as the table that `aj list` prints.
pub fn table(records: &[Record]) -> String {
    let id_width = records
        .iter()
        .map(|record| record.oci.id().len())
        .chain([2])
        .max()
        .unwrap_or(2);
    let mut table = format!(
        "{:<id_width$}  {:<7}  {:<8}  CREATED\n",
        "ID", "PID", "STATUS"
    );
    for record in records {
        let pid = record
            .oci
            .pid()
            .map_or_else(|| "-".to_owned(), |pid| pid.to_string());
        table += &format!(
            "{:<id_width$}  {pid:<7}  {:<8}  {}\n",
            record.oci.id(),
            record.oci.status().to_string(),
            local_time(record.created),
        );
    }
    table
}

/// `seconds` after the Unix epoch as local time, in the form `2026-11-11 14:12:30`.
fn local_time(seconds: u64) -> String {
    let Ok(time) = libc::time_t::try_from(seconds) else {
        return seconds.to_string();
    };
    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r only writes the broken-down time into `tm`, and returns null if it
    // cannot convert `time`.
    if unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) }.is_null() {
        return seconds.to_string();
    }
    // SAFETY: localtime_r succeeded, so it filled in `tm`.
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc_stat;
    use crate::state::{STATE_FILE, StateDir};
    use nix::unistd::Pid;
    use rand::random;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("agent-jail-query-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        /// Write a state directory that no runtime process holds.
        fn leave(&self, record: &Record) {
            let dir = self.0.join(record.oci.id());
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join(STATE_FILE), serde_json::to_vec(record).unwrap()).unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn id(value: &str) -> ContainerId {
        ContainerId::parse(value).unwrap()
    }

    fn record(name: &str, created: u64) -> Record {
        let mut record = Record::creating(
            &id(name),
            PathBuf::from("/no/cgroup"),
            PathBuf::from("/bundle"),
            None,
        );
        record.created = created;
        record
    }

    /// Make `record` name a running init: this test process, with its start time plus `offset`.
    fn running(mut record: Record, offset: u64) -> Record {
        let this = Pid::this();
        record
            .oci
            .set_status(ContainerState::Running)
            .set_pid(Some(this.as_raw()));
        record.init_start_time = Some(proc_stat::start_time(this).unwrap() + offset);
        record
    }

    #[test]
    fn reports_the_status_that_the_processes_show() {
        use ContainerState::{Created, Creating, Running, Stopped};
        let cases = [
            (Creating, false, true, Creating),
            (Creating, false, false, Stopped),
            (Created, true, true, Created),
            (Created, true, false, Stopped),
            (Created, false, true, Stopped),
            (Running, true, true, Running),
            (Running, true, false, Running),
            (Running, false, true, Stopped),
            (Running, false, false, Stopped),
            (Stopped, true, true, Stopped),
        ];
        for (recorded, init_runs, lock_held, expected) in cases {
            assert_eq!(
                current_status(recorded, init_runs, lock_held),
                expected,
                "{recorded:?}, init runs: {init_runs}, lock held: {lock_held}"
            );
        }
    }

    #[test]
    fn reads_a_live_and_a_killed_container() {
        let root = TempRoot::new();
        let _live = StateDir::create(&root.0, &id("live"), record("live", 1)).unwrap();
        root.leave(&running(record("killed", 2), 1));

        assert_eq!(
            *read(&root.0, &id("live")).unwrap().oci.status(),
            ContainerState::Creating
        );
        let killed = read(&root.0, &id("killed")).unwrap();
        assert_eq!(*killed.oci.status(), ContainerState::Stopped);
        assert_eq!(*killed.oci.pid(), None);

        let error = read(&root.0, &id("missing")).unwrap_err();
        assert!(error.to_string().contains("no container"), "{error:#}");
    }

    #[test]
    fn lists_the_readable_containers_oldest_first() {
        let root = TempRoot::new();
        root.leave(&running(record("new", 30), 0));
        root.leave(&record("old", 10));
        fs::create_dir(root.0.join("no-state")).unwrap();
        fs::create_dir(root.0.join("broken")).unwrap();
        fs::write(root.0.join("broken").join(STATE_FILE), "{").unwrap();
        // A runtime that is creating a state directory gives it a name that is no valid ID.
        let creating = root.0.join(".creating-0123456789abcdef");
        fs::create_dir(&creating).unwrap();
        fs::write(
            creating.join(STATE_FILE),
            serde_json::to_vec(&record("creating", 20)).unwrap(),
        )
        .unwrap();
        fs::write(root.0.join("note"), "").unwrap();

        let records = list(&root.0).unwrap();

        let ids: Vec<&str> = records
            .iter()
            .map(|record| record.oci.id().as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        assert_eq!(*records[1].oci.status(), ContainerState::Running);
        assert!(list(&root.0.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn formats_the_list_as_a_table() {
        let mut long = record("a-long-container-id", 0);
        long.oci
            .set_status(ContainerState::Running)
            .set_pid(Some(48213));
        let mut short = record("box", 0);
        short.oci.set_status(ContainerState::Stopped);

        let output = table(&[long, short]);

        let time = local_time(0);
        assert_eq!(
            output,
            format!(
                "ID                   PID      STATUS    CREATED\n\
                 a-long-container-id  48213    running   {time}\n\
                 box                  -        stopped   {time}\n"
            )
        );
        assert_eq!(table(&[]), "ID  PID      STATUS    CREATED\n");
    }

    #[test]
    fn formats_the_local_time() {
        let time = local_time(1_762_838_400);
        assert_eq!(time.len(), "2026-11-11 14:12:30".len(), "{time}");
        assert!(time.starts_with("2025-11-1"), "{time}");
    }
}
