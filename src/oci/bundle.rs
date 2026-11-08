use anyhow::{Context, Result};
use oci_spec::runtime::Spec;
use std::path::{Path, PathBuf};

// A bundle directory and the configuration read from its config.json.
pub struct Bundle {
    // Absolute path, because a relative root.path is resolved from the bundle directory.
    pub path: PathBuf,
    pub spec: Spec,
}

impl Bundle {
    // Parse config.json
    pub fn load(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)
            .with_context(|| format!("resolve bundle path {}", path.display()))?;
        let config = path.join("config.json");
        let spec = Spec::load(&config).with_context(|| format!("load {}", config.display()))?;
        Ok(Self { path, spec })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::random;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("container-runtime-bundle-{:x}", random::<u64>()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn with_config(config: &str) -> Self {
            let temp = Self::new();
            fs::write(temp.0.join("config.json"), config).unwrap();
            temp
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_config_and_ignores_unknown_fields() {
        let temp = TempDir::with_config(
            r#"{
                "ociVersion": "1.3.0",
                "root": { "path": "rootfs" },
                "process": {
                    "user": { "uid": 0, "gid": 0 },
                    "args": ["/bin/true"],
                    "cwd": "/",
                    "unknownProcessField": true
                },
                "unknownTopLevelField": { "nested": 1 }
            }"#,
        );

        let bundle = Bundle::load(&temp.0).unwrap();
        assert_eq!(bundle.path, temp.0);
        assert_eq!(bundle.spec.version(), "1.3.0");
        assert_eq!(
            bundle.spec.root().as_ref().unwrap().path(),
            Path::new("rootfs")
        );
        let process = bundle.spec.process().as_ref().unwrap();
        assert_eq!(process.args().as_ref().unwrap(), &["/bin/true"]);
        assert_eq!(process.cwd(), Path::new("/"));
    }

    #[test]
    fn rejects_process_without_cwd() {
        let temp = TempDir::with_config(
            r#"{
                "ociVersion": "1.3.0",
                "root": { "path": "rootfs" },
                "process": { "user": { "uid": 0, "gid": 0 }, "args": ["/bin/true"] }
            }"#,
        );

        let error = Bundle::load(&temp.0).err().unwrap();
        assert!(format!("{error:#}").contains("cwd"), "{error:#}");
    }

    #[test]
    fn reports_config_path_when_loading_fails() {
        let missing = TempDir::new();
        let error = Bundle::load(&missing.0).err().unwrap();
        assert!(error.to_string().contains("config.json"), "{error:#}");

        let invalid = TempDir::with_config("{ not json");
        let error = Bundle::load(&invalid.0).err().unwrap();
        assert!(error.to_string().contains("config.json"), "{error:#}");
    }
}
