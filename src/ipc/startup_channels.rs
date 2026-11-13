use super::{ChildChannels, ParentChannels};
use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use nix::unistd::pipe2;
use std::os::fd::OwnedFd;

pub(crate) struct StartupChannels {
    cgroup: (OwnedFd, OwnedFd),
    user_namespace: (OwnedFd, OwnedFd),
    mapping: (OwnedFd, OwnedFd),
    network_namespace: (OwnedFd, OwnedFd),
    network: (OwnedFd, OwnedFd),
    created: (OwnedFd, OwnedFd),
}

impl StartupChannels {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            cgroup: pipe("cgroup")?,
            user_namespace: pipe("user namespace")?,
            mapping: pipe("mapping")?,
            network_namespace: pipe("network namespace")?,
            network: pipe("network")?,
            created: pipe("created")?,
        })
    }

    /// Call in the parent after fork
    pub(crate) fn for_parent(self) -> ParentChannels {
        let (cgroup_reader, cgroup_writer) = self.cgroup;
        let (user_namespace_reader, user_namespace_writer) = self.user_namespace;
        let (mapping_reader, mapping_writer) = self.mapping;
        let (network_namespace_reader, network_namespace_writer) = self.network_namespace;
        let (network_reader, network_writer) = self.network;
        let (created_reader, created_writer) = self.created;

        // close the ends used only by the child.
        drop((
            cgroup_reader,
            user_namespace_writer,
            mapping_reader,
            network_namespace_writer,
            network_reader,
            created_writer,
        ));

        ParentChannels::new(
            cgroup_writer,
            user_namespace_reader,
            mapping_writer,
            network_namespace_reader,
            network_writer,
            created_reader,
        )
    }

    /// Call in the child after fork
    pub(crate) fn for_child(self) -> ChildChannels {
        let (cgroup_reader, cgroup_writer) = self.cgroup;
        let (user_namespace_reader, user_namespace_writer) = self.user_namespace;
        let (mapping_reader, mapping_writer) = self.mapping;
        let (network_namespace_reader, network_namespace_writer) = self.network_namespace;
        let (network_reader, network_writer) = self.network;
        let (created_reader, created_writer) = self.created;

        // close the ends used only by the parent.
        drop((
            cgroup_writer,
            user_namespace_reader,
            mapping_writer,
            network_namespace_reader,
            network_writer,
            created_reader,
        ));

        ChildChannels::new(
            cgroup_reader,
            user_namespace_writer,
            mapping_reader,
            network_namespace_writer,
            network_reader,
            created_writer,
        )
    }
}

fn pipe(stage: &str) -> Result<(OwnedFd, OwnedFd)> {
    pipe2(OFlag::O_CLOEXEC).with_context(|| format!("create {stage} pipe"))
}
