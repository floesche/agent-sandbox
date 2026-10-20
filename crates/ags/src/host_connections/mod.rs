//! Session-scoped host stream connections; independent of SSH, GitHub, or MCP.
mod config;
mod stream;

pub use config::{CONTAINER_DIR, Connection, activate, valid_name, validate};

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::config::MountMode;
use crate::plan::{LaunchPlan, PlanMount};

const CLIENT: &str = include_str!("../../../../agent/host-connection-client.py");
const MAX_CLIENTS: usize = 32;

/// Owns all accept loops, workers and private files. Drop closes active streams.
pub struct Session {
    directory: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    tcp: bool,
}

impl Session {
    pub fn start(connections: &[Connection], runtime: &Path) -> io::Result<Self> {
        validate(connections).map_err(io::Error::other)?;
        let directory = tempfile::Builder::new()
            .prefix("connections-")
            .tempdir_in(runtime)?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let mut session = Self {
            directory,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            tcp: false,
        };
        let mut tcp = Vec::new();
        // Probe every selected target first. A probe connects and immediately closes;
        // it is availability proof, not protocol/authentication proof.
        for c in connections.iter().filter(|c| c.enabled) {
            check(c).map_err(io::Error::other)?;
        }
        for (index, c) in connections.iter().filter(|c| c.enabled).enumerate() {
            let filename = c
                .sandbox_unix
                .as_ref()
                .map(|p| {
                    Path::new(p)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned()
                })
                .unwrap_or_else(|| format!("tcp-{index}"));
            let socket = session.directory.path().join(&filename);
            let listener = UnixListener::bind(&socket)?;
            fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            if let Some(address) = c.sandbox_tcp {
                tcp.push(serde_json::json!({"host":address.ip().to_string(),"port":address.port(),"socket":format!("{CONTAINER_DIR}/{filename}")}));
            }
            let stop = Arc::clone(&session.stop);
            let connection = c.clone();
            session.threads.push(
                thread::Builder::new()
                    .name(format!("ags-{}", c.name))
                    .spawn(move || {
                        serve(listener, connection, stop);
                    })?,
            );
        }
        session.tcp = !tcp.is_empty();
        if session.tcp {
            fs::write(session.directory.path().join("client.py"), CLIENT)?;
            fs::write(
                session.directory.path().join("tcp.json"),
                serde_json::to_vec(&tcp)?,
            )?;
        }
        Ok(session)
    }

    pub fn attach(&self, plan: &mut LaunchPlan) -> Result<(), String> {
        // Do not allow any config/CLI/hook mount to mask or alias the managed directory.
        for m in &plan.mounts {
            let path = normalized_destination(&m.container);
            if path.starts_with(CONTAINER_DIR) || Path::new(CONTAINER_DIR).starts_with(&path) {
                return Err(format!(
                    "host connections overlap mount destination {:?}",
                    m.container
                ));
            }
        }
        plan.mounts.push(PlanMount {
            host: self.directory.path().to_owned(),
            container: CONTAINER_DIR.into(),
            mode: MountMode::Ro,
        });
        if self.tcp {
            plan.entrypoint = format!(
                "exec /usr/bin/python3 -I {CONTAINER_DIR}/client.py {CONTAINER_DIR}/tcp.json {} bash -lc {} _ \"$@\"",
                plan.payload_fd_count,
                crate::util::shell_quote(&plan.entrypoint)
            );
        }
        Ok(())
    }
}

// Account for lexical traversal and the standard image's /var/run -> /run alias.
fn normalized_destination(path: &str) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for part in Path::new(path).components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if let Ok(suffix) = normalized.strip_prefix("/var/run") {
        Path::new("/run").join(suffix)
    } else {
        normalized
    }
}

/// An explicit connectivity probe; never reads or logs traffic.
pub fn check(connection: &Connection) -> Result<(), String> {
    stream::connect(connection)
        .map(drop)
        .map_err(|e| format!("host connection {:?} unavailable: {e}", connection.name))
}

fn serve(listener: UnixListener, connection: Connection, stop: Arc<AtomicBool>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((client, _)) if workers.len() < MAX_CLIENTS => {
                let stop = Arc::clone(&stop);
                let connection = connection.clone();
                // Bounded workers; reconnect by path per client so host socket rotation works.
                if let Ok(worker) = thread::Builder::new().spawn(move || {
                    if let Ok(host) = stream::connect(&connection) {
                        let _ = stream::relay(stream::Stream::Unix(client), host, &stop);
                    }
                }) {
                    workers.push(worker);
                }
            }
            Ok(_) => {} // At capacity: close rather than queue unbounded work.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
