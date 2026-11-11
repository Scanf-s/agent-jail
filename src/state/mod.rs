mod dir;
mod query;
mod record;
mod stale;

pub use dir::StateDir;
pub use query::{list, read, table};
pub use record::Record;
pub use stale::remove_stale;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

const STATE_FILE: &str = "state.json";

/// The address of the socket `name` in the state directory that `dir` has open.
///
/// A socket address holds at most 107 bytes, and the path of a state directory can be longer.
/// So the address names the directory through the open descriptor.
pub fn socket_address(dir: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{name}", dir.as_raw_fd()))
}
