//! Session table: one live `boot --restore` child per session, keyed by id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct SessionConfig {
    pub boot_bin: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub store: PathBuf,
    pub base: String,
    pub uds_dir: PathBuf,
    pub max_sessions: usize,
    pub idle: Duration,
    pub net: bool,
}

#[derive(Debug)]
pub enum SessionError {
    CapReached(usize),
    Unknown(String),
    Spawn(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::CapReached(n) => write!(f, "session cap reached ({n})"),
            SessionError::Unknown(id) => write!(f, "unknown session: {id}"),
            SessionError::Spawn(e) => write!(f, "failed to start sandbox: {e}"),
        }
    }
}

/// Spawns a `boot --restore` child for a session. Abstracted so tests can fake it.
pub trait Spawner {
    fn spawn(&self, cfg: &SessionConfig, uds: &PathBuf) -> Result<Child, String>;
}

/// Real spawner: `boot --restore <base> --store <store> --vsock-uds <uds> [--net] <kernel> <rootfs>`.
pub struct BootSpawner;

impl Spawner for BootSpawner {
    fn spawn(&self, cfg: &SessionConfig, uds: &PathBuf) -> Result<Child, String> {
        let mut c = std::process::Command::new(&cfg.boot_bin);
        c.arg("--mem").arg("1024")
            .arg("--restore").arg(&cfg.base)
            .arg("--store").arg(&cfg.store)
            .arg("--vsock-uds").arg(uds);
        if cfg.net {
            c.arg("--net");
        }
        c.arg(&cfg.kernel).arg(&cfg.rootfs);
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        c.spawn().map_err(|e| e.to_string())
    }
}

pub struct Session {
    pub uds: PathBuf,
    pub child: Child,
    pub last_used: Instant,
}

pub struct SessionManager {
    cfg: SessionConfig,
    sessions: HashMap<String, Session>,
    next: u64,
}

/// Pure predicate used by the reaper (and unit-tested): has `last` aged past `idle`
/// relative to `now`?
pub fn elapsed_idle(now: Instant, last: Instant, idle: Duration) -> bool {
    now.duration_since(last) > idle
}

impl SessionManager {
    pub fn new(cfg: SessionConfig) -> Self {
        Self { cfg, sessions: HashMap::new(), next: 0 }
    }

    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }

    pub fn get_uds(&mut self, id: &str) -> Result<PathBuf, SessionError> {
        let s = self.sessions.get_mut(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        s.last_used = Instant::now();
        Ok(s.uds.clone())
    }

    pub fn open(&mut self, spawner: &dyn Spawner) -> Result<String, SessionError> {
        if self.sessions.len() >= self.cfg.max_sessions {
            return Err(SessionError::CapReached(self.cfg.max_sessions));
        }
        let id = format!("s{}", self.next);
        self.next += 1;
        let uds = self.cfg.uds_dir.join(format!("ign-mcp-{id}.sock"));
        let child = spawner.spawn(&self.cfg, &uds).map_err(SessionError::Spawn)?;
        self.sessions.insert(id.clone(), Session { uds, child, last_used: Instant::now() });
        Ok(id)
    }

    pub fn close(&mut self, id: &str) -> Result<(), SessionError> {
        let mut s = self.sessions.remove(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        let _ = s.child.kill();
        let _ = s.child.wait();
        let _ = std::fs::remove_file(&s.uds);
        Ok(())
    }

    /// Kill the current child and spawn a fresh clone under the same id (cold reset).
    pub fn reset(&mut self, id: &str, spawner: &dyn Spawner) -> Result<(), SessionError> {
        let uds = {
            let s = self.sessions.get_mut(id).ok_or_else(|| SessionError::Unknown(id.to_string()))?;
            let _ = s.child.kill();
            let _ = s.child.wait();
            s.uds.clone()
        };
        let child = spawner.spawn(&self.cfg, &uds).map_err(SessionError::Spawn)?;
        let s = self.sessions.get_mut(id).unwrap();
        s.child = child;
        s.last_used = Instant::now();
        Ok(())
    }

    /// Close every session whose idle time exceeds the configured idle window.
    pub fn reap_idle(&mut self) {
        let now = Instant::now();
        let stale: Vec<String> = self.sessions.iter()
            .filter(|(_, s)| elapsed_idle(now, s.last_used, self.cfg.idle))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            let _ = self.close(&id);
        }
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }
}

#[cfg(test)]
fn test_cfg() -> SessionConfig {
    SessionConfig {
        boot_bin: "/bin/false".into(),
        kernel: "/x/Image".into(),
        rootfs: "/x/rootfs.ext4".into(),
        store: "/x/store".into(),
        base: "tools-base".into(),
        uds_dir: std::env::temp_dir(),
        max_sessions: 8,
        idle: Duration::from_secs(600),
        net: false,
    }
}

#[cfg(test)]
struct FakeSpawner;

#[cfg(test)]
impl Spawner for FakeSpawner {
    fn spawn(&self, _cfg: &SessionConfig, _uds: &PathBuf) -> Result<Child, String> {
        // A trivially-spawnable, immediately-exiting child stands in for boot.
        std::process::Command::new("/bin/sh").arg("-c").arg("exit 0")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cap_blocks_new_sessions() {
        let mut mgr = SessionManager::new(SessionConfig { max_sessions: 2, ..test_cfg() });
        assert!(mgr.open(&FakeSpawner).is_ok());
        assert!(mgr.open(&FakeSpawner).is_ok());
        assert!(matches!(mgr.open(&FakeSpawner), Err(SessionError::CapReached(2))));
    }

    #[test]
    fn elapsed_idle_predicate() {
        let t0 = std::time::Instant::now();
        let idle = Duration::from_secs(600);
        assert!(elapsed_idle(t0 + Duration::from_secs(700), t0, idle));   // 700s > 600s
        assert!(!elapsed_idle(t0 + Duration::from_secs(50), t0, idle));   // 50s <= 600s
    }

    #[test]
    fn close_removes_and_frees_slot() {
        let mut mgr = SessionManager::new(SessionConfig { max_sessions: 1, ..test_cfg() });
        let sid = mgr.open(&FakeSpawner).unwrap();
        assert!(matches!(mgr.open(&FakeSpawner), Err(SessionError::CapReached(1))));
        mgr.close(&sid).unwrap();
        assert!(mgr.open(&FakeSpawner).is_ok());
    }
}
