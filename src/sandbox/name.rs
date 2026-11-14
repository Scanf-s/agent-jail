use crate::container_id::ContainerId;
use anyhow::{Result, anyhow};
use std::fmt;

/// The name of a sandbox. It follows the rule for container IDs, because the container of the
/// sandbox uses the name as its ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxName(ContainerId);

impl SandboxName {
    pub fn parse(value: &str) -> Result<Self> {
        ContainerId::parse(value).map(Self).map_err(|_| {
            anyhow!(
                "invalid sandbox name {value:?}: use 1 to {} ASCII letters, digits, '_', '.', \
                 and '-', and start with a letter or digit",
                ContainerId::MAX_LENGTH
            )
        })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SandboxName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_rule_for_container_ids() {
        assert_eq!(
            SandboxName::parse("my.proj-1").unwrap().as_str(),
            "my.proj-1"
        );
        let error = SandboxName::parse(".hidden").unwrap_err();
        assert!(
            error.to_string().contains("invalid sandbox name"),
            "{error}"
        );
        assert!(SandboxName::parse("a+b").is_err());
    }
}
