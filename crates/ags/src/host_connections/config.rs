use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

pub const CONTAINER_DIR: &str = "/run/ags-connections";

/// Only the trusted base config may declare destinations or grant hook activation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub name: String,
    pub host_unix: Option<PathBuf>,
    pub host_tcp: Option<SocketAddr>,
    pub sandbox_unix: Option<String>,
    pub sandbox_tcp: Option<SocketAddr>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_hooks: bool,
}

fn enabled() -> bool {
    true
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub fn validate(connections: &[Connection]) -> Result<(), String> {
    if connections.len() > 32 {
        return Err("at most 32 host connections are allowed".into());
    }
    let mut names = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for c in connections {
        if !valid_name(&c.name) || !names.insert(&c.name) {
            return Err(
                "host connection names must be unique, 1..64 ASCII letters/digits/_/-".into(),
            );
        }
        if c.host_unix.is_some() == c.host_tcp.is_some()
            || c.sandbox_unix.is_some() == c.sandbox_tcp.is_some()
        {
            return Err(format!(
                "host connection {:?}: specify exactly one host and one sandbox endpoint",
                c.name
            ));
        }
        if let Some(path) = &c.host_unix
            && (!path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
                || path.as_os_str().as_encoded_bytes().contains(&0)
                || path.as_os_str().as_encoded_bytes().len() >= 108)
        {
            return Err(format!(
                "host connection {:?}: host_unix must be an absolute Unix socket path shorter than 108 bytes",
                c.name
            ));
        }
        for address in [c.host_tcp, c.sandbox_tcp].into_iter().flatten() {
            // Numeric loopback addresses only: no DNS rebinding or wildcard exposure.
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err(format!(
                    "host connection {:?}: TCP endpoints require numeric loopback addresses and nonzero ports",
                    c.name
                ));
            }
        }
        let destination = if let Some(path) = &c.sandbox_unix {
            let filename = Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let name = filename.strip_suffix(".sock").unwrap_or("");
            if !valid_name(name) || path != &format!("{CONTAINER_DIR}/{name}.sock") {
                return Err(format!(
                    "host connection {:?}: sandbox_unix must be {CONTAINER_DIR}/NAME.sock",
                    c.name
                ));
            }
            path.clone()
        } else {
            c.sandbox_tcp.unwrap().to_string()
        };
        if !destinations.insert(destination) {
            return Err("host connections must have distinct sandbox endpoints".into());
        }
    }
    Ok(())
}

/// Hooks can request only explicitly granted names, never invent destinations.
pub fn activate(connections: &mut [Connection], requests: &BTreeSet<String>) -> Result<(), String> {
    for name in requests {
        let connection = connections
            .iter_mut()
            .find(|c| &c.name == name)
            .ok_or_else(|| format!("unknown host connection requested by hook: {name:?}"))?;
        if !connection.allow_hooks {
            return Err(format!(
                "host connection {name:?} does not allow hook activation"
            ));
        }
        connection.enabled = true;
    }
    Ok(())
}
