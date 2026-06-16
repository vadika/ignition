# Disposable Browser microVM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A throwaway Firefox-ESR-kiosk microVM cloned fresh per session from a warm snapshot, reset in place with `Ctrl-A r`, fanned out N at a time — built on an overlay root (RO ext4 + tmpfs upper) so the disk never diverges.

**Architecture:** A new browser rootfs boots via `/sbin/overlay-init` (kernel `init=`, passed by a new boot.rs `--append` cmdline flag) which `switch_root`s into a tmpfs-upper overlay; cage runs `firefox-esr --kiosk`. A one-time cold boot creates a `browser-base` snapshot; `disposable-browser.sh` restores it (no kernel/overlay needed on restore), `make-browser-base.sh` automates the snapshot via a serial readiness sentinel. The overlay makes every write land in RAM, satisfying interactive-reset's disk-non-divergence requirement by construction.

**Tech Stack:** Rust (boot.rs), Linux 6.1 aarch64 (kernel config), Alpine 3.19 arm64 rootfs (cage + Firefox ESR + Mesa llvmpipe), bash. Remote builds on artemis2. Binary: `cargo build -p ignition-spike --bin boot` then `./scripts/sign.sh target/debug/boot`. Rust tests: `cargo test -p ignition-spike --bin boot`.

---

## Spec

Source: `docs/superpowers/specs/2026-06-16-disposable-browser-design.md`. Read it first. Sub-project A (interactive reset, `Ctrl-A c`/`Ctrl-A r`) is merged on `main`. The load-bearing constraint: `Ctrl-A r` does not rewind the disk, so the disk must not diverge — the overlay root (RO ext4 lower + tmpfs upper) guarantees that. The overlay/`init=`/kernel-overlay machinery is exercised ONLY at the one cold boot that creates the warm-base; restore/fan-out/reset never reload the kernel.

## Scope & execution reality

- **Task 1 (boot.rs `--append`)** is pure Rust — full TDD, runs in-session.
- **Tasks 4, 5 (shell wrappers)** are authored + gated by `bash -n` (and a tiny arg-validation check); they don't need the hypervisor.
- **Tasks 2, 3 (kernel config + browser rootfs)** edit build scripts; the actual artifact build runs **remotely on artemis2** and is a manual step (see `docs/src/getting-started/guest-assets.md`). The automatable gate is `bash -n`.
- **Bring-up unknowns** (Firefox-on-llvmpipe Moz env; overlay `switch_root` vs `pivot_root`) are resolved by the **live cold-boot eyeball (Task 7)** — a human step. Tasks 2/3 author the best-known shape; Task 7 validates and may require small iteration.
- **Task 6 (docs)** is prose.

## Verified ground truth (file:line, read 2026-06-16)

- boot.rs arg loop: `while let Some(a) = it.next() { match a.as_str() { ... } }` at `spike/src/bin/boot.rs:765-849`; flags set `let mut <x>` locals declared ~`:756-763`; unknown `-` flag exits 2 at `:844`.
- Normal-boot FDT cmdline: `cmdline: layout::default_cmdline(),` in the `FdtConfig`/equivalent built around `spike/src/bin/boot.rs:1004` (restore builds no FDT — it does not reload the kernel, so `--append` only affects the cold/normal boot path).
- `layout::default_cmdline()` = `"console=ttyS0 earlycon=uart8250,mmio,{MMIO_BASE:#x} root=/dev/vda rw rootwait reboot=k panic=1"` at `crates/arch/src/aarch64/layout.rs:45-47`; it has a unit test at `:94-97`.
- Kernel `scripts/config` block: `kimage/build/build-kernel.sh:38-52` (a `./scripts/config --enable ... --disable ...` chain) followed by `make olddefconfig` `:55` and grep-echo verification `:56-59`.
- GUI rootfs (the base to derive from): `kimage/build/build-rootfs-gui.sh` — full file already in context. Key bits: provisioning inside `docker run ... alpine:3.19 sh -euxc '...'`; the `cage-kiosk` openrc service heredoc'd as `<<'"'"'CAGEEOF'"'"'` runs `command="/usr/bin/cage"` / `command_args="-- /usr/bin/foot"`; the netwatch poller heredoc'd as `<<'"'"'NETEOF'"'"'`; community repo enabled; packed into a 768 MiB ext4 with `mke2fs -d`.
- Fan-out wrapper pattern: `scripts/fanout-gui.sh` — `set -euo pipefail`, `usage()`, numeric-N validation, `ROOT`/`BOOT` resolution + `[ -x "$BOOT" ]` guard, `pids=()` + `trap cleanup EXIT INT TERM`, loop launching `"$BOOT" --gui --restore "$BASE" "$@" &`, `wait`.

## CRITICAL build-script rules (memory: repeated failures)

- **No apostrophes in any comment inside `build-rootfs-browser.sh`.** The provisioning runs as `sh -euxc '...'`; a `'` in a comment (e.g. "Firefox's profile") closes that string and breaks the build. Write "Firefox profile". Verify with `grep -nE "[a-z]'[a-z]" kimage/build/build-rootfs-browser.sh` → must be empty.
- **Nested heredocs use the `<<'"'"'EOF'"'"'` quoting** (single-quoted delimiter inside the outer single-quoted `-c` string), exactly as `build-rootfs-gui.sh` does for its `CAGEEOF`/`NETEOF` blocks.
- Validate every script with `bash -n <file>` before committing.

---

## Task 1: boot.rs `--append` cmdline flag

**Files:**
- Modify: `spike/src/bin/boot.rs`
- Test: inline `#[cfg(test)]` in `spike/src/bin/boot.rs`

A general, minimal cmdline knob. Used here to pass `init=/sbin/overlay-init`. The composition goes through a small pure helper so it is unit-testable; default behavior is unchanged when the flag is absent.

- [ ] **Step 1: Write the failing test**

Add to the boot.rs `#[cfg(test)] mod tests` (where the `step()` tests live):

```rust
    #[test]
    fn build_cmdline_without_append_is_default() {
        assert_eq!(build_cmdline(None), ignition_arch::aarch64::layout::default_cmdline());
    }

    #[test]
    fn build_cmdline_appends_extra_args() {
        let got = build_cmdline(Some("init=/sbin/overlay-init"));
        assert!(got.starts_with(&ignition_arch::aarch64::layout::default_cmdline()));
        assert!(got.ends_with(" init=/sbin/overlay-init"));
    }
```

(Use whatever path boot.rs already imports `layout` under — it calls `layout::default_cmdline()`, so `layout::default_cmdline()` is in scope; match that exact path in the assertions instead of `ignition_arch::...` if that is how it is imported.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ignition-spike --bin boot build_cmdline`
Expected: FAIL — `cannot find function build_cmdline`.

- [ ] **Step 3: Implement the helper**

Add a free fn near `fuzz_cmdline()` in boot.rs:

```rust
/// The normal-boot kernel command line, optionally with extra args appended
/// (`--append`). Used to pass e.g. `init=/sbin/overlay-init` for the overlay-root
/// browser rootfs. Absent `--append` reproduces `layout::default_cmdline()`.
fn build_cmdline(append: Option<&str>) -> String {
    let base = layout::default_cmdline();
    match append {
        Some(extra) if !extra.is_empty() => format!("{base} {extra}"),
        _ => base,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ignition-spike --bin boot build_cmdline`
Expected: PASS (2 tests).

- [ ] **Step 5: Wire the flag into the arg loop**

Add a local beside the others (~`:756-763`):

```rust
    let mut append: Option<String> = None;
```

Add an arm in the `match a.as_str()` loop (before the `other if other.starts_with('-')` catch-all, ~`:843`):

```rust
            "--append" => {
                append = Some(it.next().expect("--append needs a string").to_string());
            }
```

- [ ] **Step 6: Use the composed cmdline at the FDT build site**

At the normal-boot `FdtConfig` construction (~`:1004`), replace:

```rust
        cmdline: layout::default_cmdline(),
```
with:
```rust
        cmdline: build_cmdline(append.as_deref()),
```

If there is an `eprintln!("cmdline: {}", layout::default_cmdline());` echo nearby (~`:1049`), update it to `build_cmdline(append.as_deref())` too so the logged cmdline matches what is actually used. (Leave the fuzz path's `fuzz_cmdline()` untouched.)

- [ ] **Step 7: Build, test, clippy**

Run:
```bash
cargo build -p ignition-spike --bin boot 2>&1 | tail -3
cargo test -p ignition-spike --bin boot 2>&1 | tail -3
cargo clippy -p ignition-spike --bin boot 2>&1 | tail -5
```
Expected: builds, all bin tests pass (incl. the 2 new), no NEW clippy warnings (pre-existing `run_fuzz_mode` too_many_arguments is not yours).

- [ ] **Step 8: Sign + commit**

```bash
./scripts/sign.sh target/debug/boot
git add spike/src/bin/boot.rs
git commit -m "boot: --append flag to extend the kernel cmdline (overlay-root init=)"
```
Commit message plain — NO trailer.

---

## Task 2: Kernel — enable overlayfs

**Files:**
- Modify: `kimage/build/build-kernel.sh`

Edit only; the remote rebuild + pull is a manual step (Task 7). Additive config — existing guests unaffected.

- [ ] **Step 1: Add the config symbols**

In `kimage/build/build-kernel.sh`, in the `./scripts/config --enable ...` chain (`:38-52`), add two lines (match the existing indentation / line-continuation style):

```sh
      --enable OVERLAY_FS \
      --enable TMPFS \
```

(Place them alongside `--enable VIRTIO_INPUT`; keep the trailing backslashes correct so the command still parses.)

- [ ] **Step 2: Add them to the verification grep**

After `make olddefconfig`, extend the echo-grep block (`:56-59`) so the requested symbols are confirmed post-`olddefconfig`. Add:

```sh
    grep -E "CONFIG_(OVERLAY_FS|TMPFS)=" .config || echo "OVERLAY_FS/TMPFS not set (BAD for browser rootfs)"
```

- [ ] **Step 3: Syntax-check + commit**

```bash
bash -n kimage/build/build-kernel.sh
git add kimage/build/build-kernel.sh
git commit -m "kernel: enable CONFIG_OVERLAY_FS + CONFIG_TMPFS for the overlay-root browser rootfs"
```
Commit message plain. (The actual `Image` rebuild on artemis2 + `xxd` magic check happens in Task 7.)

---

## Task 3: Browser rootfs build script

**Files:**
- Create: `kimage/build/build-rootfs-browser.sh`

Derived from `build-rootfs-gui.sh` (in context). Same base (openrc, udev/seatd, netwatch poller, ttyS0 getty, cage/libinput/xkb stack), but: add Firefox ESR + Mesa software-GL + ca-certificates + fonts; swap the cage service from `foot` to `firefox-esr --kiosk <homepage>` with a clean kiosk profile; ship `/sbin/overlay-init`; add the serial readiness hook; pack a larger ext4. Edit only; remote build is Task 7.

**OBSERVE the CRITICAL build-script rules above: no apostrophes in `sh -euxc '...'` comments; nested heredocs use `<<'"'"'EOF'"'"'`.**

- [ ] **Step 1: Create the script**

Create `kimage/build/build-rootfs-browser.sh`. Start from a copy of `build-rootfs-gui.sh` and apply these exact changes:

1. **Header comment**: describe the browser rootfs (overlay-root via `/sbin/overlay-init`, Firefox ESR kiosk, Mesa llvmpipe). No apostrophes.
2. **Output names**: `rootfs-browser.tar` / `rootfs-browser.ext4`, container name `fcroot_browser_build`, label `rootfs-browser`.
3. **Homepage build arg** near the top (host side, before the docker run):
   ```sh
   HOMEPAGE="${HOMEPAGE:-https://duckduckgo.com}"
   ```
   and pass it into the container env: add `-e HOMEPAGE="$HOMEPAGE"` to the `docker run` line.
4. **Packages**: after the existing `apk add --no-cache cage foot seatd ...` line, add a line installing the browser stack:
   ```sh
   apk add --no-cache firefox-esr mesa-dri-gallium mesa-gl ca-certificates font-dejavu
   ```
   (Keep `foot` — harmless, useful for a debug shell. cage will run firefox, not foot.)
5. **Kiosk profile + env**: create a Firefox policies/prefs that disable first-run, telemetry, and update checks and set the homepage. Inside the provisioning script add (no apostrophes in comments):
   ```sh
   # Firefox kiosk policy: no first-run, no telemetry, no update checks, set homepage.
   mkdir -p /usr/lib/firefox/distribution
   cat > /usr/lib/firefox/distribution/policies.json <<'"'"'POLEOF'"'"'
{
  "policies": {
    "DisableTelemetry": true,
    "DisableFirefoxStudies": true,
    "DisableAppUpdate": true,
    "DontCheckDefaultBrowser": true,
    "OverrideFirstRunPage": "",
    "OverridePostUpdatePage": "",
    "Homepage": { "URL": "__HOMEPAGE__", "StartPage": "homepage" }
  }
}
POLEOF
   sed -i "s|__HOMEPAGE__|$HOMEPAGE|" /usr/lib/firefox/distribution/policies.json
   ```
6. **Rewrite the `cage-kiosk` service** so cage runs Firefox in kiosk mode (replace the `command_args="-- /usr/bin/foot"` and add the software-GL env). In the `<<'"'"'CAGEEOF'"'"'` heredoc set:
   ```sh
   command="/usr/bin/cage"
   command_args="-- /usr/bin/firefox-esr --kiosk __HOMEPAGE__"
   ```
   and add software-GL env exports near the existing `WLR_RENDERER` exports:
   ```sh
   export LIBGL_ALWAYS_SOFTWARE=1
   export GALLIUM_DRIVER=llvmpipe
   export MOZ_ENABLE_WAYLAND=1
   ```
   After the heredoc is written, substitute the homepage into the service file:
   ```sh
   sed -i "s|__HOMEPAGE__|$HOMEPAGE|" /etc/init.d/cage-kiosk
   ```
   (Firefox-on-llvmpipe may need an additional Moz env — `MOZ_WEBRENDER=1` or `MOZ_ACCELERATED=0`. This is the documented bring-up unknown resolved in Task 7; leave a comment, no apostrophes, noting it.)
7. **Ship `/sbin/overlay-init`** (the cold-boot PID1). Inside provisioning:
   ```sh
   # overlay-root setup: mount tmpfs upper, overlay it onto the RO ext4 lower,
   # then switch_root into openrc. Makes every guest write land in RAM so the
   # disk never diverges (required for Ctrl-A r reset). Passed via init= on the
   # kernel cmdline at cold boot only; restore inherits the mounted overlay.
   cat > /sbin/overlay-init <<'"'"'OVLEOF'"'"'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t tmpfs tmpfs /mnt
mkdir -p /mnt/up /mnt/work /mnt/root /mnt/lower
mount --bind / /mnt/lower
mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/mnt/up,workdir=/mnt/work /mnt/root
exec switch_root /mnt/root /sbin/init
OVLEOF
   chmod +x /sbin/overlay-init
   ```
   (If the cold-boot eyeball in Task 7 shows `switch_root` failing from a non-initramfs root, the fallback is `pivot_root` — that swap is a Task 7 bring-up fix, not a blocker here.)
8. **Readiness hook** for `make-browser-base.sh`. Add a local.d service that signals on ttyS0 once Firefox is up:
   ```sh
   # Readiness sentinel for make-browser-base.sh: once the firefox process is up
   # and settled, print a marker on the serial console so the host can snapshot.
   cat > /etc/local.d/browser-ready.start <<'"'"'RDYEOF'"'"'
#!/bin/sh
( i=0
  while [ "$i" -lt 120 ]; do
    if pgrep -f firefox >/dev/null 2>&1; then
      sleep 6
      echo BROWSER_READY > /dev/ttyS0
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done ) &
RDYEOF
   chmod +x /etc/local.d/browser-ready.start
   ```
   (`rc-update add local boot` is already present in the gui-derived script — confirm it stays so local.d runs.)
9. **ext4 size**: Firefox + Mesa is large. Bump the `mke2fs` size from `768M` to `1536M` (1.5 GiB) in the packing docker step.

- [ ] **Step 2: Syntax + apostrophe checks**

```bash
bash -n kimage/build/build-rootfs-browser.sh
grep -nE "[a-z]'[a-z]" kimage/build/build-rootfs-browser.sh && echo "APOSTROPHE FOUND - FIX" || echo "no apostrophes - ok"
```
Expected: `bash -n` clean; no apostrophes. If any apostrophe is reported, rewrite that comment.

- [ ] **Step 3: Commit**

```bash
git add kimage/build/build-rootfs-browser.sh
git commit -m "rootfs: build-rootfs-browser.sh (Firefox ESR kiosk, Mesa llvmpipe, overlay root)"
```
Commit message plain. (Remote build + `ls`/firefox-presence check is Task 7.)

---

## Task 4: `disposable-browser.sh` launcher

**Files:**
- Create: `scripts/disposable-browser.sh`

Mirrors `scripts/fanout-gui.sh` but defaults the snapshot name, defaults `--mem 1024 --track-dirty`, and supports `-n N` fan-out.

- [ ] **Step 1: Create the script**

```bash
#!/usr/bin/env bash
# Launch disposable Firefox-kiosk microVMs restored from a warm-base snapshot.
# Each clone is an independent `boot --gui --net --mem 1024 --track-dirty
# --restore <base>` process: its own window, its own CoW instance dir (keyed by
# pid), its own MAC/IP. Ctrl-A r inside a clone snaps it back to the warm
# homepage; closing the window tears that clone down. The base snapshot is never
# mutated.
#
# usage: disposable-browser.sh [-n N] [snapshot-name] [extra boot args...]
#   e.g. disposable-browser.sh                       # 1 clone of browser-base
#        disposable-browser.sh -n 3                  # 3 clones
#        sudo disposable-browser.sh -n 3 browser-base   # 3 networked clones
# --net needs sudo (vmnet shared/NAT). --net is added by default; pass extra
# boot args after the snapshot name to override store etc.
set -euo pipefail

N=1
if [ "${1:-}" = "-n" ]; then
  N="${2:-}"
  case "$N" in '' | *[!0-9]*) echo "usage: $0 [-n N] [snapshot-name] [extra boot args...]" >&2; exit 2 ;; esac
  [ "$N" -ge 1 ] || { echo "N must be >= 1" >&2; exit 2; }
  shift 2
fi

BASE="${1:-browser-base}"
[ $# -ge 1 ] && shift || true

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BOOT="$ROOT/target/debug/boot"
[ -x "$BOOT" ] || {
  echo "boot binary not found or not built: $BOOT" >&2
  echo "build + sign it first: cargo build -p ignition-spike --bin boot && ./scripts/sign.sh target/debug/boot" >&2
  exit 1
}

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do
    kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

echo "launching $N disposable browser(s) from base '$BASE'"
for i in $(seq 1 "$N"); do
  "$BOOT" --gui --net --mem 1024 --track-dirty --restore "$BASE" "$@" &
  pid=$!
  pids+=("$pid")
  echo "  browser $i: pid $pid"
done
echo "all launched; Ctrl-A r resets a browser in place; Ctrl-C tears them all down"
wait
```

- [ ] **Step 2: Make executable, syntax-check, smoke-test arg parsing**

```bash
chmod +x scripts/disposable-browser.sh
bash -n scripts/disposable-browser.sh && echo "syntax ok"
# arg-parse smoke test: with no boot binary present it should reach the -x guard,
# proving the -n/name parsing didn't error first. Build is not required here; if
# boot IS built it will try to launch — so test the parse path in a subshell that
# points ROOT at an empty dir:
( cd /tmp && bash -n "$ROOT/scripts/disposable-browser.sh" ) && echo "parse ok"
```
Expected: `syntax ok`, `parse ok`. (Functional launch is the Task 7 eyeball.)

- [ ] **Step 3: Commit**

```bash
git add scripts/disposable-browser.sh
git commit -m "scripts: disposable-browser.sh — restore + fan out N browser clones from browser-base"
```
Commit message plain.

---

## Task 5: `make-browser-base.sh` warm-base creator

**Files:**
- Create: `scripts/make-browser-base.sh`

Cold-boots the browser rootfs with the overlay `init=`, watches serial for `BROWSER_READY`, then drives `Ctrl-A s` (snapshot) and `Ctrl-A x` (quit) on boot's stdin. The escape FSM maps `\x01 s` → snapshot and `\x01 x` → quit (boot.rs `step()`).

- [ ] **Step 1: Create the script**

```bash
#!/usr/bin/env bash
# Create the warm-base snapshot for the disposable browser: cold-boot the browser
# rootfs (overlay root via init=/sbin/overlay-init), wait for the guest to print
# BROWSER_READY on the serial console, then snapshot it as "browser-base" by
# sending Ctrl-A s to boot, and quit with Ctrl-A x.
#
# This is a ONE-TIME step. After it, use scripts/disposable-browser.sh.
#
# MANUAL EQUIVALENT (if you prefer to eyeball readiness yourself):
#   sudo target/debug/boot --gui --net --track-dirty --mem 1024 \
#        --append "init=/sbin/overlay-init" kimage/out/Image kimage/out/rootfs-browser.ext4
#   ...watch the window paint the homepage, then press Ctrl-A s, name it browser-base,
#   then Ctrl-A x.
#
# usage: sudo make-browser-base.sh [snapshot-name] [kernel] [rootfs]
set -euo pipefail

NAME="${1:-browser-base}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="${2:-$ROOT/kimage/out/Image}"
ROOTFS="${3:-$ROOT/kimage/out/rootfs-browser.ext4}"
BOOT="$ROOT/target/debug/boot"

[ -x "$BOOT" ] || { echo "boot not built/signed: $BOOT" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found: $KERNEL" >&2; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found: $ROOTFS" >&2; exit 1; }

# CTRL-A is 0x01. Drive boot's stdin through a FIFO so we can inject the snapshot
# and quit keystrokes after we see BROWSER_READY on its output.
fifo="$(mktemp -u)"; mkfifo "$fifo"
cleanup() { rm -f "$fifo"; [ -n "${boot_pid:-}" ] && kill "$boot_pid" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Hold the FIFO open for writing on fd 3 so boot does not see EOF.
exec 3>"$fifo"

echo "cold-booting browser rootfs to create snapshot '$NAME' ..."
# Boot reads stdin from the FIFO; its serial output goes to our stdout, which we
# tee through a reader that watches for BROWSER_READY.
"$BOOT" --gui --net --track-dirty --mem 1024 \
  --append "init=/sbin/overlay-init" --name "$NAME" \
  "$KERNEL" "$ROOTFS" <"$fifo" 2>&1 | (
    while IFS= read -r line; do
      echo "$line"
      case "$line" in
        *BROWSER_READY*)
          echo ">> guest ready; snapshotting as '$NAME'"
          printf '\001s' >&3
          ;;
        *"snapshot"*"done"* | *"[snapshot"*"]"*)
          # Snapshot confirmation seen; detach.
          sleep 1
          printf '\001x' >&3
          ;;
      esac
    done
  ) &
boot_pid=$!
wait "$boot_pid"
echo "done. snapshot '$NAME' written. Run: scripts/disposable-browser.sh $NAME"
```

> Note on the snapshot-confirmation match: boot prints `\n[snapshot requested]` when `Ctrl-A s` is received and a `[snapshot ...]` line when the write completes (see boot.rs `Action::Snapshot` dispatch + the snapshot handler eprintln). The `case` matches the bracketed forms; adjust the glob in Step 2 if the live output differs.

- [ ] **Step 2: Make executable, syntax-check**

```bash
chmod +x scripts/make-browser-base.sh
bash -n scripts/make-browser-base.sh && echo "syntax ok"
```
Expected: `syntax ok`. Functional verification is Task 7 (it needs the built rootfs + hypervisor).

- [ ] **Step 3: Commit**

```bash
git add scripts/make-browser-base.sh
git commit -m "scripts: make-browser-base.sh — auto-snapshot the warm browser base via serial readiness sentinel"
```
Commit message plain.

---

## Task 6: Documentation

**Files:**
- Create: `docs/src/features/disposable-browser.md`
- Modify: `docs/src/getting-started/guest-assets.md`
- Modify: `docs/src/SUMMARY.md` (if it exists — add the new page to the mdBook TOC)

- [ ] **Step 1: Write the showcase page**

Create `docs/src/features/disposable-browser.md` covering, in narrative prose matching the other feature pages: what it is (throwaway Firefox kiosk microVM); the overlay-root model and why it makes `Ctrl-A r` safe (link to snapshot-restore.md's reset section); building `rootfs-browser.ext4` (point to guest-assets.md); creating the warm-base (the `make-browser-base.sh` helper AND the manual `Ctrl-A s` flow with the exact `--append "init=/sbin/overlay-init"` cold-boot command); running a disposable session (`disposable-browser.sh`); `Ctrl-A r` reset behavior (snaps back to the warm homepage, history/cookies gone); fan-out with `--net` under sudo (distinct MAC/IP per clone); and the `--mem 1024` / per-clone RAM note. Do not claim live-verified specifics that depend on Task 7 until that runs — phrase Firefox-render and overlay details as the design intent.

- [ ] **Step 2: Add the rootfs build section to guest-assets.md**

In `docs/src/getting-started/guest-assets.md`, add a "## Rebuild the browser rootfs" section after the GUI rootfs section, mirroring its format:

```bash
cd kimage
scp build/build-rootfs-browser.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-browser.sh && HOMEPAGE=https://duckduckgo.com ./build-rootfs-browser.sh'
scp artemis2:'~/kbuild/out/rootfs-browser.ext4' out/rootfs-browser.ext4
```

And add a note in the kernel section that the browser rootfs additionally needs `CONFIG_OVERLAY_FS=y` (+ `CONFIG_TMPFS=y`), and that this is only required for the one-time warm-base cold boot (restore does not reload the kernel). Mention the cold-boot command uses `--append "init=/sbin/overlay-init"`.

- [ ] **Step 3: Add to SUMMARY.md if present**

```bash
test -f docs/src/SUMMARY.md && grep -q disposable-browser docs/src/SUMMARY.md || true
```
If `docs/src/SUMMARY.md` exists and lists the feature pages, add a line for `features/disposable-browser.md` next to the other features. If it does not exist, skip.

- [ ] **Step 4: Commit**

```bash
git add docs/
git commit -m "docs: disposable browser showcase + browser rootfs build steps"
```
Commit message plain.

---

## Task 7: Remote build + live eyeball (HUMAN STEP — hand back)

Not automatable: needs artemis2 (kernel/rootfs build) and the hypervisor + a GUI window (cold boot, Firefox render, snapshot, reset, fan-out). The agent STOPS after Task 6 and hands these to the human.

- [ ] **Step 1: Rebuild + pull the kernel** (CONFIG_OVERLAY_FS)
```bash
cd kimage
scp build/build-kernel.sh artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-kernel.sh && ./build-kernel.sh'
scp artemis2:'~/kbuild/out/Image' out/Image
xxd -s 56 -l 4 out/Image   # expect 4152 4d64 (ARMd)
```

- [ ] **Step 2: Build + pull the browser rootfs**
```bash
cd kimage
scp build/build-rootfs-browser.sh build/devmem.c artemis2:~/kbuild/
ssh artemis2 'cd ~/kbuild && chmod +x build-rootfs-browser.sh && ./build-rootfs-browser.sh'
scp artemis2:'~/kbuild/out/rootfs-browser.ext4' out/rootfs-browser.ext4
dd if=out/rootfs-browser.ext4 bs=1 skip=$((0x438)) count=2 2>/dev/null | xxd   # expect 53ef
```

- [ ] **Step 3: Cold-boot eyeball (overlay + Firefox)**
```bash
sudo target/debug/boot --gui --net --track-dirty --mem 1024 \
     --append "init=/sbin/overlay-init" kimage/out/Image kimage/out/rootfs-browser.ext4
```
Verify: overlay-init runs, `mount` (via the ttyS0 console) shows `overlay on /`, the ext4 lower stays read-only, and Firefox kiosk paints the homepage in the window. If `switch_root` fails, switch the overlay-init to `pivot_root` (Task 3 fallback). If Firefox does not render, add the Moz software-render env (`MOZ_WEBRENDER=1` / `MOZ_ACCELERATED=0`) in the cage service and rebuild the rootfs.

- [ ] **Step 4: Warm-base + disposable session + reset**
```bash
sudo scripts/make-browser-base.sh           # creates browser-base
sudo scripts/disposable-browser.sh           # one browser, browse around, then Ctrl-A r -> back to homepage
sudo scripts/disposable-browser.sh -n 3      # 3 windows, distinct IPs; reset one independently
```
Verify reset clears history/cookies, the guest stays interactive, and after several resets there are no ext4 errors (disk did not diverge).

- [ ] **Step 5: Report results** so the docs (Task 6) can be updated with the resolved Moz-env / switch_root specifics, then finish the branch.

---

## Self-review notes (for the executor)

- **Spec coverage:** kernel CONFIG_OVERLAY_FS (Task 2), boot.rs `--append` (Task 1), `/sbin/overlay-init` + browser rootfs + kiosk profile + readiness hook (Task 3), `disposable-browser.sh` (Task 4), `make-browser-base.sh` (Task 5), docs (Task 6), build+eyeball gate incl. the bring-up unknowns (Task 7). All spec components mapped.
- **Disk-non-divergence requirement:** satisfied by the overlay root (Task 3 overlay-init), documented (Task 6). No disk-rollback code — correct per spec.
- **Type/flag consistency:** `--append` (Task 1) is the exact flag `make-browser-base.sh` (Task 5) and the docs (Task 6) pass; `init=/sbin/overlay-init` path matches the script shipped in Task 3; `browser-base` snapshot name is the default in Tasks 4 and 5 and the output of Task 5; `--mem 1024 --track-dirty --net` consistent across Tasks 4, 5, 7.
- **Known soft spots (flag to reviewer):** (a) overlay `switch_root` vs `pivot_root` — unproven until Task 7. (b) Firefox-on-llvmpipe Moz env — finalized at Task 7. (c) the `make-browser-base.sh` snapshot-confirmation glob may need adjusting to the live `[snapshot ...]` output. All three are Task-7 bring-up items, not code-structure defects.
- **Build-script hazards:** Task 3 explicitly guards the apostrophe-in-`sh -euxc` and nested-heredoc rules (prior repeated failures).
