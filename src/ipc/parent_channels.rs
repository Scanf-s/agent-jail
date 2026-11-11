use super::{send_ready, wait_ready};
use anyhow::{Context, Result};
use std::os::fd::OwnedFd;

pub(crate) struct ParentChannels {
    cgroup_writer: Option<OwnedFd>,
    user_namespace_reader: Option<OwnedFd>,
    mapping_writer: Option<OwnedFd>,
    network_namespace_reader: Option<OwnedFd>,
    network_writer: Option<OwnedFd>,
    created_reader: Option<OwnedFd>,
}

impl ParentChannels {
    pub(super) fn new(
        cgroup_writer: OwnedFd,
        user_namespace_reader: OwnedFd,
        mapping_writer: OwnedFd,
        network_namespace_reader: OwnedFd,
        network_writer: OwnedFd,
        created_reader: OwnedFd,
    ) -> Self {
        Self {
            cgroup_writer: Some(cgroup_writer),
            user_namespace_reader: Some(user_namespace_reader),
            mapping_writer: Some(mapping_writer),
            network_namespace_reader: Some(network_namespace_reader),
            network_writer: Some(network_writer),
            created_reader: Some(created_reader),
        }
    }

    // signal to the child that cgroup task is done
    pub(crate) fn signal_cgroup_ready(&mut self) -> Result<()> {
        let fd = self
            .cgroup_writer
            .take()
            .context("cgroup already signaled")?;
        send_ready(&fd, "cgroup")
    }

    // wait until the child finishes its user/network namespace isolation
    pub(crate) fn wait_for_user_namespace(&mut self) -> Result<()> {
        let fd = self
            .user_namespace_reader
            .take()
            .context("user namespace already checked")?;
        wait_ready(&fd, "user namespace")
    }

    // signal to the child that uid/gid mapping task is done
    pub(crate) fn signal_mapping_ready(&mut self) -> Result<()> {
        let fd = self
            .mapping_writer
            .take()
            .context("mapping already signaled")?;
        send_ready(&fd, "mapping")
    }

    // wait until the child creates its network namespace
    pub(crate) fn wait_for_network_namespace(&mut self) -> Result<()> {
        let fd = self
            .network_namespace_reader
            .take()
            .context("network namespace already checked")?;
        wait_ready(&fd, "network namespace")
    }

    // signal to the child that the network of its namespace is configured
    pub(crate) fn signal_network_ready(&mut self) -> Result<()> {
        let fd = self
            .network_writer
            .take()
            .context("network already signaled")?;
        send_ready(&fd, "network")
    }

    // the pipe on which the child signals that the container is created. The parent waits for
    // it together with signals, so it reads the pipe itself.
    pub(crate) fn take_created_reader(&mut self) -> Result<OwnedFd> {
        self.created_reader
            .take()
            .context("created pipe already taken")
    }
}
