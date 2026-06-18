//! VM lifecycle state machine. Owns the accumulated config, the running boot
//! child, the control-socket client, and the snapshot path<->name map.
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use crate::config::VmConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State { NotStarted, Running, Paused, Stopped }

impl State {
    pub fn as_str(&self) -> &'static str {
        match self { State::NotStarted => "Not started", State::Running => "Running",
                     State::Paused => "Paused", State::Stopped => "Stopped" }
    }
}

/// Server settings resolved at startup.
pub struct Settings {
    pub boot_bin: PathBuf,
    pub store: PathBuf,
    pub control_sock: PathBuf,
    pub kernel_default: PathBuf,
}

pub struct VmState {
    pub state: State,
    pub config: VmConfig,
    pub child: Option<Child>,
    pub paths: HashMap<String, String>, // snapshot_path -> store name
    pub settings: Settings,
}

/// Sanitize a client snapshot path into a store name: basename, drop extension,
/// non-[A-Za-z0-9_-] -> '_'.
pub fn sanitize_name(snapshot_path: &str) -> String {
    let base = snapshot_path.rsplit('/').next().unwrap_or(snapshot_path);
    let stem = base.split('.').next().unwrap_or(base);
    let s: String = stem.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if s.is_empty() { "snapshot".to_string() } else { s }
}

impl VmState {
    pub fn new(settings: Settings) -> Self {
        VmState { state: State::NotStarted, config: VmConfig::default(),
                  child: None, paths: HashMap::new(), settings }
    }

    /// Guard: config PUTs only before boot.
    pub fn ensure_not_started(&self) -> Result<(), String> {
        if self.state == State::NotStarted { Ok(()) }
        else { Err("operation not allowed post-boot".into()) }
    }

    /// PATCH /vm transition check. Returns the control action to send on success.
    pub fn vm_update(&self, target: &str) -> Result<&'static str, String> {
        match (self.state, target) {
            (State::Running, "Paused") => Ok("pause"),
            (State::Paused, "Resumed") => Ok("resume"),
            (State::Paused, "Paused") => Err("vm already paused".into()),
            (State::Running, "Resumed") => Err("vm already running".into()),
            _ => Err(format!("cannot set state {target} from {}", self.state.as_str())),
        }
    }

    /// snapshot/create precondition.
    pub fn ensure_paused_for_snapshot(&self) -> Result<(), String> {
        if self.state == State::Paused { Ok(()) }
        else { Err("vm must be paused before snapshotting".into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("/var/run/snap.file"), "snap");
        assert_eq!(sanitize_name("a b/c@d"), "c_d");
        assert_eq!(sanitize_name("/x/"), "snapshot");
    }
    fn st(state: State) -> VmState {
        let mut v = VmState::new(Settings {
            boot_bin: "boot".into(), store: "s".into(),
            control_sock: "c".into(), kernel_default: "k".into() });
        v.state = state;
        v
    }
    #[test]
    fn config_put_blocked_after_boot() {
        assert!(st(State::Running).ensure_not_started().is_err());
        assert!(st(State::NotStarted).ensure_not_started().is_ok());
    }
    #[test]
    fn pause_resume_transitions() {
        assert_eq!(st(State::Running).vm_update("Paused").unwrap(), "pause");
        assert_eq!(st(State::Paused).vm_update("Resumed").unwrap(), "resume");
        assert!(st(State::Paused).vm_update("Paused").is_err());
        assert!(st(State::Running).vm_update("Resumed").is_err());
    }
    #[test]
    fn snapshot_requires_paused() {
        assert!(st(State::Running).ensure_paused_for_snapshot().is_err());
        assert!(st(State::Paused).ensure_paused_for_snapshot().is_ok());
    }
}
