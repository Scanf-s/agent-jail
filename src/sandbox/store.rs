use super::{SandboxName, SandboxRecord};
use crate::paths;
use crate::state::local_time;
use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::fcntl::{RenameFlags, renameat2};
use rand::random;
use serde_json::Value;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const RECORD_FILE: &str = "sandbox.json";
const UPPER: &str = "upper";
const WORK: &str = "work";

/// `$XDG_DATA_HOME/agent-jail/sandboxes`.
pub fn sandboxes_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("sandboxes"))
}

/// Create the directory of a new sandbox under `root`, with `record` and empty upper and work
/// directories. Returns the directory.
pub fn create_in(root: &Path, record: &SandboxRecord) -> Result<PathBuf> {
    // The record can hold values that the user gave with --env, so only the user may read it.
    private_dir()
        .recursive(true)
        .create(root)
        .with_context(|| format!("create {root:?}"))?;
    // The directory gets the name of the sandbox only when it is complete, so a reader never
    // finds it without its record. No sandbox name starts with '.'.
    let temporary = root.join(format!(".creating-{:016x}", random::<u64>()));
    private_dir()
        .create(&temporary)
        .with_context(|| format!("create {temporary:?}"))?;
    let path = root.join(&record.name);
    let result = fill(&temporary, record).and_then(|()| {
        match renameat2(None, &temporary, None, &path, RenameFlags::RENAME_NOREPLACE) {
            Err(Errno::EEXIST) => bail!("a sandbox named {} already exists", record.name),
            result => result.with_context(|| format!("rename {temporary:?} to {path:?}")),
        }
    });
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    Ok(path)
}

fn fill(dir: &Path, record: &SandboxRecord) -> Result<()> {
    let json = serde_json::to_vec_pretty(record).context("serialize the sandbox record")?;
    let file = dir.join(RECORD_FILE);
    fs::write(&file, json).with_context(|| format!("write {file:?}"))?;
    // The mode of the upper directory becomes the mode of / in the container, so it keeps the
    // default mode that lets every user of the container enter /.
    for name in [UPPER, WORK] {
        let sub = dir.join(name);
        fs::create_dir(&sub).with_context(|| format!("create {sub:?}"))?;
    }
    Ok(())
}

/// The record of the sandbox in `dir`.
pub fn load(dir: &Path) -> Result<SandboxRecord> {
    let file = dir.join(RECORD_FILE);
    let json = fs::read(&file).with_context(|| format!("read {file:?}"))?;
    let value: Value = serde_json::from_slice(&json).with_context(|| format!("parse {file:?}"))?;
    let version = value.get("recordVersion").and_then(Value::as_u64);
    if version.is_some_and(|version| version > u64::from(SandboxRecord::VERSION)) {
        bail!("{file:?} was written by a later version of aj");
    }
    let record: SandboxRecord =
        serde_json::from_value(value).with_context(|| format!("parse {file:?}"))?;
    let name = dir.file_name().and_then(|name| name.to_str());
    if name != Some(record.name.as_str()) {
        bail!("{file:?} names the sandbox {:?}", record.name);
    }
    Ok(record)
}

/// The records of all sandboxes under `root`, oldest first. A sandbox that cannot be read is
/// left out with a warning.
pub fn list_in(root: &Path) -> Result<Vec<SandboxRecord>> {
    let entries = match fs::read_dir(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.with_context(|| format!("read {root:?}"))?,
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {root:?}"))?;
        // A directory that is being created has a temporary name, which is no sandbox name.
        let is_name = entry
            .file_name()
            .to_str()
            .is_some_and(|name| SandboxName::parse(name).is_ok());
        if !is_name {
            continue;
        }
        match paths::is_directory(&entry) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                report!("aj: warning: read {:?}: {error}", entry.path());
                continue;
            }
        }
        match load(&entry.path()) {
            Ok(record) => records.push(record),
            Err(error) => report!("aj: warning: {error:#}"),
        }
    }
    records.sort_by(|a, b| (a.created, &a.name).cmp(&(b.created, &b.name)));
    Ok(records)
}

/// Format `records` as the table that `aj sandbox list` prints.
pub fn table(records: &[SandboxRecord]) -> String {
    let name_width = records
        .iter()
        .map(|record| record.name.len())
        .chain([4])
        .max()
        .unwrap_or(4);
    let mut table = format!("{:<name_width$}  {:<12}  CREATED\n", "NAME", "IMAGE");
    for record in records {
        table += &format!(
            "{:<name_width$}  {:<12}  {}\n",
            record.name,
            record.image.short(),
            local_time(record.created),
        );
    }
    table
}

fn private_dir() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(test)]
mod tests {
    use super::super::record::tests::{mapping, record};
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("agent-jail-sandboxes-{:x}", random::<u64>())))
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_a_sandbox_and_reads_it_back() {
        let root = TempRoot::new();
        let record = record("box", &mapping(100_000));

        let dir = create_in(&root.0, &record).unwrap();

        assert_eq!(dir, root.0.join("box"));
        assert_eq!(load(&dir).unwrap(), record);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(UPPER)), 0o777 & !current_umask());
        assert!(dir.join(WORK).is_dir());
        assert_eq!(list_in(&root.0).unwrap(), [record]);
    }

    #[test]
    fn rejects_a_name_in_use_and_leaves_no_temporary_directory() {
        let root = TempRoot::new();
        create_in(&root.0, &record("box", &mapping(100_000))).unwrap();

        let error = create_in(&root.0, &record("box", &mapping(100_000))).unwrap_err();

        assert!(error.to_string().contains("already exists"), "{error}");
        let names: Vec<_> = fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["box"]);
    }

    #[test]
    fn lists_only_complete_sandboxes_oldest_first() {
        let root = TempRoot::new();
        let mut older = record("zeta", &mapping(100_000));
        older.created -= 10;
        create_in(&root.0, &older).unwrap();
        create_in(&root.0, &record("alpha", &mapping(100_000))).unwrap();
        // A creation that was killed leaves its temporary directory.
        fs::create_dir(root.0.join(".creating-1")).unwrap();

        let names: Vec<_> = list_in(&root.0)
            .unwrap()
            .into_iter()
            .map(|record| record.name)
            .collect();

        assert_eq!(names, ["zeta", "alpha"]);
        assert!(list_in(&root.0.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn rejects_a_record_of_a_later_version() {
        let root = TempRoot::new();
        let dir = create_in(&root.0, &record("box", &mapping(100_000))).unwrap();
        let file = dir.join(RECORD_FILE);
        let mut value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        value["recordVersion"] = Value::from(SandboxRecord::VERSION + 1);
        fs::write(&file, value.to_string()).unwrap();

        let error = load(&dir).unwrap_err();

        assert!(error.to_string().contains("later version"), "{error}");
    }

    /// The umask, read without changing it, because the tests run in parallel threads.
    fn current_umask() -> u32 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .unwrap()
            .trim();
        u32::from_str_radix(mask, 8).unwrap()
    }
}
