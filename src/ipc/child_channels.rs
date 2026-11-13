use super::{send_ready, wait_ready};
use anyhow::{Context, Result};
use std::os::fd::OwnedFd;

pub(crate) struct ChildChannels {
    cgroup_reader: Option<OwnedFd>,
    user_namespace_writer: Option<OwnedFd>,
    mapping_reader: Option<OwnedFd>,
    network_namespace_writer: Option<OwnedFd>,
    network_reader: Option<OwnedFd>,
    created_writer: Option<OwnedFd>,
}

impl ChildChannels {
    pub(super) fn new(
        cgroup_reader: OwnedFd,
        user_namespace_writer: OwnedFd,
        mapping_reader: OwnedFd,
        network_namespace_writer: OwnedFd,
        network_reader: OwnedFd,
        created_writer: OwnedFd,
    ) -> Self {
        Self {
            cgroup_reader: Some(cgroup_reader),
            user_namespace_writer: Some(user_namespace_writer),
            mapping_reader: Some(mapping_reader),
            network_namespace_writer: Some(network_namespace_writer),
            network_reader: Some(network_reader),
            created_writer: Some(created_writer),
        }
    }

    pub(crate) fn wait_for_cgroup(&mut self) -> Result<()> {
        let fd = self
            .cgroup_reader
            .take()
            .context("cgroup already checked")?;
        wait_ready(&fd, "cgroup")
    }

    pub(crate) fn signal_user_namespace_ready(&mut self) -> Result<()> {
        let fd = self
            .user_namespace_writer
            .take()
            .context("user namespace already signaled")?;
        send_ready(&fd, "user namespace")
    }

    pub(crate) fn wait_for_mapping(&mut self) -> Result<()> {
        let fd = self
            .mapping_reader
            .take()
            .context("mapping already checked")?;
        wait_ready(&fd, "mapping")
    }

    pub(crate) fn signal_network_namespace_ready(&mut self) -> Result<()> {
        let fd = self
            .network_namespace_writer
            .take()
            .context("network namespace already signaled")?;
        send_ready(&fd, "network namespace")
    }

    pub(crate) fn wait_for_network(&mut self) -> Result<()> {
        let fd = self
            .network_reader
            .take()
            .context("network already checked")?;
        wait_ready(&fd, "network")
    }

    pub(crate) fn signal_created(&mut self) -> Result<()> {
        let fd = self
            .created_writer
            .take()
            .context("created already signaled")?;
        send_ready(&fd, "created")
    }
}
