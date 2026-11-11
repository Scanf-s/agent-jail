use rand::random;
use std::fmt;

/// The name that every resource of one container uses: its systemd scope, its cgroup, and its
/// run directory. With one name, a later command can find what a killed run left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerId(String);

impl ContainerId {
    const LENGTH: usize = 16;

    pub fn generate() -> Self {
        Self(format!("{:016x}", random::<u64>()))
    }

    /// Accept only the form that `generate` creates: 16 lowercase hexadecimal digits.
    ///
    /// The ID becomes a path component and a systemd unit name, so other characters are never
    /// taken from outside input.
    pub fn parse(value: &str) -> Option<Self> {
        let valid = value.len() == Self::LENGTH
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        valid.then(|| Self(value.to_owned()))
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
    fn parses_the_generated_form_only() {
        let id = ContainerId::generate();
        assert_eq!(id.as_str().len(), 16);
        assert_eq!(ContainerId::parse(id.as_str()), Some(id));

        for invalid in [
            "",
            "..",
            "1a2b",
            "0123456789ABCDEF",
            "0123456789abcdef0",
            "0123456789abcde/",
        ] {
            assert_eq!(ContainerId::parse(invalid), None, "{invalid:?}");
        }
    }
}
