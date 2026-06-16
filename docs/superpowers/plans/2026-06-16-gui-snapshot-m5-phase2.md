# M5 Phase 2 — GUI fan-out Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Launch N independent GUI desktops from one warm-base snapshot — each its own window, CoW instance, and (with `--net`) MAC/IP — via a small helper script, and document the flow.

**Architecture:** No new Rust. Each clone is a plain `target/debug/boot --gui --restore <base>` process; the restore path already gives each pid its own CoW instance dir (`<store>/instances/<name>-<pid>`), so N background processes fan out automatically. A shell helper launches N, prints pids, and tears them all down on exit. Then a docs sweep.

**Tech Stack:** bash, mdBook docs.

**Spec:** `docs/superpowers/specs/2026-06-16-gui-snapshot-m5-design.md` (Plan B).

---

## Orientation (read before starting)

- `target/debug/boot --gui --restore <name>` restores a snapshot into a window
  (M5 Phase 1, already on main). It reads RAM/disk from the snapshot store
  (default `./vmstore`, override with `--store <dir>`), so it needs NO
  kernel/rootfs positionals.
- Each restore makes its own CoW instance dir keyed by pid
  (`snapshot::instance_dir(store, name, pid)`), so launching the same `--restore
  <name>` N times yields N independent guests; the immutable base is never
  mutated.
- `scripts/` holds repo helpers; `scripts/sign.sh <binary>` re-signs the boot
  binary (HVF entitlement) after a rebuild. The headless N-clone analog is
  `scripts/restore_clone_test.py`.
- winit/macOS does not expose window position through our CLI, so the helper
  does NOT tile windows (YAGNI) — macOS stacks them and the user drags them
  apart.

---

## Task 1: fan-out helper script

**Files:**
- Create: `scripts/fanout-gui.sh`
- Test: inline bash assertions (Step 1) — the script's arg validation runs
  without launching any GUI, so it is testable; the actual N-window launch is
  the operator eyeball in Task 3.

- [ ] **Step 1: Write the failing test**

Run these assertions in a terminal (they fail now — the script does not exist):

```bash
# no args → usage, exit 2
bash scripts/fanout-gui.sh; test $? -eq 2 && echo "PASS no-args" || echo "FAIL no-args"
# non-numeric N → usage, exit 2
bash scripts/fanout-gui.sh abc warm-base; test $? -eq 2 && echo "PASS bad-N" || echo "FAIL bad-N"
# syntax check
bash -n scripts/fanout-gui.sh && echo "PASS syntax" || echo "FAIL syntax"
```

Expected now: all three FAIL (file missing → bash error, not exit 2).

- [ ] **Step 2: Create the script**

`scripts/fanout-gui.sh`:

```bash
#!/usr/bin/env bash
# Fan out N GUI clones from one warm-base snapshot. Each clone is an independent
# `boot --gui --restore <base>` process: its own macOS window, its own CoW
# instance dir (keyed by pid), its own MAC/IP when --net is passed. The immutable
# base snapshot is never mutated.
#
# usage: fanout-gui.sh <N> <snapshot-name> [extra boot args...]
#   e.g. fanout-gui.sh 3 warm-base
#        fanout-gui.sh 4 warm-base --store ./vmstore
set -euo pipefail

usage() {
  echo "usage: $0 <N> <snapshot-name> [extra boot args...]" >&2
  exit 2
}

[ $# -ge 2 ] || usage
N="$1"
BASE="$2"
shift 2
case "$N" in
  '' | *[!0-9]*) usage ;;
esac
[ "$N" -ge 1 ] || usage

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

echo "fanning out $N GUI clone(s) from base '$BASE'"
for i in $(seq 1 "$N"); do
  "$BOOT" --gui --restore "$BASE" "$@" &
  pid=$!
  pids+=("$pid")
  echo "  clone $i: pid $pid"
done
echo "all clones launched; Ctrl-C tears them all down"
wait
```

Make it executable:

```bash
chmod +x scripts/fanout-gui.sh
```

- [ ] **Step 3: Run the test to verify it passes**

```bash
bash scripts/fanout-gui.sh; test $? -eq 2 && echo "PASS no-args" || echo "FAIL no-args"
bash scripts/fanout-gui.sh abc warm-base; test $? -eq 2 && echo "PASS bad-N" || echo "FAIL bad-N"
bash -n scripts/fanout-gui.sh && echo "PASS syntax" || echo "FAIL syntax"
```

Expected: all three PASS. (Note: `set -e` + the arg check exits 2 before any
launch, so these never spawn a guest.)

- [ ] **Step 4: Verify the binary-missing guard (optional sanity)**

If `target/debug/boot` does not exist, `bash scripts/fanout-gui.sh 2 warm-base`
exits 1 with the build hint. If it does exist, this step would launch GUIs —
SKIP it here; the real launch is Task 3.

- [ ] **Step 5: Commit**

```bash
git add scripts/fanout-gui.sh
git commit -m "feat(scripts): fanout-gui.sh — launch N GUI clones from a warm base"
```

---

## Task 2: documentation sweep

**Files:**
- Modify: `docs/src/features/devices.md` (GUI compositor section)
- Modify: `docs/src/features/snapshot-restore.md`
- Modify: `docs/src/getting-started/guest-assets.md`
- Modify: `ROADMAP.md`

No tests (prose). Gate: `mdbook build docs` succeeds if mdbook is installed;
otherwise visual read.

- [ ] **Step 1: devices.md — note GUI snapshot/restore + fan-out**

In `docs/src/features/devices.md`, at the END of the "Wayland compositor (cage +
foot)" subsection (after the paragraph ending "...uses the framebuffer
console."), append:

```markdown

The GUI guest also snapshots and restores: the virtio-gpu resource table +
scanout binding and the virtio-input config state survive a snapshot, and
`boot --gui --restore <name>` reopens the window and repaints the resumed
desktop before the guest runs (the device re-reads the scanout from the
restored backing — no pixel bytes are stored). A headless `--restore` (no
`--gui`) restores the same guest to the serial console with frames discarded.
Because each restore gets its own copy-on-write instance, one warm-base
snapshot fans out into N independent desktops — see
`scripts/fanout-gui.sh N <base>`.
```

- [ ] **Step 2: snapshot-restore.md — add a GUI fan-out subsection**

In `docs/src/features/snapshot-restore.md`, append a new section at the end of
the file:

```markdown
## GUI snapshot, restore & fan-out

A `--gui` guest (the cage + foot desktop over virtio-gpu/virtio-input) snapshots
and restores like any other: `Ctrl-A s` writes a snapshot of the live desktop,
and `boot --gui --restore <name>` reopens a window with the desktop resuming
where it left off. The virtio-gpu resource table and scanout binding plus the
virtio-input config cursor are serialized; pixels are not — on restore the
device re-reads the scanout from the restored guest-RAM backing and presents one
frame, so the window paints the resumed screen before the guest runs again.

Because each restore clones the immutable base into its own copy-on-write
instance dir (keyed by pid), one warm base fans out into N independent desktops,
each with its own window and — under `--net` — its own MAC and DHCP lease:

```console
# take one warm-base snapshot of a logged-in desktop (Ctrl-A s), then:
scripts/fanout-gui.sh 3 warm-base
# -> 3 boot --gui --restore processes, 3 windows, 3 isolated guests
```

The base snapshot is never mutated; closing a clone's window tears down only
that guest.
```

- [ ] **Step 3: guest-assets.md — run note**

In `docs/src/getting-started/guest-assets.md`, in the "Rebuild the GUI rootfs"
section, after the line beginning "Run it: `boot --gui --mem 512 ...`" paragraph,
append:

```markdown

To snapshot and restore the live desktop, add `--track-dirty`, press `Ctrl-A s`
to write a snapshot, then `boot --gui --restore <name>` to reopen it. Fan out N
clones from one base with `scripts/fanout-gui.sh N <name>`.
```

- [ ] **Step 4: ROADMAP.md — mark M5 done**

In `ROADMAP.md`, find the M5 entry (GUI snapshot/clone milestone) and mark it
complete in the same style as the other completed milestones (e.g. a checked box
or "done" marker — match the surrounding format). Read the file first to match
the exact convention; update the M5 line to reflect: GUI snapshot/restore +
fan-out shipped (virtio-gpu/input snapshot state, `--gui` restore window,
`fanout-gui.sh`).

- [ ] **Step 5: Build the docs (if mdbook present)**

```bash
mdbook build docs 2>/dev/null && echo "docs build OK" || echo "mdbook not installed — visual read only"
```

Expected: "docs build OK", or skip if mdbook absent.

- [ ] **Step 6: Commit**

```bash
git add docs/src/features/devices.md docs/src/features/snapshot-restore.md docs/src/getting-started/guest-assets.md ROADMAP.md
git commit -m "docs: GUI snapshot/restore + fan-out (M5)"
```

---

## Task 3: live eyeball — N-clone fan-out (operator, no code)

**Files:** none (manual hardware test on the macOS host).

- [ ] **Step 1: Build + sign (if not already current)**

```bash
cargo build -p ignition-spike --bin boot && ./scripts/sign.sh target/debug/boot
```

- [ ] **Step 2: Take a warm-base snapshot of a logged-in desktop**

```bash
target/debug/boot --gui --track-dirty --mem 512 kimage/out/Image kimage/out/rootfs-gui.ext4
```

Log in / open something in foot, press `Ctrl-A s`, note the printed snapshot
name, `Ctrl-A x` to quit.

- [ ] **Step 3: Fan out 3 clones**

```bash
scripts/fanout-gui.sh 3 <name>
```

Expected: 3 windows open, each showing the resumed desktop. Type different text
in each to confirm they are independent (one guest's input does not appear in
another). `Ctrl-C` in the launching terminal tears all three down.

- [ ] **Step 4: Confirm base immutability**

After tearing the clones down, restore the base once more
(`target/debug/boot --gui --restore <name>`) and confirm it still shows the
original snapshot content (the clones' edits did not leak into the base).

- [ ] **Step 5: Report**

Report to the operator: did 3 independent desktops come up, and did the base
stay pristine? If yes, Phase 2 (and M5) is complete.

---

## Self-Review notes

- **Spec coverage:** Plan B / B1 (helper script) → Task 1; B2 (docs:
  devices.md, snapshot-restore.md, guest-assets.md, ROADMAP.md) → Task 2;
  B3 (live eyeball N=3) → Task 3. Spec "no new Rust" → honored (bash + docs only).
- **Consistency:** script name `scripts/fanout-gui.sh` and invocation
  `fanout-gui.sh N <base>` identical across Tasks 1, 2, 3 and all doc snippets.
  Snapshot store default `./vmstore` matches the binary.
- **No placeholders:** the script is complete; doc snippets are complete prose.
  Task 2 Step 4 (ROADMAP) intentionally says "read first to match convention"
  because the exact M5 line format is repo-specific — this is a real instruction,
  not a deferral.
