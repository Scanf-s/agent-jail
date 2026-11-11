mod dir;
mod record;
mod stale;

pub use dir::StateDir;
pub use record::Record;
pub use stale::remove_stale;

const STATE_FILE: &str = "state.json";
