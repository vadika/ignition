//! Self-applied macOS Seatbelt profile for the ignition VMM (targeted-deny v1).
//! See docs/superpowers/specs/2026-06-15-seatbelt-sandbox-design.md.

use std::path::PathBuf;

/// Host paths the sandboxed VMM keeps accessing at runtime.
#[derive(Debug, Default, Clone)]
pub struct SandboxPaths {
    /// Files/dirs the VMM reads (kernel, rootfs, initramfs, restore base).
    /// Emitted as explicit read-allows so the v2 deny-default flip keeps them valid.
    pub readable: Vec<PathBuf>,
    /// Dirs the VMM writes (snapshot store, temp_dir clones, solutions, metrics
    /// parent, vsock uds parent).
    pub writable: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum SandboxError {
    /// sandbox_init returned non-zero; carries the (freed) errbuf text.
    Apply(String),
    /// a writable dir could not be created/canonicalized.
    Path(std::io::Error),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Apply(s) => write!(f, "sandbox_init failed: {s}"),
            SandboxError::Path(e) => write!(f, "sandbox path error: {e}"),
        }
    }
}
impl std::error::Error for SandboxError {}

/// Escape a path for use inside an SBPL `"..."` string literal: backslash and
/// double-quote are the only metacharacters that can break the literal.
pub(crate) fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the targeted-deny v1 SBPL profile for `paths`. Pure: no syscalls.
/// Order matters — the secret-deny block is emitted LAST so it wins under
/// SBPL last-match-wins, overriding any user --store/writable overlap.
pub fn build_profile(paths: &SandboxPaths) -> String {
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(allow default)\n\n");

    out.push_str(";; network egress: the VMM opens no IP sockets itself\n");
    out.push_str("(deny network-outbound (remote ip))\n");
    out.push_str("(deny network-inbound  (remote ip))\n\n");

    out.push_str(";; no spawning helpers out of a compromised VMM\n");
    out.push_str("(deny process-exec*)\n");
    out.push_str("(deny process-fork)\n\n");

    if !paths.readable.is_empty() {
        out.push_str(";; declared readable paths (explicit; redundant under allow-default)\n");
        for r in &paths.readable {
            out.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sbpl_escape(&r.to_string_lossy())
            ));
        }
        out.push('\n');
    }

    out.push_str(";; writes only to the declared VM-state dirs\n");
    out.push_str("(deny file-write*)\n");
    out.push_str("(allow file-write*\n");
    out.push_str("  (subpath \"/private/var/folders\")\n");
    for w in &paths.writable {
        out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&w.to_string_lossy())));
    }
    out.push_str(")\n\n");

    out.push_str(";; host secrets: last block, denies read+write, wins over --store overlap\n");
    out.push_str("(deny file-read* file-write*\n");
    for sub in secret_subpaths() {
        out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&sub)));
    }
    out.push_str(")\n");
    out
}

/// Host-secret directories denied (read+write) regardless of other rules.
fn secret_subpaths() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/var/empty".to_string());
    vec![
        format!("{home}/.ssh"),
        format!("{home}/Library/Keychains"),
        "/Library/Keychains".to_string(),
        format!("{home}/.aws"),
        format!("{home}/.gnupg"),
    ]
}

use std::ffi::{c_char, c_int, CString};

// macOS Seatbelt. Deprecated, no SDK header, but the symbols are live in
// libSystem (auto-linked). flags = 0 => `profile` is a literal SBPL string.
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Render the v1 profile for `paths` and apply it to the current process.
/// Process-wide and irreversible; call once, late in startup. Every thread is
/// affected, including those already spawned.
pub fn apply(paths: &SandboxPaths) -> Result<(), SandboxError> {
    let mut canonical = SandboxPaths { readable: Vec::new(), writable: Vec::new() };
    for w in &paths.writable {
        std::fs::create_dir_all(w).map_err(SandboxError::Path)?;
        canonical.writable.push(std::fs::canonicalize(w).map_err(SandboxError::Path)?);
    }
    for r in &paths.readable {
        canonical.readable.push(std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()));
    }

    let profile = build_profile(&canonical);
    let c_profile = CString::new(profile).map_err(|e| {
        SandboxError::Path(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    })?;

    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: c_profile is a valid NUL-terminated string for the call; err is a
    // valid out-pointer; on failure we read then free it via sandbox_free_error.
    let rc = unsafe { sandbox_init(c_profile.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let msg = if err.is_null() {
            "sandbox_init failed (no error string)".to_string()
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(err) };
            s
        };
        return Err(SandboxError::Apply(msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` in a forked child after applying `paths`. Returns true iff the
    /// child exited 0. The parent stays unsandboxed (sandbox_init is irreversible).
    #[cfg(target_os = "macos")]
    fn in_sandboxed_child(paths: &SandboxPaths, body: impl FnOnce() -> bool) -> bool {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let ok = apply(paths).is_ok() && body();
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }

    #[cfg(target_os = "macos")]
    fn minimal_paths() -> SandboxPaths {
        SandboxPaths { readable: vec![], writable: vec![std::env::temp_dir()] }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn apply_succeeds() {
        assert!(in_sandboxed_child(&minimal_paths(), || true));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn egress_is_denied_after_apply() {
        let ok = in_sandboxed_child(&minimal_paths(), || {
            use std::net::TcpStream;
            use std::time::Duration;
            match TcpStream::connect_timeout(
                &"1.1.1.1:80".parse().unwrap(),
                Duration::from_secs(2),
            ) {
                Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
                Ok(_) => false,
            }
        });
        assert!(ok, "TcpStream::connect must be PermissionDenied under the sandbox");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn write_jail_after_apply() {
        let ok = in_sandboxed_child(&minimal_paths(), || {
            let tmp = std::env::temp_dir().join("ign-sbtest-allowed");
            let allowed = std::fs::File::create(&tmp).is_ok();
            let _ = std::fs::remove_file(&tmp);
            let home = std::env::var("HOME").unwrap();
            let denied = std::fs::File::create(format!("{home}/ign-sbtest-denied"));
            let denied_blocked = matches!(
                denied.as_ref().map_err(|e| e.kind()),
                Err(std::io::ErrorKind::PermissionDenied)
            );
            allowed && denied_blocked
        });
        assert!(ok, "temp write allowed, $HOME write denied");
    }

    #[test]
    fn profile_contains_denies_and_writable_subpaths() {
        let paths = SandboxPaths {
            readable: vec![PathBuf::from("/tmp/kern/Image")],
            writable: vec![PathBuf::from("/tmp/store"), PathBuf::from("/tmp/snaps")],
        };
        let p = build_profile(&paths);
        assert!(p.contains("(allow default)"));
        assert!(p.contains("(deny network-outbound (remote ip))"));
        assert!(p.contains("(deny network-inbound  (remote ip))"));
        assert!(p.contains("(deny process-exec*)"));
        assert!(p.contains("(deny process-fork)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(subpath \"/tmp/store\")"));
        assert!(p.contains("(subpath \"/tmp/snaps\")"));
        assert!(p.contains("(subpath \"/private/var/folders\")"));
        assert!(p.contains("(allow file-read* (subpath \"/tmp/kern/Image\")"));
        assert!(p.contains("Library/Keychains"));
    }

    #[test]
    fn sbpl_escapes_quotes_and_backslashes() {
        let raw = "/tmp/a \"b\"\\c";
        let esc = sbpl_escape(raw);
        assert_eq!(esc, "/tmp/a \\\"b\\\"\\\\c");
        let lit = format!("(subpath \"{esc}\")");
        let bare = lit.matches('"').count() - lit.matches("\\\"").count();
        assert_eq!(bare, 2, "exactly the two delimiter quotes are unescaped");
    }

    #[test]
    fn secret_deny_is_the_last_block() {
        let paths = SandboxPaths {
            readable: vec![],
            writable: vec![PathBuf::from("/Users/x/.ssh/snaps")],
        };
        let p = build_profile(&paths);
        let allow_off = p.find("(allow file-write*").expect("writable allow present");
        let secret_off = p.find("(deny file-read* file-write*").expect("secret deny present");
        assert!(secret_off > allow_off, "secret deny must follow the writable allow");
    }
}
