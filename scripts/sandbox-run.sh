#!/bin/bash
# Runs one program with the real home hidden and a scratch /tmp. See scripts/test.sh.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$(realpath "$1"); shift
SCRATCH=$(mktemp -d "$ROOT/target/test/sandbox.XXXXXX")
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -p "$SCRATCH/home" "$SCRATCH/tmp"
bwrap --die-with-parent --unshare-pid --unshare-net --ro-bind / / --dev /dev --proc /proc --tmpfs /run --tmpfs /run/gannet-tree/mnt --tmpfs "$HOME" \
  --bind "$SCRATCH/home" "$SCRATCH/home" --bind "$SCRATCH/tmp" /tmp \
  --ro-bind "$BIN" "$BIN" --ro-bind "$ROOT" "$ROOT" --bind "$SCRATCH" "$SCRATCH" \
  --chdir "$ROOT" \
  --setenv GANNET_TEST_SANDBOX 1 --setenv HOME "$SCRATCH/home" --setenv TMPDIR /tmp \
  --setenv XDG_DATA_HOME "$SCRATCH/home/.local/share" --setenv XDG_CONFIG_HOME "$SCRATCH/home/.config" \
  --setenv XDG_STATE_HOME "$SCRATCH/home/.local/state" --setenv XDG_CACHE_HOME "$SCRATCH/home/.cache" \
  --unsetenv XDG_RUNTIME_DIR --unsetenv WAYLAND_DISPLAY --unsetenv DBUS_SESSION_BUS_ADDRESS \
  "$BIN" "$@"
