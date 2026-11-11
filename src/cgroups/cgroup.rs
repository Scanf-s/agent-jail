use crate::container_id::ContainerId;
use anyhow::{Context, Result, bail};
use nix::unistd::{AccessFlags, Pid, access};
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const REQUIRED_CONTROLLERS: [&str; 3] = ["memory", "cpu", "pids"];
// The runtime moves itself here so that the delegated cgroup holds no process.
const RUNTIME_LEAF: &str = "runtime";

fn check_controllers(available: &str) -> Result<()> {
    let available: Vec<&str> = available.split_whitespace().collect();
    for controller in REQUIRED_CONTROLLERS {
        if !available.contains(&controller) {
            bail!("controller {controller} is not available");
        }
    }
    Ok(())
}

/// Return the path of the cgroup v2 entry (`0::<path>`) in `/proc/<pid>/cgroup`,
/// relative to the cgroup root.
fn unified_cgroup_path(proc_cgroup: &str) -> Result<PathBuf> {
    proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| PathBuf::from(path.trim_start_matches('/')))
        .context("no cgroup v2 entry in /proc/self/cgroup")
}

/// Return the absolute path of the cgroup that this process belongs to.
pub fn own_cgroup() -> Result<PathBuf> {
    let root = Path::new(CGROUP_ROOT);
    if !root.join("cgroup.controllers").is_file() {
        bail!("cgroupfs is not v2 (or not mounted)");
    }
    let own = fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
    Ok(root.join(unified_cgroup_path(&own)?))
}

/// Check that the runtime can create the container cgroup under `dir`.
///
/// `dir` must be writable by this user and must hold no other process, because a cgroup that
/// holds processes cannot pass controllers to its children.
pub fn check_delegated(dir: &Path, runtime: Pid) -> Result<()> {
    access(&dir.join("cgroup.subtree_control"), AccessFlags::W_OK)
        .with_context(|| format!("cgroup {dir:?} is not delegated to this user"))?;

    let procs = fs::read_to_string(dir.join("cgroup.procs"))
        .with_context(|| format!("read {dir:?}/cgroup.procs"))?;
    if procs
        .split_whitespace()
        .any(|pid| pid != runtime.to_string())
    {
        bail!("cgroup {dir:?} holds processes other than the runtime");
    }

    let available = fs::read_to_string(dir.join("cgroup.controllers"))
        .with_context(|| format!("read {dir:?}/cgroup.controllers"))?;
    check_controllers(&available).with_context(|| format!("cgroup {dir:?}"))
}

// Resource limits for the container's cgroup.
pub struct CgroupSettings {
    pub cpu: Option<CpuMax>,
    /// RAM limit in bytes. Setting it also stops the container from using swap.
    pub memory_max: Option<u64>,
    pub pids_max: Option<u64>,
}

// CPU time the cgroup may use, in microseconds, within each period.
pub struct CpuMax {
    pub quota_us: NonZeroU64,
    pub period_us: u64,
}

impl CpuMax {
    pub const DEFAULT_PERIOD_US: u64 = 100_000; // 100ms
}

pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    /// Create the container cgroup inside `delegated`, the cgroup that the runtime runs in.
    pub fn new(delegated: &Path, id: &ContainerId) -> Result<Self> {
        Self::create_in(delegated, Pid::this(), id)
    }

    fn create_in(delegated: &Path, runtime: Pid, id: &ContainerId) -> Result<Self> {
        check_delegated(delegated, runtime)?;

        let leaf = delegated.join(RUNTIME_LEAF);
        fs::create_dir(&leaf).with_context(|| format!("create {leaf:?}"))?;
        fs::write(leaf.join("cgroup.procs"), runtime.to_string())
            .with_context(|| format!("move the runtime into {leaf:?}"))?;

        fs::write(
            delegated.join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        )
        .with_context(|| format!("enable controllers in {delegated:?}"))?;

        Self::create_cgroup_dir(delegated, id)
    }

    fn create_cgroup_dir(parent: &Path, id: &ContainerId) -> Result<Self> {
        let path = parent.join(format!("jail_{id}"));
        fs::create_dir(&path).context("create cgroup dir")?;
        // From here on, Drop removes the directory if a later check fails.
        let cgroup = Cgroup { path };

        let available = fs::read_to_string(cgroup.path.join("cgroup.controllers"))
            .context("read new cgroup.controllers")?;
        check_controllers(&available).context("new container cgroup")?;

        Ok(cgroup)
    }

    pub fn configure(&self, settings: &CgroupSettings) -> Result<()> {
        if let Some(cpu) = &settings.cpu {
            self.set_cpu_max(cpu.quota_us.get(), cpu.period_us)?;
        }
        if let Some(bytes) = settings.memory_max {
            self.set_memory_max(bytes)?;
            // memory.max limits only RAM. Without this, the kernel moves the excess to swap
            // instead of stopping the container at the limit.
            self.set_swap_max(0)?;
        }
        if let Some(pids) = settings.pids_max {
            self.set_pids_max(pids)?;
        }
        Ok(())
    }

    pub fn add_pid(&self, pid: nix::unistd::Pid) -> Result<()> {
        fs::write(self.path.join("cgroup.procs"), pid.to_string()).context("write cgroup.procs")?;
        Ok(())
    }

    fn set_memory_max(&self, bytes: u64) -> Result<()> {
        fs::write(self.path.join("memory.max"), bytes.to_string()).context("write memory.max")?;
        Ok(())
    }

    fn set_swap_max(&self, bytes: u64) -> Result<()> {
        fs::write(self.path.join("memory.swap.max"), bytes.to_string())
            .context("write memory.swap.max")?;
        Ok(())
    }

    fn set_cpu_max(&self, quota_us: u64, period_us: u64) -> Result<()> {
        fs::write(self.path.join("cpu.max"), format!("{quota_us} {period_us}"))
            .context("write cpu.max")?;
        Ok(())
    }

    fn set_pids_max(&self, pids: u64) -> Result<()> {
        fs::write(self.path.join("pids.max"), pids.to_string()).context("write pids.max")?;
        Ok(())
    }
}

// Remove the cgroup directory when the handle is dropped.
impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;
    use std::os::unix::fs::PermissionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("agent-jail-cgroup-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A directory laid out like a delegated cgroup that holds only `runtime`.
    fn delegated_dir(runtime: Pid) -> TempDir {
        let temp = TempDir::new();
        fs::write(temp.0.join("cgroup.subtree_control"), "").unwrap();
        fs::write(temp.0.join("cgroup.procs"), format!("{runtime}\n")).unwrap();
        fs::write(temp.0.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        temp
    }

    #[test]
    fn reads_the_unified_cgroup_path() {
        let path = unified_cgroup_path("0::/user.slice/run-1.scope\n").unwrap();
        assert_eq!(path, PathBuf::from("user.slice/run-1.scope"));

        let hybrid = "1:name=systemd:/other\n0::/app.slice\n";
        assert_eq!(
            unified_cgroup_path(hybrid).unwrap(),
            PathBuf::from("app.slice")
        );

        assert!(unified_cgroup_path("1:name=systemd:/other\n").is_err());
    }

    #[test]
    fn rejects_a_missing_controller() {
        assert!(check_controllers("cpu memory pids io").is_ok());
        let error = check_controllers("cpu memoryful").unwrap_err();
        assert!(error.to_string().contains("controller memory"), "{error:#}");
    }

    #[test]
    fn accepts_a_delegated_cgroup_that_holds_only_the_runtime() {
        let runtime = Pid::from_raw(1234);
        let temp = delegated_dir(runtime);
        check_delegated(&temp.0, runtime).unwrap();
    }

    #[test]
    fn rejects_a_cgroup_that_holds_other_processes() {
        let runtime = Pid::from_raw(1234);
        let temp = delegated_dir(runtime);
        fs::write(temp.0.join("cgroup.procs"), "1234\n5678\n").unwrap();
        let error = check_delegated(&temp.0, runtime).unwrap_err();
        assert!(error.to_string().contains("holds processes"), "{error:#}");
    }

    #[test]
    fn rejects_a_cgroup_without_required_controllers() {
        let runtime = Pid::from_raw(1234);
        let temp = delegated_dir(runtime);
        fs::write(temp.0.join("cgroup.controllers"), "cpu memory\n").unwrap();
        let error = check_delegated(&temp.0, runtime).unwrap_err();
        assert!(
            format!("{error:#}").contains("controller pids"),
            "{error:#}"
        );
    }

    #[test]
    fn rejects_a_cgroup_that_this_user_cannot_write() {
        // Root bypasses file permissions, so this case cannot be reproduced.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let runtime = Pid::from_raw(1234);
        let temp = delegated_dir(runtime);
        let control = temp.0.join("cgroup.subtree_control");
        fs::set_permissions(&control, fs::Permissions::from_mode(0o444)).unwrap();
        let error = check_delegated(&temp.0, runtime).unwrap_err();
        assert!(error.to_string().contains("not delegated"), "{error:#}");
    }

    #[test]
    fn moves_the_runtime_out_before_enabling_controllers() {
        let runtime = Pid::from_raw(1234);
        let temp = delegated_dir(runtime);

        // A plain directory has no cgroup.controllers in the new child, so creation stops there.
        let error = Cgroup::create_in(&temp.0, runtime, &ContainerId::generate())
            .err()
            .unwrap();
        assert!(error.to_string().contains("read new cgroup.controllers"));

        let leaf = temp.0.join(RUNTIME_LEAF);
        assert_eq!(
            fs::read_to_string(leaf.join("cgroup.procs")).unwrap(),
            "1234"
        );
        assert_eq!(
            fs::read_to_string(temp.0.join("cgroup.subtree_control")).unwrap(),
            "+cpu +memory +pids"
        );
    }

    #[test]
    fn removes_new_cgroup_when_controller_read_fails() {
        // Unlike cgroupfs, a normal directory has no cgroup.controllers file.
        let temp = TempDir::new();
        let error = Cgroup::create_cgroup_dir(&temp.0, &ContainerId::generate())
            .err()
            .unwrap();
        assert!(error.to_string().contains("read new cgroup.controllers"));

        // The new cgroup directory should be removed when setup fails.
        assert_eq!(
            fs::read_dir(&temp.0).unwrap().count(),
            0,
            "failed cgroup setup left a directory behind"
        );
    }

    #[test]
    fn writes_pid_and_limits_to_cgroup_files() {
        let temp = TempDir::new();
        let path = temp.0.join("cgroup");
        fs::create_dir(&path).unwrap();
        let cgroup = Cgroup { path: path.clone() };

        let settings = CgroupSettings {
            cpu: Some(CpuMax {
                quota_us: NonZeroU64::new(5_000).unwrap(),
                period_us: 10_000,
            }),
            memory_max: Some(512 * 1024 * 1024),
            pids_max: Some(64),
        };
        cgroup.add_pid(Pid::from_raw(1234)).unwrap();
        cgroup.configure(&settings).unwrap();

        assert_eq!(
            fs::read_to_string(path.join("cgroup.procs")).unwrap(),
            "1234"
        );
        assert_eq!(
            fs::read_to_string(path.join("memory.max")).unwrap(),
            "536870912"
        );
        assert_eq!(
            fs::read_to_string(path.join("memory.swap.max")).unwrap(),
            "0"
        );
        assert_eq!(
            fs::read_to_string(path.join("cpu.max")).unwrap(),
            "5000 10000"
        );
        assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "64");

        for file in [
            "cgroup.procs",
            "memory.max",
            "memory.swap.max",
            "cpu.max",
            "pids.max",
        ] {
            fs::remove_file(path.join(file)).unwrap();
        }
        drop(cgroup);
        assert!(!path.exists());
    }

    #[test]
    fn leaves_unspecified_limits_unwritten() {
        let temp = TempDir::new();
        let cgroup = Cgroup {
            path: temp.0.clone(),
        };
        let settings = CgroupSettings {
            cpu: None,
            memory_max: None,
            pids_max: Some(64),
        };
        cgroup.configure(&settings).unwrap();

        assert!(!temp.0.join("cpu.max").exists());
        assert!(!temp.0.join("memory.max").exists());
        assert!(!temp.0.join("memory.swap.max").exists());
        assert_eq!(fs::read_to_string(temp.0.join("pids.max")).unwrap(), "64");
    }

    #[test]
    fn reports_the_file_when_a_limit_write_fails() {
        let temp = TempDir::new();
        let cgroup = Cgroup {
            path: temp.0.join("missing"),
        };
        let error = cgroup.set_memory_max(1).unwrap_err();
        assert!(error.to_string().contains("write memory.max"));
    }
}
