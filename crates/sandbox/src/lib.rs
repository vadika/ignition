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

#[cfg(test)]
mod tests {
    use super::*;

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
