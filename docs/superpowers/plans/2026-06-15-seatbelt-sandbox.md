# Seatbelt sandbox (v1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `boot` VMM applies a self-imposed macOS Seatbelt profile (targeted-deny v1) before entering its run loop — denying IP egress, exec/fork, writes outside the VM-state dirs, and host-secret access — while HVF, vmnet, vsock, and snapshots keep working.

**Architecture:** A new `crates/sandbox` crate renders an SBPL profile string from a `SandboxPaths` allow-set and applies it in-process via `sandbox_init` (FFI, flags=0). `boot.rs` assembles the paths and calls `apply` late (after HVF/vmnet/vsock are up, before the vCPU run loop) on all three run paths, fail-closed, with a logged `--no-sandbox` opt-out.

**Tech Stack:** Rust (edition 2024), macOS `sandbox_init`/`sandbox_free_error` (libSystem, deprecated, no SDK header), `libc` for the forked-child tests.

**Spec:** `docs/superpowers/specs/2026-06-15-seatbelt-sandbox-design.md`

---

## File Structure

- `crates/sandbox/Cargo.toml` — new crate `ignition-sandbox`, lib `ignition_sandbox`, dep `libc`.
- `crates/sandbox/src/lib.rs` — `SandboxPaths`, `SandboxError`, `build_profile` (pure), `apply` (FFI), `sbpl_escape`, all tests.
- `Cargo.toml` (workspace) — add `crates/sandbox` to members.
- `spike/Cargo.toml` — depend on `ignition-sandbox`.
- `spike/src/bin/boot.rs` — `--no-sandbox` flag; `apply_or_exit` helper; `SandboxPaths` assembly + `apply_or_exit` at the 3 run sites; thread `no_sandbox` into `run_restore` and `run_fuzz_mode`.
- `docs/src/internals/sandbox.md` — v1 profile, honest gap, v2 plan; linked from `docs/src/SUMMARY.md`.
- `ROADMAP.md` — mark Seatbelt v1 shipped, note deferred uid-drop + v2 deny-default.

**Build/test commands:**
- Crate tests: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-sandbox`
- Build: `PATH="$HOME/.cargo/bin:$PATH" cargo build`
- Clippy: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-sandbox`

`~/.cargo/bin` is not on PATH by default — always prefix as shown.

---

### Task 1: New `ignition-sandbox` crate + `build_profile`

**Files:**
- Create: `crates/sandbox/Cargo.toml`
- Create: `crates/sandbox/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

This task builds the pure profile-rendering half (no syscalls), fully unit-testable in CI without the hypervisor entitlement.

- [ ] **Step 1: Create the crate manifest**

Create `crates/sandbox/Cargo.toml`:

```toml
[package]
name = "ignition-sandbox"
version = "0.0.0"
edition = "2024"
description = "Self-applied macOS Seatbelt (sandbox_init) profile for the ignition VMM process"
license = "Apache-2.0"

[lib]
name = "ignition_sandbox"
path = "src/lib.rs"

[dependencies]
libc = "0.2"
```

- [ ] **Step 2: Add the crate to the workspace**

In the workspace `Cargo.toml`, add `"crates/sandbox",` to the `members` list (after `"crates/vmnet",`):

```toml
members = [
    "crates/arch",
    "crates/hvf",
    "crates/devices",
    "crates/vmm",
    "crates/vmnet",
    "crates/sandbox",
    "spike",
]
```

- [ ] **Step 3: Write the failing tests**

Create `crates/sandbox/src/lib.rs` with the types and tests first (the function bodies come in Step 5):

```rust
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
        // base + the four risk carves
        assert!(p.contains("(allow default)"));
        assert!(p.contains("(deny network-outbound (remote ip))"));
        assert!(p.contains("(deny network-inbound  (remote ip))"));
        assert!(p.contains("(deny process-exec*)"));
        assert!(p.contains("(deny process-fork)"));
        assert!(p.contains("(deny file-write*)"));
        // writable allow-list carries each writable subpath
        assert!(p.contains("(subpath \"/tmp/store\")"));
        assert!(p.contains("(subpath \"/tmp/snaps\")"));
        // temp clone root always present
        assert!(p.contains("(subpath \"/private/var/folders\")"));
        // readable emitted as explicit read-allow (forward-proof for v2)
        assert!(p.contains("(allow file-read* (subpath \"/tmp/kern/Image\")"));
        // secret block present
        assert!(p.contains("Library/Keychains"));
    }

    #[test]
    fn sbpl_escapes_quotes_and_backslashes() {
        let raw = "/tmp/a \"b\"\\c";
        let esc = sbpl_escape(raw);
        // every embedded quote and backslash is backslash-escaped
        assert_eq!(esc, "/tmp/a \\\"b\\\"\\\\c");
        // and it round-trips into a literal with no bare quote that would end the string
        let lit = format!("(subpath \"{esc}\")");
        // the only unescaped quotes are the two delimiters
        let bare = lit.matches('"').count() - lit.matches("\\\"").count();
        assert_eq!(bare, 2, "exactly the two delimiter quotes are unescaped");
    }

    #[test]
    fn secret_deny_is_the_last_block() {
        // even when a writable path overlaps a secret dir, the secret deny wins
        // because it is rendered last (SBPL last-match-wins).
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
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-sandbox`
Expected: FAIL — `build_profile` / `sbpl_escape` not found.

- [ ] **Step 5: Implement `sbpl_escape` and `build_profile`**

Add to `crates/sandbox/src/lib.rs` (above the `#[cfg(test)]` block):

```rust
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

    // Reads are allowed by default in v1; emit explicit read-allows for the
    // declared readable paths anyway, so flipping to (deny default) in v2 keeps
    // them valid with no extra work.
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
    out.push_str("  (subpath \"/private/var/folders\")\n"); // temp_dir() clone root
    for w in &paths.writable {
        out.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&w.to_string_lossy())));
    }
    out.push_str(")\n\n");

    // ALWAYS LAST: host-secret deny overrides any writable allow above.
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
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-sandbox`
Expected: PASS (3 tests).

- [ ] **Step 7: Build + clippy**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-sandbox && PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-sandbox`
Expected: clean, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/sandbox/Cargo.toml crates/sandbox/src/lib.rs Cargo.toml
git commit -m "feat(sandbox): ignition-sandbox crate + SBPL profile renderer (v1)"
```

---

### Task 2: `apply` (FFI) + forked-child integration tests

**Files:**
- Modify: `crates/sandbox/src/lib.rs`

`sandbox_init` is irreversible and process-wide, so each integration assertion runs in
a forked child that applies the sandbox and reports via exit code; the parent never
sandboxes itself.

- [ ] **Step 1: Write the failing integration tests**

Add to the `#[cfg(test)] mod tests` block in `crates/sandbox/src/lib.rs`:

```rust
    use std::io::Write;

    /// Run `body` in a forked child after applying `paths`. Returns true iff the
    /// child exited 0. The parent stays unsandboxed (sandbox_init is irreversible).
    #[cfg(target_os = "macos")]
    fn in_sandboxed_child(paths: &SandboxPaths, body: impl FnOnce() -> bool) -> bool {
        // SAFETY: fork in a test; child does minimal work then _exit (no Drop, no
        // atfork hazards beyond what std permits for these simple calls).
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
            // routable, unlikely to connect even if allowed; under the sandbox the
            // socket/connect is refused with PermissionDenied before any network I/O.
            match TcpStream::connect_timeout(
                &"1.1.1.1:80".parse().unwrap(),
                Duration::from_secs(2),
            ) {
                Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
                Ok(_) => false, // a successful connect means egress was NOT denied
            }
        });
        assert!(ok, "TcpStream::connect must be PermissionDenied under the sandbox");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn write_jail_after_apply() {
        let ok = in_sandboxed_child(&minimal_paths(), || {
            // allowed: under temp_dir()
            let tmp = std::env::temp_dir().join("ign-sbtest-allowed");
            let allowed = std::fs::File::create(&tmp).is_ok();
            let _ = std::fs::remove_file(&tmp);
            // denied: under $HOME
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-sandbox`
Expected: FAIL — `apply` not found.

- [ ] **Step 3: Implement the FFI and `apply`**

Add to `crates/sandbox/src/lib.rs` (above the `#[cfg(test)]` block):

```rust
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
    // Ensure each writable dir exists (we own them) then canonicalize so the
    // SBPL subpath matches the kernel's resolved path.
    let mut canonical = SandboxPaths { readable: Vec::new(), writable: Vec::new() };
    for w in &paths.writable {
        std::fs::create_dir_all(w).map_err(SandboxError::Path)?;
        canonical.writable.push(std::fs::canonicalize(w).map_err(SandboxError::Path)?);
    }
    for r in &paths.readable {
        // readable paths are validated as existing by the caller before apply;
        // canonicalize best-effort, falling back to the raw path.
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
```

(The `use std::io::Write;` already added in Step 1's test block may be unused — if
clippy flags it, remove it; it is only there if a test uses it.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test -p ignition-sandbox`
Expected: PASS — all 6 tests (3 pure + 3 forked-child). The forked-child tests
genuinely apply `sandbox_init` and assert egress/write behavior.

- [ ] **Step 5: Build + clippy**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build -p ignition-sandbox && PATH="$HOME/.cargo/bin:$PATH" cargo clippy -p ignition-sandbox`
Expected: clean. If the `use std::io::Write;` import is unused, remove it.

- [ ] **Step 6: Commit**

```bash
git add crates/sandbox/src/lib.rs
git commit -m "feat(sandbox): apply() via sandbox_init FFI + forked-child egress/write tests"
```

---

### Task 3: Wire the sandbox into `boot.rs`

**Files:**
- Modify: `spike/Cargo.toml`
- Modify: `spike/src/bin/boot.rs`

Apply the sandbox late on all three run paths, fail-closed, with a `--no-sandbox`
opt-out. The boot path and restore/fuzz paths assemble their own `SandboxPaths` from
locals in scope; a shared `apply_or_exit` carries the WARN / fail-closed policy.

- [ ] **Step 1: Add the dependency**

In `spike/Cargo.toml`, under `[dependencies]`, add:

```toml
ignition-sandbox = { path = "../crates/sandbox" }
```

- [ ] **Step 2: Add the `--no-sandbox` flag**

In `boot.rs` `main()`, near the other `let mut` flag defaults (around line 545), add:

```rust
    let mut no_sandbox = false;
```

In the arg-parsing `match`, add an arm (next to `"--force"`):

```rust
            "--no-sandbox" => {
                no_sandbox = true;
            }
```

- [ ] **Step 3: Add the `apply_or_exit` helper**

Add this free function to `boot.rs` (near `spawn_vsock_reactor`):

```rust
/// Apply the Seatbelt sandbox, or exit. Fail-closed: an apply error terminates
/// the process (a security gate must not silently degrade open). `--no-sandbox`
/// is the one explicit, loudly-logged way to run unconfined.
fn apply_or_exit(paths: &ignition_sandbox::SandboxPaths, no_sandbox: bool) {
    if no_sandbox {
        eprintln!("WARN: sandbox disabled (--no-sandbox) — VMM runs unconfined");
        return;
    }
    if let Err(e) = ignition_sandbox::apply(paths) {
        eprintln!("FATAL: failed to apply sandbox: {e}");
        process::exit(1);
    }
    eprintln!("sandbox: applied (targeted-deny v1)");
}
```

- [ ] **Step 4: Apply on the boot path**

In `main()`, immediately before the boot-path run-loop call `match manager.run(entry, fdt_addr)` (around line 1004), insert:

```rust
    // Jail the VMM before running guest code. Reads of kernel/rootfs are already
    // done or held; writes must stay open for snapshot-on-demand to the store.
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: [Some(PathBuf::from(&positionals[0])), positionals.get(1).map(PathBuf::from)]
            .into_iter().flatten().collect(),
        writable: [Some(store.clone()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    apply_or_exit(&sb_paths, no_sandbox);
```

(`positionals[0]` is the kernel, `positionals.get(1)` the optional rootfs; `store` and
`vsock_uds` are the parsed locals in `main`.)

- [ ] **Step 5: Thread `no_sandbox` into `run_restore` and apply there**

Change the `run_restore` signature to accept `no_sandbox: bool` (add it as the last
param). Its current signature is around line 1335:

```rust
fn run_restore(
    store: &std::path::Path,
    rname: &str,
    name: Option<String>,
    force: bool,
    track_dirty: bool,
    vsock_uds: Option<PathBuf>,
    no_sandbox: bool,
) -> io::Result<()> {
```

Update the call site in `main()` (around line 644):

```rust
        match run_restore(&store, &rname, name.clone(), force, track_dirty, vsock_uds, no_sandbox) {
```

In `run_restore`, immediately before `let run_result = manager.run_restored(...)`
(around line 1754), insert:

```rust
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: vec![store.to_path_buf()], // restore reads the base from the store
        writable: [Some(store.to_path_buf()), Some(std::env::temp_dir()),
                   vsock_uds.as_ref().and_then(|u| u.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    apply_or_exit(&sb_paths, no_sandbox);
```

- [ ] **Step 6: Thread `no_sandbox` into `run_fuzz_mode` and apply there**

Change the `run_fuzz_mode` signature (around line 1046) to accept `no_sandbox: bool`
as the last parameter:

```rust
fn run_fuzz_mode(
    kernel_path: &std::path::Path,
    initramfs: &std::path::Path,
    solutions: &std::path::Path,
    seed: Option<&std::path::Path>,
    replay: Option<Vec<u8>>,
    window_size: u64,
    ram_size: u64,
    reset_mode: ignition_vmm::fuzz::controller::ResetMode,
    metrics_path: Option<PathBuf>,
    no_sandbox: bool,
) -> io::Result<()> {
```

Update its call site in `main()` (around line 673):

```rust
        match run_fuzz_mode(&kernel_path, &initramfs, &solutions, seed_path.as_deref(), replay, window_size, ram_size, reset_mode, metrics_path, no_sandbox) {
```

In `run_fuzz_mode`, immediately before its run-loop call (`manager.run(...)`, around
line 1310), insert:

```rust
    let sb_paths = ignition_sandbox::SandboxPaths {
        readable: vec![kernel_path.to_path_buf(), initramfs.to_path_buf()],
        writable: [Some(solutions.to_path_buf()), Some(std::env::temp_dir()),
                   metrics_path.as_ref().and_then(|m| m.parent().map(PathBuf::from))]
            .into_iter().flatten().collect(),
    };
    apply_or_exit(&sb_paths, no_sandbox);
```

- [ ] **Step 7: Build**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo build`
Expected: clean build of `boot`. Fix any borrow/move issues (e.g. clone `store`/paths
as shown; `metrics_path` is moved into the controller later — assemble `sb_paths`
before that move, or clone the parent).

- [ ] **Step 8: Update the usage strings**

In `main()`, both `eprintln!("usage: ...")` lines (around lines 695-696), add
`[--no-sandbox]` to the optional-flags list so the flag is documented.

- [ ] **Step 9: Smoke-check the flag is wired**

Run:
```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build 2>&1 | tail -2
grep -n "no_sandbox\|apply_or_exit" spike/src/bin/boot.rs
```
Expected: build succeeds; `apply_or_exit` is called on all three run paths and
`no_sandbox` threads through both function signatures.

- [ ] **Step 10: Commit**

```bash
git add spike/Cargo.toml spike/src/bin/boot.rs
git commit -m "feat(sandbox): apply Seatbelt on all run paths, --no-sandbox opt-out, fail-closed"
```

---

### Task 4: Docs + ROADMAP

**Files:**
- Create: `docs/src/internals/sandbox.md`
- Modify: `docs/src/SUMMARY.md`
- Modify: `ROADMAP.md`

- [ ] **Step 1: Write the internals doc**

Create `docs/src/internals/sandbox.md`:

```markdown
# Seatbelt sandbox

`boot` confines itself with a macOS Seatbelt profile (`sandbox_init`) applied late in
startup — after Hypervisor.framework is up, vmnet is started, and the vsock control
socket is bound, but before any guest code runs. The profile ships in the binary and
is on by default; `--no-sandbox` disables it (logged loudly), and an apply failure is
fatal (fail-closed).

## What v1 denies (targeted-deny)

The profile is `(allow default)` then carves out the high-value escape surfaces:

- **Network egress/ingress** — the VMM opens no IP sockets itself. vsock is
  AF_UNIX-local and vmnet rides vmnet.framework's XPC path, so neither is affected.
- **`process-exec*` / `process-fork`** — no spawning shells or helpers.
- **Filesystem writes** — denied everywhere except `/private/var/folders` (the
  `temp_dir()` CoW-clone root) and the declared VM-state dirs (the `--store`, plus the
  solutions/metrics/vsock paths in scope for the run mode).
- **Host secrets** — `~/.ssh`, `~/.aws`, `~/.gnupg`, and the Keychains are denied for
  both read and write. This block is emitted **last**, so it overrides a `--store`
  that a user points inside a secret dir (SBPL is last-match-wins).

## What v1 does NOT yet confine (honest gap)

v1 leaves arbitrary host **reads** and the full **mach** surface allowed (that is what
keeps HVF and vmnet working without enumerating undocumented service names). A
compromised VMM could still read most of the filesystem. Closing that is **v2**: flip
the base to `(deny default)` and grow an explicit allow-list (including the HVF and
vmnet mach services). The declared `readable` paths are already emitted as explicit
read-allows so that flip is a one-liner. The separate-uid privilege drop is also a
deferred follow-up.

## Threat-model line

With v1: egress, exec, arbitrary writes, and secret access are confined — "your own
code, your own machine" with a real process jail around the VMM. Multi-tenant /
untrusted positioning still waits on v2 (full read + mach confinement) and the uid
drop.
```

- [ ] **Step 2: Link it from SUMMARY.md**

In `docs/src/SUMMARY.md`, under the `# Internals` section, add a line after
`- [Validation spike](internals/validation-spike.md)`:

```markdown
- [Seatbelt sandbox](internals/sandbox.md)
```

- [ ] **Step 3: Update ROADMAP**

In `ROADMAP.md`, the "Hardening & honesty gates" section currently has:

```
- [ ] **Seatbelt sandbox** — `sandbox_init` profile + separate uid (no Linux jailer/seccomp
  equivalent). **Gates any "untrusted / multi-tenant" positioning.** Until it lands, lead
  with "your own code, your own machine," never "secure multi-tenant hosting."
```

Replace with:

```
- [~] **Seatbelt sandbox** — **v1 shipped** (`docs/src/internals/sandbox.md`): self-applied
  `sandbox_init` targeted-deny profile (no IP egress, no exec/fork, writes only to VM-state
  dirs, host secrets denied), on by default, fail-closed, `--no-sandbox` opt-out; HVF + vmnet
  intact. **Remaining (v2):** flip to `(deny default)` for full read+mach confinement, and the
  separate-uid privilege drop. Until v2, lead with "your own code, your own machine."
```

Also in the parity table (around line 236), the Jailer/seccomp row:

```
| Jailer / seccomp | ❌ planned (Seatbelt) | ✅ | gates untrusted-tenant claims |
```

becomes:

```
| Jailer / seccomp | 🟡 Seatbelt v1 (targeted-deny) | ✅ | full deny-default + uid drop = v2 |
```

- [ ] **Step 4: Verify docs build**

Run: `PATH="$HOME/.cargo/bin:$PATH" mdbook build docs 2>/dev/null && echo "BOOK OK" || echo "mdbook not installed — skipping"`
Expected: `BOOK OK` (or skip; not fatal).

- [ ] **Step 5: Commit**

```bash
git add docs/src/internals/sandbox.md docs/src/SUMMARY.md ROADMAP.md
git commit -m "docs(sandbox): internals chapter + SUMMARY link + ROADMAP v1 status"
```

---

## Self-Review

**Spec coverage:**
- New `crates/sandbox` with `SandboxPaths`/`SandboxError`/`build_profile`/`apply` → Tasks 1, 2. ✓
- In-process `sandbox_init` FFI, flags=0, literal profile, `sandbox_free_error` → Task 2. ✓
- Targeted-deny SBPL: allow-default, network deny, exec/fork deny, write deny+allow, secret deny last (read+write) → Task 1 `build_profile` + tests. ✓
- `readable` emitted as explicit read-allows (forward-proof for v2) → Task 1 (refines the spec's illustrative profile, documented in Task 4 doc). ✓
- Path canonicalize + create-if-absent for writable → Task 2 `apply`. ✓
- SBPL escaping (no injection) → Task 1 `sbpl_escape` + test. ✓
- Applied late on all three run paths (boot, restore, fuzz) → Task 3 Steps 4-6. ✓
- `--no-sandbox` opt-out, loud WARN, fail-closed exit → Task 3 `apply_or_exit`. ✓
- Tests: profile-renders, escaping, secret-last (unit) + apply-succeeds, egress-denied, write-jail (forked-child) → Tasks 1, 2. ✓
- Docs + ROADMAP (honest gap, v2 plan) → Task 4. ✓

**Placeholder scan:** No TBD/TODO. Every code step shows full code. Approx line numbers are hints; each insertion is anchored to a named call (`manager.run`, `manager.run_restored`, the `run_*` signatures) the implementer greps for.

**Type consistency:** `SandboxPaths { readable, writable }`, `SandboxError::{Apply, Path}`, `build_profile(&SandboxPaths) -> String`, `apply(&SandboxPaths) -> Result<(), SandboxError>`, `sbpl_escape(&str) -> String`, `apply_or_exit(&SandboxPaths, bool)` — names identical across Tasks 1-3. `ignition-sandbox` / `ignition_sandbox` crate/lib names consistent.
```
