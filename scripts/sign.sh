#!/usr/bin/env bash
# Ad-hoc codesign a built binary with the hypervisor entitlement.
# Required after every build: without it, hv_vm_create returns HV_DENIED.
set -euo pipefail
BIN="${1:?usage: scripts/sign.sh <path-to-binary>}"
ENT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/ignition.entitlements"
codesign --force --sign - --entitlements "$ENT" "$BIN"
echo "signed: $BIN"
