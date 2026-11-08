use crate::config::Volume;
use anyhow::{Context, Result, bail, ensure};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Parse `HOST[:CONTAINER][:ro|rw]`. CONTAINER defaults to the host path, so the agent can use
/// the same paths inside and outside the container.
pub fn parse_volume(spec: &str) -> Result<Volume> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (host, container, mode) = match parts.as_slice() {
        [host] => (*host, None, None),
        [host, mode] if matches!(*mode, "ro" | "rw") => (*host, None, Some(*mode)),
        [host, container] => (*host, Some(*container), None),
        [host, container, mode] => (*host, Some(*container), Some(*mode)),
        _ => bail!("--volume {spec:?} must have the form HOST[:CONTAINER][:ro]"),
    };
    let read_only = match mode {
        None | Some("rw") => false,
        Some("ro") => true,
        Some(other) => bail!("--volume {spec:?} has the unknown option {other:?}; use ro or rw"),
    };

    ensure!(!host.is_empty(), "--volume {spec:?} has an empty host path");
    // Resolving symbolic links here makes the default destination match the path that the
    // runtime's current directory reports.
    let source = fs::canonicalize(host)
        .with_context(|| format!("--volume {spec:?}: cannot find host path {host:?}"))?;

    let destination = match container {
        None => source.clone(),
        Some(container) => {
            let destination = PathBuf::from(container);
            ensure!(
                destination.is_absolute()
                    && !destination.components().any(|c| c == Component::ParentDir),
                "--volume {spec:?}: the container path must be absolute and must not contain '..'"
            );
            destination
        }
    };
    ensure!(
        destination != Path::new("/"),
        "--volume {spec:?}: the container path must not be '/'"
    );

    Ok(Volume {
        source,
        destination,
        read_only,
    })
}

/// The container path of `cwd` if a volume contains it, and `/` otherwise.
///
/// When volumes are nested, the innermost one decides.
pub fn default_workdir(cwd: &Path, volumes: &[Volume]) -> PathBuf {
    volumes
        .iter()
        .filter_map(|volume| {
            let rest = cwd.strip_prefix(&volume.source).ok()?;
            Some((
                volume.source.components().count(),
                volume.destination.join(rest),
            ))
        })
        .max_by_key(|(depth, _)| *depth)
        .map_or_else(|| PathBuf::from("/"), |(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_dir() -> PathBuf {
        fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap()
    }

    fn volume(source: &str, destination: &str) -> Volume {
        Volume {
            source: PathBuf::from(source),
            destination: PathBuf::from(destination),
            read_only: false,
        }
    }

    #[test]
    fn uses_the_host_path_as_the_default_destination() {
        let dir = manifest_dir();
        let parsed = parse_volume(dir.to_str().unwrap()).unwrap();
        assert_eq!(parsed.source, dir);
        assert_eq!(parsed.destination, dir);
        assert!(!parsed.read_only);

        let parsed = parse_volume(&format!("{}:ro", dir.display())).unwrap();
        assert_eq!(parsed.destination, dir);
        assert!(parsed.read_only);
    }

    #[test]
    fn accepts_a_container_path_and_mode() {
        let dir = manifest_dir();
        let parsed = parse_volume(&format!("{}/src:/data:ro", dir.display())).unwrap();
        assert_eq!(parsed.source, dir.join("src"));
        assert_eq!(parsed.destination, PathBuf::from("/data"));
        assert!(parsed.read_only);
    }

    #[test]
    fn rejects_invalid_volumes() {
        let dir = manifest_dir().display().to_string();
        let cases = [
            (format!("{dir}:data"), "must be absolute"),
            (format!("{dir}:/a/../b"), "must not contain '..'"),
            (format!("{dir}:/"), "must not be '/'"),
            (format!("{dir}:/data:rx"), "unknown option"),
            (format!("{dir}:/data:ro:x"), "must have the form"),
            (":/data".to_string(), "empty host path"),
            ("/no/such/path".to_string(), "cannot find host path"),
        ];
        for (spec, expected) in cases {
            let error = parse_volume(&spec).unwrap_err();
            assert!(error.to_string().contains(expected), "{spec}: {error:#}");
        }
    }

    #[test]
    fn picks_the_working_directory_from_the_innermost_volume() {
        let volumes = [
            volume("/home/jace/proj", "/home/jace/proj"),
            volume("/home/jace/proj/data", "/data"),
        ];
        let workdir = |cwd: &str| default_workdir(Path::new(cwd), &volumes);
        assert_eq!(
            workdir("/home/jace/proj/src"),
            Path::new("/home/jace/proj/src")
        );
        assert_eq!(workdir("/home/jace/proj/data/raw"), Path::new("/data/raw"));
        assert_eq!(workdir("/home/jace"), Path::new("/"));
        // A shared prefix is not enough: /home/jace/project is outside /home/jace/proj.
        assert_eq!(workdir("/home/jace/project"), Path::new("/"));
    }
}
