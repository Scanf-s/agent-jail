mod mapping;
mod namespace;
mod subordinate;

pub use mapping::{IdRange, Mapping};
pub use namespace::run_in_user_namespace;
#[cfg(test)]
pub use subordinate::SubordinateRange;
