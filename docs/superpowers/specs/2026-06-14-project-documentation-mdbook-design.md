# Project documentation (mdBook) — design

Date: 2026-06-14. Status: approved for planning.

## 1. Goal

Rework the scattered development notes under `docs/` into a single, coherent,
publishable documentation site that serves both audiences at once:

- **Tool-builders** (the thesis audience: agent-sandbox / fuzzing / CI authors) — what
  ignition is, how the clone primitive works, how to use `boot`.
- **Contributors** — architecture, the HVF↔Firecracker mapping, design decisions.

The site is built with **mdBook**. Existing dated notes are preserved (moved into the
book as primary-source pages, history kept via `git mv`), not deleted. The agentic
reference material under `docs/superpowers/` (26 specs + 32 plans) stays exactly where it
is. Runnable examples move to a top-level `examples/`.

## 2. Non-goals (YAGNI)

- Custom theme/CSS, versioned docs, external search plugins, PDF export.
- Rewriting or relocating `docs/superpowers/` specs/plans (they are agentic reference;
  left in place, linked as an appendix).
- Changing any code, scripts, or behavior. This is documentation only.
- A second generator (MkDocs etc.). mdBook only.

## 3. Approach

mdBook rooted at `docs/`, sources under `docs/src/`, output to `docs/book/` (gitignored).
Curated evergreen chapters (introduction, getting-started, concepts, features, fuzzing,
benchmarks, internals) are indexed by `docs/src/SUMMARY.md`. Most chapters are existing
notes moved under `src/` and lightly rewritten for an evergreen voice; three concept
chapters are written fresh. `superpowers/` and `examples/` live outside the book tree and
are linked from appendix pages via absolute GitHub URLs.

## 4. Target layout

```
docs/
  book.toml
  src/
    SUMMARY.md
    introduction.md
    getting-started/
      build-and-run.md
      boot-a-guest.md
      guest-assets.md
    concepts/
      architecture.md
      device-model.md
      clone-primitive.md
    features/
      snapshot-restore.md
      diff-snapshots.md
      devices.md
    fuzzing/
      overview.md
      running.md
      fuzzing-steps.png
      fuzzing-steps.svg
    benchmarks/
      boot-and-restore.md
      diff-snapshots.md
      fuzzing.md
    internals/
      hvf-firecracker-map.md
      design-decisions.md
      validation-spike.md
    appendix/
      specs-and-plans.md
      examples.md
  superpowers/                 # UNCHANGED
  book/                        # generated, gitignored

examples/                      # MOVED from docs/examples/
  diff-snapshot-fanout.md
  fuzzing/  (target.c, harness.c, repro.c, run.sh, README.md, .gitignore)
```

Root files:
- `README.md` — stays the GitHub landing. Trimmed to: project intro, quickstart
  (build + sign + boot), and a prominent "full documentation → the book" link. Deep
  sections now living in the book (detailed snapshot/restore, diff-snapshots, fuzzing
  walkthroughs) are removed from README to prevent drift.
- `ROADMAP.md` — unchanged, stays at root (living index). `introduction.md` links to it;
  no duplication into the book.
- `REBUILD-GUEST-ASSETS.md` — content moves to `getting-started/guest-assets.md`. The
  root file is replaced with a one-line pointer to that chapter.

## 5. `SUMMARY.md`

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

(`SUMMARY.md` uses an em-dash-free title for the HVF/Firecracker page; rendered nav text
matches the link labels above.)

## 6. Source-to-chapter mapping

Every existing note has a destination; nothing is orphaned. Moves use `git mv` (history
preserved); folds copy content from multiple sources into one chapter, then the now-empty
originals are removed.

| Existing | Destination | Action |
|---|---|---|
| README intro + ROADMAP thesis | `introduction.md` | new prose, sourced |
| README build/run | `getting-started/build-and-run.md` | extract |
| README boot section | `getting-started/boot-a-guest.md` | extract |
| `REBUILD-GUEST-ASSETS.md` | `getting-started/guest-assets.md` | move; root → pointer |
| `snapshot-restore-result.md` | `features/snapshot-restore.md` | move + light rewrite |
| `diff-snapshot-research.md` + README diff | `features/diff-snapshots.md` | fold |
| `virtio-net-result.md`, `2e-virtio-result.md`, `serial-rx-result.md`, `smp-result.md` | `features/devices.md` | fold (4→1) |
| `fuzzing-demonstrator-result.md`, `fuzzing-steps.{png,svg}` | `fuzzing/overview.md` | move + assets |
| (fuzz gate/bench commands from README + scripts) | `fuzzing/running.md` | new prose |
| `benchmarks.md` | `benchmarks/boot-and-restore.md` | move |
| `diff-snapshot-benchmarks.md` | `benchmarks/diff-snapshots.md` | move |
| (fuzzing numbers from demonstrator result) | `benchmarks/fuzzing.md` | extract |
| `firecracker-hvf-porting-map.md` | `internals/hvf-firecracker-map.md` | move |
| `HANDOFF.md` | `internals/design-decisions.md` | move + light rewrite |
| `SPIKE_RESULTS.md`, `2d-boot-result.md`, `2f-findings.md`, `phase1-followups.md` | `internals/validation-spike.md` | fold (4→1) |
| `docs/superpowers/` | `appendix/specs-and-plans.md` | link only; files unchanged |
| `docs/examples/` | `examples/` (root) + `appendix/examples.md` | move dir + link page |

New-prose chapters with no single source (written from README layout + HANDOFF +
porting-map): `concepts/architecture.md`, `concepts/device-model.md`,
`concepts/clone-primitive.md`.

Folded-away originals removed after their content lands in the destination:
`virtio-net-result.md`, `2e-virtio-result.md`, `serial-rx-result.md`, `smp-result.md`,
`diff-snapshot-research.md`, `SPIKE_RESULTS.md`, `2d-boot-result.md`, `2f-findings.md`,
`phase1-followups.md`. (`phase1-followups.md` is stale phase-1 TODOs; its still-relevant
items fold into `validation-spike.md`, the rest are dropped.)

## 7. Build, deploy, testing

**`book.toml`:**
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

**Local:** `cargo install mdbook mdbook-linkcheck`; `mdbook build docs/`; `mdbook serve docs/`.

**Deploy (GitHub Pages via Actions):** `.github/workflows/docs.yml` — trigger on push to
`main` touching `docs/`; install mdbook + mdbook-linkcheck; `mdbook build docs/`; upload
`docs/book/` and deploy with `actions/upload-pages-artifact` + `actions/deploy-pages`
(permissions: `pages: write`, `id-token: write`). One-time manual prereq (repo owner):
Settings → Pages → Source = "GitHub Actions". CI is inert until that is set.

**`.gitignore`:** add `docs/book/`.

**Quality gates:**
- `mdbook build docs/` succeeds with no warnings (missing files / broken `SUMMARY` fail).
- `mdbook-linkcheck` passes: every internal link + image resolves.
- Code fences are tagged `console`, `text`, `c`, or `rust,ignore` so `mdbook test`'s Rust
  doctest runner skips shell/C blocks.
- Links to `superpowers/` and `examples/` use absolute GitHub URLs (linkcheck has
  `follow-web-links = false`, so they are not fetched but resolve on the live site).
- Manual: walk the §6 table; every listed destination exists and every folded-away
  original is removed.

## 8. Risks

- **Cross-link rot.** Moving files breaks existing relative links (e.g. ROADMAP and
  README point at `docs/*-result.md`). Mitigation: after the moves, grep the repo for the
  old filenames and update every reference; linkcheck covers in-book links.
- **README/book drift.** Mitigation: README keeps only quickstart + a book link; deep
  content lives once, in the book.
- **Pages not enabled.** CI deploy step no-ops/fails until the owner flips the Pages
  source. Documented as a manual prereq; not blocking for local builds.
