use super::{ChildChannels, ParentChannels};
use anyhow::{Context, Result};
use nix::unistd::pipe;
use std::os::fd::OwnedFd;

pub(crate) struct StartupChannels {
    cgroup: (OwnedFd, OwnedFd),
    user_namespace: (OwnedFd, OwnedFd),
    mapping: (OwnedFd, OwnedFd),
    network_namespace: (OwnedFd, OwnedFd),
    network: (OwnedFd, OwnedFd),
}

impl StartupChannels {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            cgroup: pipe().context("create cgroup pipe")?,
            user_namespace: pipe().context("create user namespace pipe")?,
            mapping: pipe().context("create mapping pipe")?,
            network_namespace: pipe().context("create network namespace pipe")?,
            network: pipe().context("create network pipe")?,
        })
    }

    /// Call in the parent after fork
    pub(crate) fn for_parent(self) -> ParentChannels {
        let (cgroup_reader, cgroup_writer) = self.cgroup;
        let (user_namespace_reader, user_namespace_writer) = self.user_namespace;
        let (mapping_reader, mapping_writer) = self.mapping;
        let (network_namespace_reader, network_namespace_writer) = self.network_namespace;
        let (network_reader, network_writer) = self.network;

        // close the ends used only by the child.
        drop((
            cgroup_reader,
            user_namespace_writer,
            mapping_reader,
            network_namespace_writer,
            network_reader,
        ));

        ParentChannels::new(
            cgroup_writer,
            user_namespace_reader,
            mapping_writer,
            network_namespace_reader,
            network_writer,
        )
    }

    /// Call in the child after fork
    pub(crate) fn for_child(self) -> ChildChannels {
        let (cgroup_reader, cgroup_writer) = self.cgroup;
        let (user_namespace_reader, user_namespace_writer) = self.user_namespace;
        let (mapping_reader, mapping_writer) = self.mapping;
        let (network_namespace_reader, network_namespace_writer) = self.network_namespace;
        let (network_reader, network_writer) = self.network;

        // close the ends used only by the parent.
        drop((
            cgroup_writer,
            user_namespace_reader,
            mapping_writer,
            network_namespace_reader,
            network_writer,
        ));

        ChildChannels::new(
            cgroup_reader,
            user_namespace_writer,
            mapping_reader,
            network_namespace_writer,
            network_reader,
        )
    }
}
