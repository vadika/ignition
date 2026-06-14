# Introduction

ignition is a research microVM for macOS on Apple Silicon, built on Apple's
Hypervisor.framework (HVF). It is architecturally modeled on AWS Firecracker, the
microVM model, the vstate seam, the device set, but it is not a port: it shares
roughly zero lines of Firecracker source. The lineage is the design plus the
rust-vmm building blocks Firecracker also uses. The one genuinely lifted piece is the
`hvf` crate, taken from [libkrun](https://github.com/containers/libkrun) and then
substantially reworked here.

## The differentiator

The macOS microVM space is already contested by Virtualization.framework based tools,
so "isolated Linux microVM on a Mac" is not, by itself, a reason to exist. The
differentiator is the fast snapshot plus clone-from-warm-base primitive on bare HVF:
`clonefile` plus `MAP_SHARED` against an immutable base, where a clone idles at about
0% CPU and touches only its own dirtied pages. This is the Firecracker production
pattern. Virtualization.framework based tools cannot expose it cleanly, because they
sit on a closed whole-VM checkpoint API. ignition runs on raw HVF, so it can.

## Positioning

ignition is a substrate that other tools are built on, not an end-user product. Its
intended consumers are tool-builders: agent-sandbox authors, fuzzing harnesses, and CI
backends, not Mac users at large. Everything is organized around making the clone
primitive provably fast and correct, and reachable from infrastructure developers
already run.

## Two tracks

Two tracks carry the project beyond Firecracker parity:

- **Demonstrator** (snapshot fuzzing). The cleanest proof the clone primitive does
  real work: execs/sec is a direct function of reset latency, and a fuzz loop is the
  most brutal correctness test the snapshot path will ever face.
- **Adoption** (integration). Impersonate interfaces that already have consumers, MCP,
  the Firecracker REST API, and OCI, so adoption cost is near zero. One faithful seam
  at a time.

## Where to go next

- [Build & run](getting-started/build-and-run.md), get a guest booting.
- [The clone primitive](concepts/clone-primitive.md), the core idea.
- [How snapshot fuzzing works](fuzzing/overview.md), the demonstrator.
- [Roadmap](https://github.com/vadika/ignition/blob/main/ROADMAP.md), what is built and
  what is next.
