use anyhow::{Result, bail};
use rand::random;
use std::fmt;

/// The name that every resource of one container uses: its systemd scope, its cgroup, and its
/// run directory. With one name, a later command can find what a killed run left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerId(String);

impl ContainerId {
    /// The ID is part of a systemd unit name, which may have at most 255 characters.
    const MAX_LENGTH: usize = 128;

    /// Generate an ID of 16 lowercase hexadecimal digits.
    pub fn generate() -> Self {
        Self(format!("{:016x}", random::<u64>()))
    }

    /// Accept the container names that Docker and Podman accept, up to 128 characters: an ASCII
    /// letter or digit, then ASCII letters, digits, `_`, `.`, and `-`.
    ///
    /// The ID becomes a path component and a systemd unit name. The rule for the first character
    /// keeps out `.`, `..`, and IDs that a command line reads as options. runc also accepts `+`,
    /// but systemd unit names cannot contain it.
    pub fn parse(value: &str) -> Result<Self> {
        let mut bytes = value.bytes();
        let valid = value.len() <= Self::MAX_LENGTH
            && bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
        if !valid {
            bail!(
                "invalid container ID {value:?}: use 1 to {} ASCII letters, digits, '_', '.', \
                 and '-', and start with a letter or digit",
                Self::MAX_LENGTH
            );
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_16_hexadecimal_digits() {
        let id = ContainerId::generate();
        assert_eq!(id.as_str().len(), 16);
        assert!(id.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(ContainerId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn accepts_docker_container_names() {
        let longest = "a".repeat(128);
        for valid in [
            "a",
            "box1",
            "Agent.session-2",
            "web.service",
            "my_box",
            "5f0c3d1e-8a2b-4c6d-9e7f-0a1b2c3d4e5f",
            longest.as_str(),
        ] {
            assert_eq!(ContainerId::parse(valid).unwrap().as_str(), valid);
        }
    }

    #[test]
    fn rejects_other_names() {
        let too_long = "a".repeat(129);
        for invalid in [
            "",
            ".",
            "..",
            "-rm",
            "_box",
            ".hidden",
            "my+box",
            "data/box",
            "two words",
            "상자1",
            too_long.as_str(),
        ] {
            let error = ContainerId::parse(invalid).unwrap_err();
            assert!(
                error.to_string().contains("invalid container ID"),
                "{invalid:?}: {error:#}"
            );
        }
    }
}
