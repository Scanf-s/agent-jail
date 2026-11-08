use super::Bundle;
use crate::cgroups::{CgroupSettings, CpuMax};
use crate::config::{ContainerConfig, ProcessConfig};
use anyhow::{Context, Result, bail, ensure};
use nix::unistd::{Gid, Uid};
use oci_spec::runtime::{
    Linux, LinuxCpu, LinuxIdMapping, LinuxMemory, LinuxNamespace, LinuxNamespaceType, LinuxPids,
    LinuxResources, Mount, Process, Root, Spec,
};
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const SUPPORTED_OCI_VERSION: &str = "1.3.0";

// The runtime always creates exactly these namespaces.
const REQUIRED_NAMESPACES: [LinuxNamespaceType; 4] = [
    LinuxNamespaceType::User,
    LinuxNamespaceType::Pid,
    LinuxNamespaceType::Network,
    LinuxNamespaceType::Mount,
];

// Container init always mounts proc at /proc with these flags.
const PROC_MOUNT_OPTIONS: [&str; 3] = ["nosuid", "nodev", "noexec"];

// The kernel rejects other values in cpu.max.
const CPU_PERIOD_RANGE_US: RangeInclusive<u64> = 1_000..=1_000_000;
const MIN_CPU_QUOTA_US: i64 = 1_000;

impl Bundle {
    // Check that this runtime can apply every requested setting, then convert the configuration.
    pub fn into_config(self) -> Result<ContainerConfig> {
        self.into_config_for(Uid::effective().as_raw(), Gid::effective().as_raw())
    }

    // `host_uid` and `host_gid` belong to the user who runs the runtime.
    fn into_config_for(self, host_uid: u32, host_gid: u32) -> Result<ContainerConfig> {
        let spec = &self.spec;
        check_version(spec.version())?;
        reject_unsupported_top_level(spec)?;
        let rootfs = rootfs_path(&self.path, spec.root().as_ref())?;
        check_mounts(spec.mounts().as_deref())?;

        let linux = spec.linux().as_ref().context("linux is required")?;
        check_namespaces(linux.namespaces().as_deref())?;
        reject_unsupported_linux(linux)?;
        check_root_mapping(
            "linux.uidMappings",
            linux.uid_mappings().as_deref(),
            host_uid,
        )?;
        check_root_mapping(
            "linux.gidMappings",
            linux.gid_mappings().as_deref(),
            host_gid,
        )?;
        let process = process_config(spec.process().as_ref())?;
        let resources = cgroup_settings(linux.resources().as_ref())?;

        Ok(ContainerConfig {
            rootfs,
            process,
            resources,
        })
    }
}

fn reject_present(fields: &[(&str, bool)]) -> Result<()> {
    for (name, present) in fields {
        ensure!(!present, "{name} is not supported yet");
    }
    Ok(())
}

fn check_version(version: &str) -> Result<()> {
    ensure!(!version.is_empty(), "ociVersion is required");
    ensure!(
        version == SUPPORTED_OCI_VERSION,
        "ociVersion {version} is not supported; only {SUPPORTED_OCI_VERSION} is supported"
    );
    Ok(())
}

fn reject_unsupported_top_level(spec: &Spec) -> Result<()> {
    reject_present(&[
        ("hostname", spec.hostname().is_some()),
        ("domainname", spec.domainname().is_some()),
        ("hooks", spec.hooks().is_some()),
        ("solaris", spec.solaris().is_some()),
        ("windows", spec.windows().is_some()),
        ("vm", spec.vm().is_some()),
        ("zos", spec.zos().is_some()),
    ])
}

fn rootfs_path(bundle: &Path, root: Option<&Root>) -> Result<PathBuf> {
    let root = root.context("root is required")?;
    ensure!(!root.path().as_os_str().is_empty(), "root.path is required");
    ensure!(
        root.readonly() != Some(true),
        "root.readonly=true is not supported yet"
    );

    // Joining an absolute root.path replaces the bundle path, which matches the specification.
    let rootfs = bundle.join(root.path());
    ensure!(
        rootfs.is_dir(),
        "root.path {} does not exist or is not a directory",
        rootfs.display()
    );
    Ok(rootfs)
}

fn check_mounts(mounts: Option<&[Mount]>) -> Result<()> {
    let unsupported = || {
        format!(
            "mounts must contain only a proc mount at /proc with options {}",
            PROC_MOUNT_OPTIONS.join(",")
        )
    };
    let [mount] = mounts.unwrap_or_default() else {
        bail!(unsupported());
    };

    let mut options: Vec<&str> = mount
        .options()
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();
    options.sort_unstable();
    let mut expected = PROC_MOUNT_OPTIONS;
    expected.sort_unstable();

    ensure!(
        mount.destination() == Path::new("/proc")
            && mount.typ().as_deref() == Some("proc")
            && mount.source().as_deref() == Some(Path::new("proc"))
            && options == expected,
        unsupported()
    );
    reject_present(&[
        ("mounts[0].uidMappings", mount.uid_mappings().is_some()),
        ("mounts[0].gidMappings", mount.gid_mappings().is_some()),
    ])
}

fn check_namespaces(namespaces: Option<&[LinuxNamespace]>) -> Result<()> {
    let namespaces = namespaces.unwrap_or_default();
    for namespace in namespaces {
        ensure!(
            namespace.path().is_none(),
            "linux.namespaces: joining an existing {:?} namespace is not supported yet",
            namespace.typ()
        );
    }

    let matches_required = namespaces.len() == REQUIRED_NAMESPACES.len()
        && REQUIRED_NAMESPACES
            .iter()
            .all(|required| namespaces.iter().any(|ns| ns.typ() == *required));
    ensure!(
        matches_required,
        "linux.namespaces must be exactly {REQUIRED_NAMESPACES:?}"
    );
    Ok(())
}

fn reject_unsupported_linux(linux: &Linux) -> Result<()> {
    reject_present(&[
        ("linux.netDevices", linux.net_devices().is_some()),
        ("linux.sysctl", linux.sysctl().is_some()),
        ("linux.cgroupsPath", linux.cgroups_path().is_some()),
        ("linux.devices", linux.devices().is_some()),
        ("linux.seccomp", linux.seccomp().is_some()),
        (
            "linux.rootfsPropagation",
            linux.rootfs_propagation().is_some(),
        ),
        ("linux.maskedPaths", linux.masked_paths().is_some()),
        ("linux.readonlyPaths", linux.readonly_paths().is_some()),
        ("linux.mountLabel", linux.mount_label().is_some()),
        ("linux.intelRdt", linux.intel_rdt().is_some()),
        ("linux.memoryPolicy", linux.memory_policy().is_some()),
        ("linux.personality", linux.personality().is_some()),
        ("linux.timeOffsets", linux.time_offsets().is_some()),
    ])
}

// Without privilege, the runtime can map container ID 0 only to its own host ID.
fn check_root_mapping(
    field: &str,
    mappings: Option<&[LinuxIdMapping]>,
    host_id: u32,
) -> Result<()> {
    let Some([mapping]) = mappings else {
        bail!("{field} must contain exactly one entry with containerID 0 and size 1");
    };
    ensure!(
        mapping.container_id() == 0 && mapping.size() == 1,
        "{field} must contain exactly one entry with containerID 0 and size 1"
    );
    ensure!(
        mapping.host_id() == host_id,
        "{field}[0].hostID must be {host_id}, the ID of the user who runs the runtime, but got {}",
        mapping.host_id()
    );
    Ok(())
}

fn process_config(process: Option<&Process>) -> Result<ProcessConfig> {
    let process = process.context("process is required to run a program")?;
    ensure!(
        process.terminal() != Some(true),
        "process.terminal=true is not supported yet"
    );
    ensure!(
        process.no_new_privileges() != Some(true),
        "process.noNewPrivileges=true is not supported yet"
    );
    reject_present(&[
        ("process.commandLine", process.command_line().is_some()),
        ("process.capabilities", process.capabilities().is_some()),
        ("process.rlimits", process.rlimits().is_some()),
        (
            "process.apparmorProfile",
            process.apparmor_profile().is_some(),
        ),
        ("process.oomScoreAdj", process.oom_score_adj().is_some()),
        ("process.selinuxLabel", process.selinux_label().is_some()),
        ("process.ioPriority", process.io_priority().is_some()),
        ("process.scheduler", process.scheduler().is_some()),
        (
            "process.execCPUAffinity",
            process.exec_cpu_affinity().is_some(),
        ),
    ])?;

    let user = process.user();
    reject_present(&[
        ("process.user.umask", user.umask().is_some()),
        ("process.user.username", user.username().is_some()),
        (
            "process.user.additionalGids",
            user.additional_gids()
                .as_ref()
                .is_some_and(|gids| !gids.is_empty()),
        ),
    ])?;
    // Only container ID 0 is mapped, so the program must run as container root.
    ensure!(
        user.uid() == 0 && user.gid() == 0,
        "process.user must be uid 0 and gid 0 because only container ID 0 is mapped"
    );

    let args = process.args().clone().unwrap_or_default();
    let program = args.first().context("process.args must not be empty")?;
    ensure!(
        program.starts_with('/'),
        "process.args[0] must be an absolute path but got {program:?}"
    );
    ensure!(
        process.cwd().is_absolute(),
        "process.cwd must be an absolute path but got {}",
        process.cwd().display()
    );
    reject_nul("process.args", &args)?;
    reject_nul("process.env", process.env().as_deref().unwrap_or_default())?;
    ensure!(
        !process.cwd().as_os_str().as_bytes().contains(&0),
        "process.cwd contains a NUL character"
    );

    Ok(ProcessConfig {
        args,
        env: process.env().clone().unwrap_or_default(),
        cwd: process.cwd().clone(),
        uid: user.uid(),
        gid: user.gid(),
    })
}

// exec system calls end each string at the first NUL byte.
fn reject_nul(field: &str, values: &[String]) -> Result<()> {
    if let Some(index) = values.iter().position(|value| value.contains('\0')) {
        bail!("{field}[{index}] contains a NUL character");
    }
    Ok(())
}

fn cgroup_settings(resources: Option<&LinuxResources>) -> Result<CgroupSettings> {
    let Some(resources) = resources else {
        return Ok(CgroupSettings {
            cpu: None,
            memory_max: None,
            pids_max: None,
        });
    };
    reject_present(&[
        ("linux.resources.devices", resources.devices().is_some()),
        ("linux.resources.blockIO", resources.block_io().is_some()),
        (
            "linux.resources.hugepageLimits",
            resources.hugepage_limits().is_some(),
        ),
        ("linux.resources.network", resources.network().is_some()),
        ("linux.resources.rdma", resources.rdma().is_some()),
        ("linux.resources.unified", resources.unified().is_some()),
    ])?;

    Ok(CgroupSettings {
        cpu: resources.cpu().as_ref().map(cpu_max).transpose()?.flatten(),
        memory_max: resources
            .memory()
            .as_ref()
            .map(memory_max)
            .transpose()?
            .flatten(),
        pids_max: resources.pids().as_ref().map(pids_max).transpose()?,
    })
}

fn cpu_max(cpu: &LinuxCpu) -> Result<Option<CpuMax>> {
    reject_present(&[
        ("linux.resources.cpu.shares", cpu.shares().is_some()),
        ("linux.resources.cpu.idle", cpu.idle().is_some()),
        ("linux.resources.cpu.burst", cpu.burst().is_some()),
        (
            "linux.resources.cpu.realtimeRuntime",
            cpu.realtime_runtime().is_some(),
        ),
        (
            "linux.resources.cpu.realtimePeriod",
            cpu.realtime_period().is_some(),
        ),
        ("linux.resources.cpu.cpus", cpu.cpus().is_some()),
        ("linux.resources.cpu.mems", cpu.mems().is_some()),
    ])?;

    let Some(quota) = cpu.quota() else {
        ensure!(
            cpu.period().is_none(),
            "linux.resources.cpu.period without quota is not supported yet"
        );
        return Ok(None);
    };
    let period = cpu.period().unwrap_or(CpuMax::DEFAULT_PERIOD_US);
    ensure!(
        CPU_PERIOD_RANGE_US.contains(&period),
        "linux.resources.cpu.period must be between {} and {} but got {period}",
        CPU_PERIOD_RANGE_US.start(),
        CPU_PERIOD_RANGE_US.end()
    );
    let quota_us = u64::try_from(quota)
        .ok()
        .filter(|_| quota >= MIN_CPU_QUOTA_US)
        .and_then(NonZeroU64::new)
        .with_context(|| {
            format!("linux.resources.cpu.quota must be at least {MIN_CPU_QUOTA_US} but got {quota}")
        })?;

    Ok(Some(CpuMax {
        quota_us,
        period_us: period,
    }))
}

fn memory_max(memory: &LinuxMemory) -> Result<Option<u64>> {
    reject_present(&[
        (
            "linux.resources.memory.reservation",
            memory.reservation().is_some(),
        ),
        ("linux.resources.memory.swap", memory.swap().is_some()),
        ("linux.resources.memory.kernel", memory.kernel().is_some()),
        (
            "linux.resources.memory.kernelTCP",
            memory.kernel_tcp().is_some(),
        ),
        (
            "linux.resources.memory.swappiness",
            memory.swappiness().is_some(),
        ),
        (
            "linux.resources.memory.disableOOMKiller",
            memory.disable_oom_killer().is_some(),
        ),
        (
            "linux.resources.memory.useHierarchy",
            memory.use_hierarchy().is_some(),
        ),
        (
            "linux.resources.memory.checkBeforeUpdate",
            memory.check_before_update().is_some(),
        ),
    ])?;

    memory
        .limit()
        .map(|limit| {
            u64::try_from(limit)
                .ok()
                .filter(|&limit| limit > 0)
                .with_context(|| {
                    format!("linux.resources.memory.limit must be positive but got {limit}")
                })
        })
        .transpose()
}

fn pids_max(pids: &LinuxPids) -> Result<u64> {
    let limit = pids.limit();
    u64::try_from(limit)
        .ok()
        .filter(|&limit| limit > 0)
        .with_context(|| format!("linux.resources.pids.limit must be positive but got {limit}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;

    // A configuration that the runtime accepts. root.path is relative to the bundle path.
    fn sample() -> Value {
        json!({
            "ociVersion": "1.3.0",
            "root": { "path": "src" },
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": ["/bin/sh", "-c", "echo OCI_STARTED"],
                "env": ["PATH=/bin:/usr/bin"],
                "cwd": "/"
            },
            "mounts": [{
                "destination": "/proc",
                "type": "proc",
                "source": "proc",
                "options": ["nosuid", "nodev", "noexec"]
            }],
            "linux": {
                "namespaces": [
                    { "type": "user" },
                    { "type": "pid" },
                    { "type": "network" },
                    { "type": "mount" }
                ],
                "uidMappings": [{ "containerID": 0, "hostID": 1000, "size": 1 }],
                "gidMappings": [{ "containerID": 0, "hostID": 1001, "size": 1 }],
                "resources": {
                    "cpu": { "quota": 50000, "period": 200000 },
                    "memory": { "limit": 536870912 },
                    "pids": { "limit": 64 }
                }
            }
        })
    }

    // The sample maps container root to host UID 1000 and GID 1001.
    fn convert(config: Value) -> Result<ContainerConfig> {
        convert_as(config, 1000, 1001)
    }

    fn convert_as(config: Value, host_uid: u32, host_gid: u32) -> Result<ContainerConfig> {
        Bundle {
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            spec: serde_json::from_value(config)?,
        }
        .into_config_for(host_uid, host_gid)
    }

    #[test]
    fn converts_supported_configuration() {
        let config = convert(sample()).unwrap();

        assert_eq!(
            config.rootfs,
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
        );
        assert_eq!(config.process.args, ["/bin/sh", "-c", "echo OCI_STARTED"]);
        assert_eq!(config.process.env, ["PATH=/bin:/usr/bin"]);
        assert_eq!(config.process.cwd, Path::new("/"));
        assert_eq!((config.process.uid, config.process.gid), (0, 0));

        let cpu = config.resources.cpu.unwrap();
        assert_eq!((cpu.quota_us.get(), cpu.period_us), (50_000, 200_000));
        assert_eq!(config.resources.memory_max, Some(536_870_912));
        assert_eq!(config.resources.pids_max, Some(64));
    }

    #[test]
    fn uses_absolute_root_path_as_given() {
        let mut config = sample();
        let rootfs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        config["root"]["path"] = json!(rootfs);

        assert_eq!(convert(config).unwrap().rootfs, rootfs);
    }

    #[test]
    fn leaves_unspecified_resources_unset() {
        let mut config = sample();
        config["linux"].as_object_mut().unwrap().remove("resources");
        let resources = convert(config).unwrap().resources;
        assert!(resources.cpu.is_none());
        assert!(resources.memory_max.is_none());
        assert!(resources.pids_max.is_none());

        let mut config = sample();
        config["linux"]["resources"]["cpu"] = json!({ "quota": 50000 });
        let cpu = convert(config).unwrap().resources.cpu.unwrap();
        assert_eq!(cpu.period_us, CpuMax::DEFAULT_PERIOD_US);
    }

    #[test]
    fn uses_empty_environment_when_env_is_missing() {
        let mut config = sample();
        config["process"].as_object_mut().unwrap().remove("env");
        assert!(convert(config).unwrap().process.env.is_empty());
    }

    #[test]
    fn accepts_namespaces_in_any_order() {
        let mut config = sample();
        config["linux"]["namespaces"] = json!([
            { "type": "mount" },
            { "type": "network" },
            { "type": "user" },
            { "type": "pid" }
        ]);
        assert!(convert(config).is_ok());
    }

    #[test]
    fn accepts_example_bundle_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/oci-bundle/config.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        // The example rootfs is not committed, so point root.path at a directory that exists.
        config["root"]["path"] = json!("src");
        // The example maps container root to host UID and GID 1000.
        convert_as(config, 1000, 1000).unwrap();
    }

    #[test]
    fn rejects_unsupported_or_invalid_values() {
        type Change = fn(&mut Value);
        let cases: &[(Change, &str)] = &[
            (|c| c["ociVersion"] = json!("1.2.0"), "ociVersion 1.2.0"),
            (
                |c| {
                    c.as_object_mut().unwrap().remove("ociVersion");
                },
                "ociVersion is required",
            ),
            (|c| c["hooks"] = json!({}), "hooks"),
            (|c| c["hostname"] = json!("box"), "hostname"),
            (|c| c["root"]["path"] = json!(""), "root.path is required"),
            (|c| c["root"]["path"] = json!("missing-rootfs"), "root.path"),
            (|c| c["root"]["readonly"] = json!(true), "root.readonly"),
            (
                |c| {
                    c.as_object_mut().unwrap().remove("process");
                },
                "process is required",
            ),
            (
                |c| c["process"]["terminal"] = json!(true),
                "process.terminal",
            ),
            (
                |c| c["process"]["noNewPrivileges"] = json!(true),
                "process.noNewPrivileges",
            ),
            (
                |c| c["process"]["capabilities"] = json!({}),
                "process.capabilities",
            ),
            (|c| c["process"]["rlimits"] = json!([]), "process.rlimits"),
            (
                |c| c["process"]["user"]["additionalGids"] = json!([10]),
                "process.user.additionalGids",
            ),
            (|c| c["process"]["user"]["uid"] = json!(1), "process.user"),
            (|c| c["process"]["args"] = json!([]), "process.args"),
            (
                |c| c["process"]["args"] = json!(["sh"]),
                "process.args[0] must be an absolute path",
            ),
            (
                |c| c["process"]["args"] = json!(["/bin/sh", "a\u{0}b"]),
                "process.args[1] contains a NUL",
            ),
            (
                |c| c["process"]["env"] = json!(["A=\u{0}"]),
                "process.env[0] contains a NUL",
            ),
            (|c| c["process"]["cwd"] = json!("tmp"), "process.cwd"),
            (|c| c["mounts"] = json!([]), "mounts"),
            (|c| c["mounts"][0]["options"] = json!(["nosuid"]), "mounts"),
            (|c| c["mounts"][0]["destination"] = json!("/sys"), "mounts"),
            (
                |c| c["linux"]["namespaces"][3] = json!({ "type": "uts" }),
                "linux.namespaces",
            ),
            (
                |c| c["linux"]["namespaces"][1]["path"] = json!("/proc/1/ns/pid"),
                "linux.namespaces",
            ),
            (
                |c| c["linux"]["maskedPaths"] = json!(["/proc/kcore"]),
                "linux.maskedPaths",
            ),
            (
                |c| c["linux"]["seccomp"] = json!({ "defaultAction": "SCMP_ACT_ALLOW" }),
                "linux.seccomp",
            ),
            (
                |c| c["linux"]["uidMappings"][0]["size"] = json!(65536),
                "linux.uidMappings",
            ),
            (
                |c| c["linux"]["uidMappings"][0]["hostID"] = json!(0),
                "linux.uidMappings[0].hostID must be 1000",
            ),
            (
                |c| c["linux"]["gidMappings"][0]["hostID"] = json!(1000),
                "linux.gidMappings[0].hostID must be 1001",
            ),
            (
                |c| {
                    c["linux"].as_object_mut().unwrap().remove("gidMappings");
                },
                "linux.gidMappings",
            ),
            (
                |c| c["linux"]["resources"]["devices"] = json!([]),
                "linux.resources.devices",
            ),
            (
                |c| c["linux"]["resources"]["cpu"]["shares"] = json!(1024),
                "linux.resources.cpu.shares",
            ),
            (
                |c| c["linux"]["resources"]["cpu"]["period"] = json!(500),
                "linux.resources.cpu.period",
            ),
            (
                |c| c["linux"]["resources"]["cpu"]["quota"] = json!(-1),
                "linux.resources.cpu.quota",
            ),
            (
                |c| c["linux"]["resources"]["cpu"] = json!({ "period": 100000 }),
                "linux.resources.cpu.period without quota",
            ),
            (
                |c| c["linux"]["resources"]["memory"]["swap"] = json!(0),
                "linux.resources.memory.swap",
            ),
            (
                |c| c["linux"]["resources"]["memory"]["limit"] = json!(-1),
                "linux.resources.memory.limit",
            ),
            (
                |c| c["linux"]["resources"]["pids"]["limit"] = json!(0),
                "linux.resources.pids.limit",
            ),
        ];

        for (change, expected) in cases {
            let mut config = sample();
            change(&mut config);
            let error = match convert(config) {
                Ok(_) => panic!("accepted a configuration that should fail with {expected:?}"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains(expected),
                "expected {expected:?} in {error:?}"
            );
        }
    }
}
