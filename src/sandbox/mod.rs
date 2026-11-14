//! Sandboxes: named environments whose installed files stay across containers.
//!
//! A sandbox keeps its record and the overlay upper and work directories under
//! `$XDG_DATA_HOME/agent-jail/sandboxes/<name>`.

mod name;
mod record;
mod store;

pub use name::SandboxName;
pub use record::SandboxRecord;
pub use store::{create_in, list_in, sandboxes_dir, table};
