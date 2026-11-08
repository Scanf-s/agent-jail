use anyhow::{Context, Result};
use nix::unistd::{Uid, User};
use std::fs;

/// Number of IDs that the container uses above root, so container IDs 1 to 65536 exist.
pub const CONTAINER_RANGE_SIZE: u32 = 65536;

/// Host IDs that `/etc/subuid` or `/etc/subgid` assigns to a user, for use inside the container.
#[derive(Debug, PartialEq, Eq)]
pub struct SubordinateRange {
    pub start: u32,
    pub count: u32,
}

/// Read the user's subordinate UID range from `/etc/subuid`.
pub fn subordinate_uids(uid: Uid) -> Result<SubordinateRange> {
    read_range("/etc/subuid", "--add-subuids", uid)
}

/// Read the user's subordinate GID range from `/etc/subgid`.
///
/// Entries in this file also name the user, not a group.
pub fn subordinate_gids(uid: Uid) -> Result<SubordinateRange> {
    read_range("/etc/subgid", "--add-subgids", uid)
}

fn read_range(path: &str, usermod_option: &str, uid: Uid) -> Result<SubordinateRange> {
    let name = User::from_uid(uid)
        .with_context(|| format!("look up the name of UID {uid}"))?
        .map(|user| user.name);
    let contents = fs::read_to_string(path).with_context(|| format!("read {path}"))?;

    let shown_name = name.clone().unwrap_or_else(|| uid.to_string());
    find_range(&contents, name.as_deref(), uid.as_raw()).with_context(|| {
        format!(
            "{path} has no range of at least {CONTAINER_RANGE_SIZE} IDs for {shown_name}; \
             add one with `sudo usermod {usermod_option} 100000-165535 {shown_name}`"
        )
    })
}

/// Return the first range of at least `CONTAINER_RANGE_SIZE` IDs for the user.
///
/// An entry names the user by login name or by numeric ID. Lines that do not have the
/// `owner:start:count` form are skipped, as the shadow tools do.
fn find_range(contents: &str, name: Option<&str>, uid: u32) -> Option<SubordinateRange> {
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        let (owner, start, count) = (fields.next()?, fields.next()?, fields.next()?);
        if fields.next().is_some() {
            return None;
        }
        let is_user = Some(owner) == name || owner.parse() == Ok(uid);
        let range = SubordinateRange {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        };
        (is_user && range.count >= CONTAINER_RANGE_SIZE).then_some(range)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u32, count: u32) -> Option<SubordinateRange> {
        Some(SubordinateRange { start, count })
    }

    #[test]
    fn finds_the_range_by_name_or_numeric_id() {
        let contents = "alice:100000:65536\njace:165536:65536\n";
        assert_eq!(
            find_range(contents, Some("jace"), 1000),
            range(165536, 65536)
        );
        assert_eq!(
            find_range("1000:200000:65536\n", Some("jace"), 1000),
            range(200000, 65536)
        );
        assert_eq!(
            find_range("1000:200000:65536\n", None, 1000),
            range(200000, 65536)
        );
    }

    #[test]
    fn skips_ranges_that_are_too_small_or_malformed() {
        let contents = "\
# comment
jace:100000:1000
jace:x:65536
jace:150000:65536:extra
jace:300000:70000
";
        assert_eq!(
            find_range(contents, Some("jace"), 1000),
            range(300000, 70000)
        );
    }

    #[test]
    fn returns_nothing_when_the_user_has_no_range() {
        assert_eq!(find_range("alice:100000:65536\n", Some("jace"), 1000), None);
        assert_eq!(find_range("", Some("jace"), 1000), None);
    }
}
