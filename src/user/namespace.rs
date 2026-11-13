use super::Mapping;
use crate::ipc::{send_ready, wait_ready};
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, fork, pipe};

/// Run `task` in a child process that is root in a new user namespace with the maps of
/// `mapping`, and wait until it ends.
///
/// Files that container users other than root create belong to subordinate IDs. Only a
/// process in a user namespace that maps these IDs can change or remove such files.
pub fn run_in_user_namespace(mapping: &Mapping, task: impl FnOnce() -> Result<()>) -> Result<()> {
    let (namespace_reader, namespace_writer) = pipe().context("create user namespace pipe")?;
    let (mapping_reader, mapping_writer) = pipe().context("create mapping pipe")?;

    // SAFETY: the runtime has a single thread, so the child can run any code after fork.
    match unsafe { fork() }.context("fork user namespace helper")? {
        ForkResult::Child => {
            drop((namespace_reader, mapping_writer));
            let result = (|| -> Result<()> {
                unshare(CloneFlags::CLONE_NEWUSER).context("unshare(CLONE_NEWUSER)")?;
                send_ready(&namespace_writer, "user namespace")?;
                wait_ready(&mapping_reader, "mapping")?;
                task()
            })();
            if let Err(error) = result {
                report!("aj: {error:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        ForkResult::Parent { child } => {
            drop((namespace_writer, mapping_reader));
            let setup = (|| -> Result<()> {
                wait_ready(&namespace_reader, "user namespace")?;
                mapping.apply(child).context("map helper UIDs and GIDs")?;
                send_ready(&mapping_writer, "mapping")
            })();
            if setup.is_err() {
                let _ = kill(child, Signal::SIGKILL);
            }
            let status = waitpid(child, None).context("wait for user namespace helper")?;
            setup?;
            match status {
                WaitStatus::Exited(_, 0) => Ok(()),
                other => bail!("user namespace helper failed: {other:?}"),
            }
        }
    }
}
