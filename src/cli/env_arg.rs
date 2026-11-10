use anyhow::{Result, ensure};
use std::collections::BTreeMap;

/// The PATH of Alpine's /etc/profile, which is also Docker's default.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Host variables that the container receives without --env. The host environment often holds
/// secrets such as API keys, so only variables that programs need to work are copied.
/// Package managers cannot reach the network behind a proxy without the proxy variables.
const COPIED_FROM_HOST: &[&str] = &[
    "TERM",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
];

/// Build the container environment as `KEY=VALUE` entries.
///
/// Each spec is `KEY=VALUE`, or `KEY` to copy the host's value. A `KEY` that the host does not
/// set is skipped, as Docker does. Specs override the defaults, and later specs override
/// earlier ones.
pub fn build_env(specs: &[String], host: impl Fn(&str) -> Option<String>) -> Result<Vec<String>> {
    let mut env = BTreeMap::from([
        (String::from("PATH"), String::from(DEFAULT_PATH)),
        // The program runs as container root, whose home in the rootfs is /root.
        (String::from("HOME"), String::from("/root")),
    ]);
    for &key in COPIED_FROM_HOST {
        if let Some(value) = host(key) {
            env.insert(String::from(key), value);
        }
    }

    for spec in specs {
        let (key, value) = match spec.split_once('=') {
            Some((key, value)) => (key, Some(String::from(value))),
            None => (spec.as_str(), host(spec)),
        };
        ensure!(
            !key.is_empty(),
            "--env {spec:?} must have the form KEY=VALUE or KEY"
        );
        if let Some(value) = value {
            env.insert(String::from(key), value);
        }
    }

    Ok(env
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str) -> Option<String> {
        match key {
            "TERM" => Some(String::from("xterm-256color")),
            "https_proxy" => Some(String::from("http://proxy:8080")),
            "API_KEY" => Some(String::from("secret")),
            "PATH" => Some(String::from("/home/user/.cargo/bin:/usr/bin")),
            _ => None,
        }
    }

    fn specs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| String::from(*value)).collect()
    }

    #[test]
    fn copies_only_listed_host_variables() {
        let env = build_env(&[], host).unwrap();
        assert_eq!(
            env,
            [
                "HOME=/root",
                &format!("PATH={DEFAULT_PATH}"),
                "TERM=xterm-256color",
                "https_proxy=http://proxy:8080",
            ]
        );
    }

    #[test]
    fn applies_env_options_over_defaults() {
        let env = build_env(
            &specs(&["API_KEY", "LANG=C.UTF-8", "HOME=/work", "MISSING", "EMPTY="]),
            host,
        )
        .unwrap();
        assert!(env.contains(&String::from("API_KEY=secret")));
        assert!(env.contains(&String::from("LANG=C.UTF-8")));
        assert!(env.contains(&String::from("HOME=/work")));
        assert!(env.contains(&String::from("EMPTY=")));
        assert!(!env.iter().any(|entry| entry.starts_with("MISSING")));
    }

    #[test]
    fn later_options_win_and_empty_keys_fail() {
        let env = build_env(&specs(&["A=1", "A=2"]), host).unwrap();
        assert!(env.contains(&String::from("A=2")));
        assert!(!env.contains(&String::from("A=1")));
        assert!(build_env(&specs(&["=value"]), host).is_err());
        assert!(build_env(&specs(&[""]), host).is_err());
    }
}
