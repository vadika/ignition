#!/usr/bin/env bash
# One-time setup for sudo-free guest networking: install the socket_vmnet daemon
# (lima-vm/socket_vmnet) via Homebrew and start its root LaunchDaemon. After this,
# `boot --net` connects to the daemon socket and needs no sudo.
set -euo pipefail

if ! command -v brew >/dev/null 2>&1; then
  echo "Homebrew not found. Install it from https://brew.sh first." >&2
  exit 1
fi

brew install socket_vmnet
# The daemon must run as root (vmnet shared mode); brew services + sudo installs
# the LaunchDaemon homebrew.mxcl.socket_vmnet.
sudo "$(brew --prefix)/bin/brew" services start socket_vmnet

sock="$(brew --prefix)/var/run/socket_vmnet"
echo "socket_vmnet socket path: $sock"
if [ -S "$sock" ]; then
  echo "ready: boot --net will use it (override with --net-socket or IGN_VMNET_SOCKET)."
else
  echo "socket not present yet; give it a moment, then check: sudo brew services list" >&2
fi
