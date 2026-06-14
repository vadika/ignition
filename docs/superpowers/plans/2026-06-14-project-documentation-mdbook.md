# Project Documentation Site (mdBook) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the scattered `docs/` notes into a single mdBook documentation site that serves tool-builders and contributors, preserving the raw notes and the agentic `superpowers/` reference.

**Architecture:** mdBook rooted at `docs/`, sources under `docs/src/`, output `docs/book/` (gitignored). A skeleton (book.toml + full `SUMMARY.md` + valid stub pages + CI) lands first so every commit builds and link-checks; later tasks replace each stub with real content (moves via `git mv`, folds, and three fresh concept chapters). `superpowers/` is untouched; `docs/examples/` moves to top-level `examples/`.

**Tech Stack:** mdBook + mdbook-linkcheck (Rust), GitHub Actions + Pages, markdown.

**Spec:** `docs/superpowers/specs/2026-06-14-project-documentation-mdbook-design.md`

**Global conventions for every task:**
- The build/link-check gate is the "test." After any change run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log` — it must finish with `Running the linkcheck backend` and no `ERROR`/`WARN` lines about missing files or broken links. (`mdbook-linkcheck` runs as a backend configured in `book.toml`.)
- Prereq once per machine: `cargo install mdbook mdbook-linkcheck` (skip if `mdbook --version` and `mdbook-linkcheck --version` already work).
- Code fences in chapters use language tags `console`, `text`, `c`, `toml`, `yaml`, or `rust,ignore` — never a bare ```` ``` ```` (keeps `mdbook test` from trying to compile shell/C).
- Commit messages are plain (no co-author/generated trailer).

---

### Task 1: Scaffold the book (config, SUMMARY, stub pages, CI)

**Files:**
- Create: `docs/book.toml`
- Create: `docs/src/SUMMARY.md`
- Create: 19 stub chapter pages under `docs/src/` (listed below)
- Modify: `.gitignore` (add `docs/book/`)
- Create: `.github/workflows/docs.yml`

- [ ] **Step 1: Write `docs/book.toml`**

```toml
[book]
title = "ignition"
authors = ["Vadim Likholetov"]
src = "src"
language = "en"

[output.html]
git-repository-url = "https://github.com/vadika/ignition"
edit-url-template = "https://github.com/vadika/ignition/edit/main/docs/{path}"
default-theme = "navy"

[output.html.fold]
enable = true

[output.linkcheck]
follow-web-links = false
```

- [ ] **Step 2: Create the stub pages**

Run this exact script (creates every directory and a valid stub page with a real H1 + one sentence — no placeholders, just a starting heading each later task fills):

```bash
cd docs
mkdir -p src/getting-started src/concepts src/features src/fuzzing src/benchmarks src/internals src/appendix
stub () { printf '# %s\n\n_%s_\n' "$2" "$3" > "src/$1"; }
stub introduction.md                 "ignition" "A research microVM for macOS on Apple Silicon, built on Hypervisor.framework."
stub getting-started/build-and-run.md "Build & run" "Build the workspace and the boot binary, and sign it for the hypervisor entitlement."
stub getting-started/boot-a-guest.md  "Boot a Linux guest" "Load a kernel and rootfs and get an interactive console."
stub getting-started/guest-assets.md  "Building guest assets" "Rebuild the guest kernel, rootfs, and fuzz initramfs on the remote builder."
stub concepts/architecture.md         "Architecture" "Crates, the vstate seam, and the Hypervisor.framework backend."
stub concepts/device-model.md         "Device model" "The uniform DeviceManager and MmioDevice trait."
stub concepts/clone-primitive.md      "The clone primitive" "Snapshot, lazy CoW restore, dirty tracking, and diff chains."
stub features/snapshot-restore.md     "Snapshot & restore" "Clone-capable snapshot and lazy restore."
stub features/diff-snapshots.md       "Diff / incremental snapshots" "Write-protect dirty tracking and immutable delta chains."
stub features/devices.md              "Devices, SMP & networking" "The Firecracker aarch64 device set, SMP, and vmnet networking."
stub fuzzing/overview.md              "How snapshot fuzzing works" "Park at a parse entry, inject inputs, reset the machine each iteration."
stub fuzzing/running.md               "Running the fuzzer" "The fuzz gates and the benchmark driver."
stub benchmarks/boot-and-restore.md   "Boot & restore latency" "Fresh-boot and restore timing."
stub benchmarks/diff-snapshots.md     "Diff-snapshot benchmarks" "Tracking overhead, snapshot write, restore latency."
stub benchmarks/fuzzing.md            "Snapshot-fuzzing benchmark" "Execs/sec and reset-latency on libpng."
stub internals/hvf-firecracker-map.md "HVF and Firecracker map" "Source-level mapping from Firecracker to the HVF backend."
stub internals/design-decisions.md    "Design decisions" "Why the VMM is shaped the way it is."
stub internals/validation-spike.md    "Validation spike" "The early end-to-end validation results."
stub appendix/specs-and-plans.md      "Specs & plans (agentic reference)" "Design specs and implementation plans kept under docs/superpowers/."
stub appendix/examples.md             "Examples" "Runnable walkthroughs under the top-level examples/ directory."
cd ..
```

- [ ] **Step 3: Write `docs/src/SUMMARY.md`**

```markdown
# Summary

[Introduction](introduction.md)

# Getting started
- [Build & run](getting-started/build-and-run.md)
- [Boot a Linux guest](getting-started/boot-a-guest.md)
- [Building guest assets](getting-started/guest-assets.md)

# Concepts
- [Architecture](concepts/architecture.md)
- [Device model](concepts/device-model.md)
- [The clone primitive](concepts/clone-primitive.md)

# Features
- [Snapshot & restore](features/snapshot-restore.md)
- [Diff / incremental snapshots](features/diff-snapshots.md)
- [Devices, SMP & networking](features/devices.md)

# Snapshot fuzzing
- [How it works](fuzzing/overview.md)
- [Running the fuzzer](fuzzing/running.md)

# Benchmarks
- [Boot & restore latency](benchmarks/boot-and-restore.md)
- [Diff snapshots](benchmarks/diff-snapshots.md)
- [Snapshot fuzzing](benchmarks/fuzzing.md)

# Internals
- [HVF and Firecracker map](internals/hvf-firecracker-map.md)
- [Design decisions](internals/design-decisions.md)
- [Validation spike](internals/validation-spike.md)

# Appendix
- [Specs & plans (agentic reference)](appendix/specs-and-plans.md)
- [Examples](appendix/examples.md)
```

- [ ] **Step 4: Ignore the build output**

Append to `.gitignore` (repo root):

```
# mdBook generated site
docs/book/
```

- [ ] **Step 5: Add the GitHub Pages deploy workflow**

Create `.github/workflows/docs.yml`:

```yaml
name: docs
on:
  push:
    branches: [main]
    paths: ["docs/**", ".github/workflows/docs.yml"]
  workflow_dispatch:
permissions:
  contents: read
  pages: write
  id-token: write
concurrency:
  group: pages
  cancel-in-progress: true
jobs:
  build-deploy:
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deploy.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4
      - name: Install mdBook + linkcheck
        run: |
          cargo install mdbook mdbook-linkcheck
      - name: Build
        run: mdbook build docs/
      - uses: actions/configure-pages@v5
      - uses: actions/upload-pages-artifact@v3
        with:
          path: docs/book/html
      - id: deploy
        uses: actions/deploy-pages@v4
```

(`mdbook-linkcheck` nests HTML under `book/html`; the upload path reflects that.)

- [ ] **Step 6: Build and link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: ends without `ERROR`; the `linkcheck` backend reports no broken links. `docs/book/html/index.html` exists.

- [ ] **Step 7: Commit**

```bash
git add docs/book.toml docs/src .gitignore .github/workflows/docs.yml
git commit -m "docs(book): scaffold mdBook site (config, SUMMARY, stub pages, Pages CI)"
```

---

### Task 2: Getting-started chapters

**Files:**
- Modify: `docs/src/getting-started/build-and-run.md`
- Modify: `docs/src/getting-started/boot-a-guest.md`
- Create (via move): `docs/src/getting-started/guest-assets.md` (from `REBUILD-GUEST-ASSETS.md`)
- Modify: `REBUILD-GUEST-ASSETS.md` (root → pointer)

- [ ] **Step 1: Move the guest-assets note into the book**

The stub `guest-assets.md` exists from Task 1; replace it with the real content and delete the stub-vs-source duplication by moving the source over it:

```bash
git rm docs/src/getting-started/guest-assets.md
git mv REBUILD-GUEST-ASSETS.md docs/src/getting-started/guest-assets.md
```

Then edit the moved file: change its top `# Rebuilding guest assets ...` heading to `# Building guest assets`, and tag every bare ```` ``` ```` shell fence as ```` ```console ````. Leave the content otherwise intact.

- [ ] **Step 2: Leave a root pointer**

Create `REBUILD-GUEST-ASSETS.md` (root) with exactly:

```markdown
# Rebuilding guest assets

Moved into the documentation site: see
[Building guest assets](docs/src/getting-started/guest-assets.md)
(rendered at `getting-started/guest-assets.html` in the built book).
```

- [ ] **Step 3: Write `build-and-run.md`**

Replace the stub body with content extracted from the README "Build & run" + "Requirements" lines. It MUST contain, as `console`-tagged blocks: `cargo build`, `scripts/sign.sh target/debug/boot`, and the requirements line (Apple Silicon, macOS 15+, Rust 1.96+ edition 2024). Structure:

```markdown
# Build & run

<one-paragraph: the runnable artifact is `boot`; it needs the hypervisor
entitlement, which relinking strips, so re-sign after every build.>

## Build

​```console
cargo build
cargo build -p ignition-spike --bin boot
scripts/sign.sh target/debug/boot
​```

## Requirements

Apple Silicon Mac, macOS 15+ (26 preferred), Rust 1.96+ (edition 2024).
```

- [ ] **Step 4: Write `boot-a-guest.md`**

Replace the stub body with the README "Boot a Linux guest" content. MUST contain the three `console` invocations: plain boot, `--smp 4`, `--net`, and the console-keys note (`Ctrl-A s` snapshot, `Ctrl-A x` quit, `Ctrl-A b` balloon). Add a final line: `Snapshot and restore are covered in [The clone primitive](../concepts/clone-primitive.md) and [Snapshot & restore](../features/snapshot-restore.md).`

- [ ] **Step 5: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; no broken links. (The two new intra-book links resolve to existing stubs.)

- [ ] **Step 6: Commit**

```bash
git add docs/src/getting-started REBUILD-GUEST-ASSETS.md
git commit -m "docs(book): getting-started chapters; move guest-assets, root pointer"
```

---

### Task 3: Introduction + slim the README

**Files:**
- Modify: `docs/src/introduction.md`
- Modify: `README.md`

- [ ] **Step 1: Write `introduction.md`**

Replace the stub with evergreen prose synthesized from the README intro and the ROADMAP "Thesis" section. It MUST state, in order:
1. ignition is a research microVM for macOS on Apple Silicon on Hypervisor.framework, architecturally modeled on Firecracker but sharing ~0 lines of Firecracker source; the one lifted piece is the `hvf` crate (from libkrun, reworked).
2. The differentiator: fast snapshot + clone-from-warm-base on bare HVF (`clonefile` + `MAP_SHARED`, immutable base, idle ~0% CPU, touches only dirtied pages) — which Virtualization.framework tools cannot expose cleanly.
3. Positioning: a substrate for tool-builders (agent-sandbox, fuzzing, CI), not an end-user product.
4. Two tracks: demonstrator (snapshot fuzzing) and adoption (MCP / Firecracker REST / OCI).
5. A closing "Where to go next" list linking: [Build & run](getting-started/build-and-run.md), [The clone primitive](concepts/clone-primitive.md), [How snapshot fuzzing works](fuzzing/overview.md), and the roadmap at `https://github.com/vadika/ignition/blob/main/ROADMAP.md`.

No em dashes (use commas/periods). Keep it under ~400 words.

- [ ] **Step 2: Slim the README**

Edit `README.md` so it keeps: the title + intro paragraphs (lines about what ignition is and the hvf-crate lineage), a short **Quickstart** (`cargo build` → `scripts/sign.sh target/debug/boot` → one `target/debug/boot kimage/out/Image kimage/out/rootfs.ext4`), the **Layout** section, and a new prominent line near the top:

```markdown
> **📖 Full documentation:** build the book with `mdbook serve docs/` (or see the
> published site). Source under [`docs/src/`](docs/src/SUMMARY.md).
```

Remove from README the now-duplicated deep sections: "Snapshot & restore" (the long version), "Diff snapshots", and the long boot/run walkthroughs — these live in the book now. Keep the "Status" bullet list (it's a useful landing summary) but trim each bullet to one line and end the section with "Full feature docs: the book; roadmap: `ROADMAP.md`."

- [ ] **Step 3: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; introduction's intra-book links resolve.

- [ ] **Step 4: Commit**

```bash
git add docs/src/introduction.md README.md
git commit -m "docs(book): introduction chapter; slim README to quickstart + book link"
```

---

### Task 4: Concept chapters (architecture, device-model, clone-primitive)

**Files:**
- Modify: `docs/src/concepts/architecture.md`
- Modify: `docs/src/concepts/device-model.md`
- Modify: `docs/src/concepts/clone-primitive.md`

Sources to read first: `README.md` (Layout), `docs/HANDOFF.md`, `docs/firecracker-hvf-porting-map.md`, the ROADMAP "Shipped" section.

- [ ] **Step 1: Write `architecture.md`**

Replace stub. MUST contain: the crate table (from README Layout) — `ignition-arch`, `ignition-hvf` (HVF backend, lifted from libkrun, reworked), `ignition-devices`, `ignition-vmm` (the vstate seam, HVF replacement for FC kvm/vm/vcpu), `ignition-spike` (the `boot` binary). MUST explain the vstate seam (where ignition substitutes HVF for KVM) and the run loop (ESR decode, MMIO dispatch, WFI/WFE idle, PSCI). Link `[HVF and Firecracker map](../internals/hvf-firecracker-map.md)` for the source-level detail. Use a `text`-tagged block for the crate tree.

- [ ] **Step 2: Write `device-model.md`**

Replace stub. MUST describe: the `DeviceManager` + `MmioDevice` trait (one trait, MMIO/SPI allocation, bus dispatch, FDT node emission, `DeviceRecord` snapshot hooks), and that fresh boot and restore drive one device-wiring site. List the shipped device set (virtio-blk/net/rng/balloon/vsock, PL031 RTC, boot-timer). Link `[Devices, SMP & networking](../features/devices.md)` for per-device behavior.

- [ ] **Step 3: Write `clone-primitive.md`** (the load-bearing chapter)

Replace stub. MUST cover, in this order, sourced from the ROADMAP "Snapshot / restore" shipped block + `docs/snapshot-restore-result.md` + `docs/diff-snapshot-research.md`:
1. Snapshot/restore: resume from saved PC, idles ~0% CPU; self-describing v2 format (`DeviceRecord` list, version guard); multi-vCPU stop-the-world rendezvous.
2. Fast restore: `clonefile` + `mmap(MAP_SHARED)`, lazy page fault-in, immutable base never mutated.
3. Snapshot store: `snapshots/<name>/` bases + `instances/<name>-<pid>/` CoW clones + `manifest.json`.
4. Dirty tracking on HVF: `hv_vm_protect` write-protect, 16 KiB granule, first write traps (Data-Abort translation fault), marks dirty, re-grants — the novel platform bit (no `KVM_GET_DIRTY_LOG` equivalent).
5. Diff / incremental snapshots: immutable delta chain, restore reassembles root + diffs.
6. In-loop `reset()`: the in-memory, microsecond-budget rollback the fuzzer uses (dirty pages + registers, no disk/format), reset p50 ~36 µs.

End with a "See also" linking `[Snapshot & restore](../features/snapshot-restore.md)`, `[Diff / incremental snapshots](../features/diff-snapshots.md)`, `[How snapshot fuzzing works](../fuzzing/overview.md)`.

- [ ] **Step 4: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; all cross-links resolve.

- [ ] **Step 5: Commit**

```bash
git add docs/src/concepts
git commit -m "docs(book): concept chapters (architecture, device model, clone primitive)"
```

---

### Task 5: Feature chapters (snapshot-restore, diff-snapshots, devices)

**Files:**
- Create (via move): `docs/src/features/snapshot-restore.md` (from `docs/snapshot-restore-result.md`)
- Modify: `docs/src/features/diff-snapshots.md` (fold `docs/diff-snapshot-research.md` + README diff section)
- Modify: `docs/src/features/devices.md` (fold 4 result notes)
- Delete: `docs/virtio-net-result.md`, `docs/2e-virtio-result.md`, `docs/serial-rx-result.md`, `docs/smp-result.md`, `docs/diff-snapshot-research.md`

- [ ] **Step 1: snapshot-restore.md**

```bash
git rm docs/src/features/snapshot-restore.md
git mv docs/snapshot-restore-result.md docs/src/features/snapshot-restore.md
```
Edit the moved file: set H1 to `# Snapshot & restore`; tag shell fences `console`; if it opens with a dated "result" preamble, replace that with one evergreen sentence ("ignition snapshots a running guest and restores it lazily from an immutable base."). Add a top link back: `See [The clone primitive](../concepts/clone-primitive.md) for the mechanism.`

- [ ] **Step 2: diff-snapshots.md**

Fold two sources into the stub. Read `docs/diff-snapshot-research.md` and the README "Diff snapshots" section. Write `docs/src/features/diff-snapshots.md` with: H1 `# Diff / incremental snapshots`; how `--track-dirty` arms write-protect tracking (16 KiB pages, first-write trap → dirty → re-grant); the delta-chain model (`parent` = restored-from leaf, RAM-only deltified, vmstate/GIC/devices full per layer); restore reassembly; the `console` example from the README diff block (`--track-dirty` boot → restore → `python3 scripts/diff_snapshot_test.py`); and a link to `../examples/diff-snapshot-fanout.md` rendered as the absolute repo URL `https://github.com/vadika/ignition/blob/main/examples/diff-snapshot-fanout.md`. Then:
```bash
git rm docs/diff-snapshot-research.md
```

- [ ] **Step 3: devices.md (fold 4 → 1)**

Read `docs/virtio-net-result.md`, `docs/2e-virtio-result.md`, `docs/serial-rx-result.md`, `docs/smp-result.md`. Write `docs/src/features/devices.md` with H1 `# Devices, SMP & networking` and one `##` section per area: Console (16550 TX+RX, from serial-rx), virtio block/rng/balloon/vsock (from 2e-virtio), virtio-net + vmnet (from virtio-net-result: `--net`, sudo/entitlement, snapshot link-bounce + carrier-watch re-DHCP), SMP (PSCI `CPU_ON`, `--smp N`, from smp-result), PL031 RTC + boot-timer. Keep the concrete numbers/behaviors; drop dated "result" framing. Then:
```bash
git rm docs/virtio-net-result.md docs/2e-virtio-result.md docs/serial-rx-result.md docs/smp-result.md
```

- [ ] **Step 4: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; no broken links.

- [ ] **Step 5: Commit**

```bash
git add docs/src/features docs/snapshot-restore-result.md docs/virtio-net-result.md docs/2e-virtio-result.md docs/serial-rx-result.md docs/smp-result.md docs/diff-snapshot-research.md
git commit -m "docs(book): feature chapters (snapshot/restore, diff snapshots, devices)"
```

---

### Task 6: Fuzzing chapters

**Files:**
- Create (via move): `docs/src/fuzzing/overview.md` (from `docs/fuzzing-demonstrator-result.md`)
- Move: `docs/fuzzing-steps.png`, `docs/fuzzing-steps.svg` → `docs/src/fuzzing/`
- Modify: `docs/src/fuzzing/running.md` (new prose)

- [ ] **Step 1: overview.md + diagram**

```bash
git rm docs/src/fuzzing/overview.md
git mv docs/fuzzing-demonstrator-result.md docs/src/fuzzing/overview.md
git mv docs/fuzzing-steps.png docs/src/fuzzing/fuzzing-steps.png
git mv docs/fuzzing-steps.svg docs/src/fuzzing/fuzzing-steps.svg
```
Edit `overview.md`: H1 `# How snapshot fuzzing works`; embed the diagram near the top with `![Snapshot-fuzzing iteration](fuzzing-steps.png)`; tag fences; keep the methodology + numbers. Add a link to `[Running the fuzzer](running.md)`.

- [ ] **Step 2: running.md**

Replace the stub with operational content. MUST contain `console` blocks for: building+signing `boot`; the M1 gate `python3 scripts/fuzz_m1_test.py`; the M2 gate `python3 scripts/fuzz_m2_test.py`; the M3 benchmark `M3_DURATION=60 python3 scripts/fuzz_m3_bench.py`; and a `boot --fuzz` invocation with `--initramfs`, `--reset dirty|full`, `--seed`, `--metrics`. MUST explain `--reset` modes and what `--metrics` dumps (execs/sec, reset p50/p99 split page-copy vs register, dirty-set distribution, coverage curve, time-to-rediscover). Link guest-asset rebuild: `[Building guest assets](../getting-started/guest-assets.md)`.

- [ ] **Step 3: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; the image resolves (linkcheck checks image paths).

- [ ] **Step 4: Commit**

```bash
git add docs/src/fuzzing docs/fuzzing-demonstrator-result.md docs/fuzzing-steps.png docs/fuzzing-steps.svg
git commit -m "docs(book): fuzzing chapters (overview + diagram, running)"
```

---

### Task 7: Benchmark chapters

**Files:**
- Create (via move): `docs/src/benchmarks/boot-and-restore.md` (from `docs/benchmarks.md`)
- Create (via move): `docs/src/benchmarks/diff-snapshots.md` (from `docs/diff-snapshot-benchmarks.md`)
- Modify: `docs/src/benchmarks/fuzzing.md` (new prose, numbers from fuzzing overview)

- [ ] **Step 1: Move the two benchmark notes**

```bash
git rm docs/src/benchmarks/boot-and-restore.md docs/src/benchmarks/diff-snapshots.md
git mv docs/benchmarks.md docs/src/benchmarks/boot-and-restore.md
git mv docs/diff-snapshot-benchmarks.md docs/src/benchmarks/diff-snapshots.md
```
Edit both: ensure H1s are `# Boot & restore latency` and `# Diff-snapshot benchmarks`; tag shell fences `console`. Leave the tables/numbers intact.

- [ ] **Step 2: fuzzing.md**

Replace the stub. MUST contain a results table with the M3 numbers (from `docs/src/fuzzing/overview.md`): dirty-reset 1309 execs/sec vs full-copy 271 (4.8×), reset p50 36 µs (page-copy 35 µs / register restore 1 µs), dirty-set 44–50 pages, 144 edges, planted CVE rediscovered in 0.002 s. State the SanCov-only-libpng methodology and the dropped Linux/KVM cross-check. Add `Reproduce: `M3_DURATION=60 python3 scripts/fuzz_m3_bench.py`` and link `[How snapshot fuzzing works](../fuzzing/overview.md)`.

- [ ] **Step 3: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add docs/src/benchmarks docs/benchmarks.md docs/diff-snapshot-benchmarks.md
git commit -m "docs(book): benchmark chapters (boot/restore, diff snapshots, fuzzing)"
```

---

### Task 8: Internals chapters

**Files:**
- Create (via move): `docs/src/internals/hvf-firecracker-map.md` (from `docs/firecracker-hvf-porting-map.md`)
- Create (via move): `docs/src/internals/design-decisions.md` (from `docs/HANDOFF.md`)
- Modify: `docs/src/internals/validation-spike.md` (fold 4 notes)
- Delete: `docs/SPIKE_RESULTS.md`, `docs/2d-boot-result.md`, `docs/2f-findings.md`, `docs/phase1-followups.md`

- [ ] **Step 1: Move the map + handoff**

```bash
git rm docs/src/internals/hvf-firecracker-map.md docs/src/internals/design-decisions.md
git mv docs/firecracker-hvf-porting-map.md docs/src/internals/hvf-firecracker-map.md
git mv docs/HANDOFF.md docs/src/internals/design-decisions.md
```
Edit both: H1s `# HVF and Firecracker map` and `# Design decisions`; tag fences; fix any in-file relative links to other docs that moved (e.g. links to `*-result.md` → their new book paths; if unsure, leave the link text and point it at the new chapter).

- [ ] **Step 2: validation-spike.md (fold 4 → 1)**

Read `docs/SPIKE_RESULTS.md`, `docs/2d-boot-result.md`, `docs/2f-findings.md`, `docs/phase1-followups.md`. Write `docs/src/internals/validation-spike.md`: H1 `# Validation spike`; `##` sections for the spike result (SPIKE_RESULTS), first boot-to-shell (2d-boot-result), findings (2f-findings), and a short "phase-1 follow-ups (historical)" section keeping only still-relevant items from `phase1-followups.md` (drop completed/stale TODOs). Then:
```bash
git rm docs/SPIKE_RESULTS.md docs/2d-boot-result.md docs/2f-findings.md docs/phase1-followups.md
```

- [ ] **Step 3: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors; no broken in-book links.

- [ ] **Step 4: Commit**

```bash
git add docs/src/internals docs/firecracker-hvf-porting-map.md docs/HANDOFF.md docs/SPIKE_RESULTS.md docs/2d-boot-result.md docs/2f-findings.md docs/phase1-followups.md
git commit -m "docs(book): internals chapters (HVF map, design decisions, validation spike)"
```

---

### Task 9: Appendix + move examples to repo root

**Files:**
- Move: `docs/examples/` → `examples/` (repo root)
- Modify: `docs/src/appendix/specs-and-plans.md`
- Modify: `docs/src/appendix/examples.md`

- [ ] **Step 1: Move examples to the repo root**

```bash
git mv docs/examples examples
```
Then check `examples/fuzzing/run.sh` and `examples/fuzzing/README.md` for any path that assumed the `docs/examples/` location; update relative references to the new `examples/` root if present (grep: `grep -rn "docs/examples" examples/`).

- [ ] **Step 2: specs-and-plans.md**

Replace stub. MUST explain that `docs/superpowers/specs/` (design specs) and `docs/superpowers/plans/` (implementation plans) are kept in place as agentic reference (the format the subagent-driven workflow consumes), and link to them as repo URLs:
```markdown
- [Design specs](https://github.com/vadika/ignition/tree/main/docs/superpowers/specs)
- [Implementation plans](https://github.com/vadika/ignition/tree/main/docs/superpowers/plans)
```

- [ ] **Step 3: examples.md**

Replace stub. List the runnable examples with repo URLs:
```markdown
- [Diff-snapshot fan-out](https://github.com/vadika/ignition/blob/main/examples/diff-snapshot-fanout.md) — one warm base, many cheap forks.
- [Snapshot-fuzzing demo](https://github.com/vadika/ignition/tree/main/examples/fuzzing) — a runnable fork-server twin of the in-VMM fuzz loop.
```

- [ ] **Step 4: Build, link-check**

Run: `mdbook build docs/ 2>&1 | tee /tmp/mdbook.log`
Expected: no errors. (Repo-URL links are web links; `follow-web-links = false` so they are not fetched.)

- [ ] **Step 5: Commit**

```bash
git add docs/src/appendix examples docs/examples
git commit -m "docs(book): appendix pages; move examples/ to repo root"
```

---

### Task 10: Cross-link sweep + final verification

**Files:**
- Modify: any repo file still referencing a moved/removed doc path.

- [ ] **Step 1: Find stale references to moved/removed docs**

Run (lists every reference to an old filename now relocated):

```bash
grep -rn -E "REBUILD-GUEST-ASSETS\.md|snapshot-restore-result\.md|virtio-net-result\.md|2e-virtio-result\.md|serial-rx-result\.md|smp-result\.md|diff-snapshot-research\.md|fuzzing-demonstrator-result\.md|fuzzing-steps\.(png|svg)|benchmarks\.md|diff-snapshot-benchmarks\.md|firecracker-hvf-porting-map\.md|HANDOFF\.md|SPIKE_RESULTS\.md|2d-boot-result\.md|2f-findings\.md|phase1-followups\.md|docs/examples" \
  --include=*.md --include=*.rs --include=*.sh --include=*.py . \
  | grep -v "docs/superpowers/" | grep -v "docs/src/"
```
Expected: ideally empty. `docs/superpowers/` (specs/plans) and `docs/src/` (the book itself, which legitimately references its own moved files) are excluded. For each remaining hit (e.g. in `ROADMAP.md`, `README.md`, a script comment), update the path to the new book location or the repo URL. Re-run until the filtered output is empty.

- [ ] **Step 2: Confirm every spec §6 destination exists and every folded original is gone**

```bash
test -f docs/src/introduction.md && \
test -f docs/src/getting-started/guest-assets.md && \
test -f docs/src/features/devices.md && \
test -f docs/src/internals/validation-spike.md && \
test -d examples/fuzzing && \
! test -e docs/HANDOFF.md && ! test -e docs/benchmarks.md && \
! test -e docs/virtio-net-result.md && ! test -e docs/2f-findings.md && \
! test -e docs/examples && echo "LAYOUT OK"
```
Expected: prints `LAYOUT OK`.

- [ ] **Step 3: Full clean build + link-check**

```bash
rm -rf docs/book
mdbook build docs/ 2>&1 | tee /tmp/mdbook.log
grep -iE "error|warn|broken" /tmp/mdbook.log || echo "BUILD CLEAN"
```
Expected: `BUILD CLEAN`; `docs/book/html/index.html` exists; every chapter in `SUMMARY.md` rendered.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs(book): fix cross-links after the doc reorganization"
```

---

## Self-Review

**Spec coverage:**
- §3 approach (mdBook at docs/, src/, book/ gitignored) → Task 1. ✓
- §4 layout incl. root-file handling (README slim, ROADMAP untouched, REBUILD pointer) → Tasks 1–3. ✓
- §5 SUMMARY.md → Task 1 Step 3 (verbatim). ✓
- §6 mapping (every source→dest, folds, removals) → Tasks 2,5,6,7,8,9; the removals list matches §6's "folded-away originals." ✓
- §7 book.toml → Task 1 Step 1 (verbatim); CI → Task 1 Step 5; gitignore → Task 1 Step 4; quality gates (build, linkcheck, fence tags, abs URLs, manual table walk) → per-task builds + Task 10. ✓
- §8 risks (cross-link rot → Task 10 sweep; README drift → Task 3; Pages prereq → noted in plan header / spec, manual). ✓

**Placeholder scan:** No "TBD/TODO" in steps. Stub pages created in Task 1 carry a real H1 + sentence (intentional scaffolding the spec calls for), and each is fully replaced by a named later task. New-prose chapters specify required content + must-contain facts + verification rather than final prose, which is appropriate for documentation pages.

**Consistency:** File paths in `SUMMARY.md` (Task 1) match the `git mv`/create targets in Tasks 2–9 exactly (e.g. `internals/hvf-firecracker-map.md`, `features/devices.md`). The removal list in Task 5/8 matches §6. The linkcheck HTML path (`book/html`) used in the CI upload (Task 1 Step 5) matches `mdbook-linkcheck`'s nested output, and Task 10 verifies `docs/book/html/index.html`.

**One-time manual prereq (not a task):** repo owner sets GitHub Pages source = "GitHub Actions" (Settings → Pages). The `docs.yml` workflow is inert until then; local builds and all tasks above do not depend on it.
```
