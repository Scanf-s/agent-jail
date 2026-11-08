mod mapping;
mod subordinate;

pub use mapping::{IdRange, Mapping};
#[cfg(test)]
pub use subordinate::SubordinateRange;
