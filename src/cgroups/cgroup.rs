use anyhow::{Context, Result, bail};
use rand::random;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

const REQUIRED_CONTROLLERS: [&str; 3] = ["memory", "cpu", "pids"];

fn missing_controllers(active: &str) -> Vec<&'static str> {
    let active: Vec<&str> = active.split_whitespace().collect();
    REQUIRED_CONTROLLERS
        .into_iter()
        .filter(|controller| !active.contains(controller))
        .collect()
}

fn check_delegated_controllers(delegated: &str) -> Result<()> {
    let delegated: Vec<&str> = delegated.split_whitespace().collect();
    for controller in REQUIRED_CONTROLLERS {
        if !delegated.contains(&controller) {
            bail!(
                "controller {controller} not delegated to new cgroup (check parent's cgroup.subtree_control)"
            );
        }
    }
    Ok(())
}

// Resource limits for the container's cgroup.
pub struct CgroupSettings {
    pub cpu: Option<CpuMax>,
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
    // Create a new cgroup for the container.
    pub fn new() -> Result<Self> {
        // controllers = resources
        let cgroup_path: &Path = Path::new("/sys/fs/cgroup");
        let subtree_path: &Path = Path::new("/sys/fs/cgroup/cgroup.subtree_control");

        // Check that cgroupfs is mounted as v2.
        let v2_marker = cgroup_path.join("cgroup.controllers");
        if !v2_marker.is_file() {
            bail!("cgroupfs is not v2 (or not mounted)");
        }

        // cgroup operations require root privileges.
        if !nix::unistd::geteuid().is_root() {
            bail!("cgroup operations require root privileges");
        }

        // Check whether subtree_control delegates the memory, cpu, and pids controllers to children.
        let controllers: String = fs::read_to_string(subtree_path)?;
        let missing_controllers = missing_controllers(&controllers);

        // Enable any missing controllers on the parent cgroup.
        if !missing_controllers.is_empty() {
            let payload = missing_controllers
                .iter()
                .map(|c| format!("+{}", c))
                .collect::<Vec<_>>()
                .join(" ");
            fs::write(subtree_path, payload).context("failed to enable controllers")?;
        }

        Self::create_cgroup_dir(cgroup_path)
    }

    fn create_cgroup_dir(cgroup_path: &Path) -> Result<Self> {
        // Create a new cgroup subdirectory for this container.
        let id = format!("rust_container_{:x}", random::<u64>());
        let new_container_cgroup = cgroup_path.join(&id);
        fs::create_dir(&new_container_cgroup).context("create cgroup dir")?;
        let cgroup = Cgroup {
            path: new_container_cgroup,
        };

        // Verify the required controllers were delegated to the new cgroup.
        let delegated = fs::read_to_string(cgroup.path.join("cgroup.controllers"))
            .context("read new cgroup.controllers")?;
        check_delegated_controllers(&delegated)?;

        Ok(cgroup)
    }

    pub fn configure(&self, settings: &CgroupSettings) -> Result<()> {
        if let Some(cpu) = &settings.cpu {
            self.set_cpu_max(cpu.quota_us.get(), cpu.period_us)?;
        }
        if let Some(bytes) = settings.memory_max {
            self.set_memory_max(bytes)?;
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
    use nix::unistd::Pid;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("container-runtime-cgroup-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_only_missing_controllers_in_required_order() {
        assert_eq!(missing_controllers("cpu io\npids"), vec!["memory"]);
        assert_eq!(
            missing_controllers("memory cpu pids io"),
            Vec::<&str>::new()
        );
        assert_eq!(missing_controllers("memoryful cpu"), vec!["memory", "pids"]);
    }

    #[test]
    fn rejects_a_missing_delegated_controller() {
        assert!(check_delegated_controllers("cpu memory pids io").is_ok());
        let error = check_delegated_controllers("cpu memory").unwrap_err();
        assert!(error.to_string().contains("controller pids not delegated"));
    }

    #[test]
    fn removes_new_cgroup_when_controller_read_fails() {
        // Unlike cgroupfs, a normal directory has no cgroup.controllers file.
        let temp = TempDir::new();
        let error = Cgroup::create_cgroup_dir(&temp.0).err().unwrap();
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
            fs::read_to_string(path.join("cpu.max")).unwrap(),
            "5000 10000"
        );
        assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "64");

        for file in ["cgroup.procs", "memory.max", "cpu.max", "pids.max"] {
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
