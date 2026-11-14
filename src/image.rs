use crate::paths;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const PREFIX: &str = "sha256-";

/// The ID of an image in the image store: `sha256-` and the 64 hexadecimal digits of the
/// SHA-256 of the image's archive. An image never changes after it is stored, so a sandbox that
/// records the ID always gets the same base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageId(String);

impl ImageId {
    pub fn parse(value: &str) -> Result<Self> {
        let valid = value.strip_prefix(PREFIX).is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        ensure!(
            valid,
            "invalid image ID {value:?}: expected {PREFIX} and 64 lowercase hexadecimal digits"
        );
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 digits of the hash, as `docker images` shows an ID.
    pub fn short(&self) -> &str {
        &self.0[PREFIX.len()..PREFIX.len() + 12]
    }

    /// The rootfs directory of the image.
    pub fn rootfs(&self) -> Result<PathBuf> {
        Ok(images_dir()?.join(&self.0))
    }
}

impl TryFrom<String> for ImageId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<ImageId> for String {
    fn from(id: ImageId) -> Self {
        id.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `$XDG_DATA_HOME/agent-jail/images`.
pub fn images_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("images"))
}

/// The ID of the image that `name` names: an image ID, or a link in the image store such as
/// `default`.
pub fn resolve(name: &str) -> Result<ImageId> {
    resolve_in(&images_dir()?, name)
}

fn resolve_in(images: &Path, name: &str) -> Result<ImageId> {
    ensure!(
        !name.is_empty() && !name.contains('/') && !name.starts_with('.'),
        "invalid image name {name:?}"
    );
    let path = images.join(name);
    let id = match fs::read_link(&path) {
        // The target is the name of an image directory next to the link.
        Ok(target) => {
            let target = target.to_str().with_context(|| {
                format!("the link {path:?} points to {target:?}, which is no image ID")
            })?;
            ImageId::parse(target).with_context(|| format!("read the link {path:?}"))?
        }
        // A path that is no link must be the image directory itself.
        Err(error) if error.raw_os_error() == Some(nix::libc::EINVAL) => ImageId::parse(name)
            .with_context(|| format!("{path:?} is neither an image nor a link to one"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "there is no image {name:?} in {images:?}; fetch one with ./scripts/fetch-rootfs.sh"
        ),
        Err(error) => return Err(error).with_context(|| format!("read {path:?}")),
    };
    let rootfs = images.join(id.as_str());
    ensure!(
        rootfs.is_dir(),
        "image {name:?} names {id}, but {rootfs:?} is not a directory"
    );
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;
    use std::os::unix::fs::symlink;

    const HASH: &str = "c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("agent-jail-image-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accepts_only_sha256_ids() {
        let id = ImageId::parse(&format!("sha256-{HASH}")).unwrap();
        assert_eq!(id.short(), "c5ca053cfe1d");
        for invalid in [
            HASH.to_owned(),
            format!("sha256-{}", &HASH[1..]),
            format!("sha256-{}", HASH.to_uppercase()),
            format!("sha512-{HASH}"),
        ] {
            assert!(ImageId::parse(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn resolves_a_link_and_an_id() {
        let images = TempDir::new();
        let id = format!("sha256-{HASH}");
        fs::create_dir(images.0.join(&id)).unwrap();
        symlink(&id, images.0.join("default")).unwrap();

        assert_eq!(resolve_in(&images.0, "default").unwrap().as_str(), id);
        assert_eq!(resolve_in(&images.0, &id).unwrap().as_str(), id);
    }

    #[test]
    fn rejects_what_is_no_image() {
        let images = TempDir::new();
        // A plain directory from an older fetch script has no ID.
        fs::create_dir(images.0.join("default")).unwrap();
        symlink("elsewhere", images.0.join("broken")).unwrap();
        symlink(format!("sha256-{HASH}"), images.0.join("dangling")).unwrap();

        for name in [
            "default", "broken", "dangling", "missing", "../x", ".fetch-1", "",
        ] {
            assert!(resolve_in(&images.0, name).is_err(), "resolved {name:?}");
        }
    }
}
