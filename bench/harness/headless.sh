#!/bin/bash
# Starts a private headless sway inside a bubblewrap sandbox with an empty /dev:
# pixman renderer, headless backend, no GPU, no DRM, no input devices, no link to
# the live session. Blocks until killed. Clients connect with
#   XDG_RUNTIME_DIR=$ROOT/runtime WAYLAND_DISPLAY=wayland-1
# Usage: headless.sh <absolute /tmp/gn* root> [WIDTHxHEIGHT]
set -euo pipefail
ROOT=${1:?usage: headless.sh <root> [WxH]}
SIZE=${2:-1280x800}
case "$ROOT" in /tmp/gn*) ;; *) echo "root must be /tmp/gn* (Unix socket paths are short)" >&2; exit 2;; esac
TOOLS=$(cd "$(dirname "$0")/../.." && pwd)/.tools/root
[ -x "$TOOLS/usr/bin/sway" ] || { echo "run bench/harness/fetch-tools.sh first" >&2; exit 2; }
mkdir -p "$ROOT"/{home,cache,config,state,data,runtime}
chmod 700 "$ROOT/runtime"
cat > "$ROOT/sway.conf" <<CONF
output HEADLESS-1 resolution $SIZE scale 1
default_border none
default_floating_border none
font pango:monospace 1
# Gannet's preview window floats over the list, as aliasOS's window rules float it.
for_window [app_id="com.aliasproject.Gannet.Preview"] floating enable, resize set 960 720, move position center
CONF
exec env -i PATH=/usr/bin HOME="$ROOT/home" XDG_CACHE_HOME="$ROOT/cache" XDG_CONFIG_HOME="$ROOT/config" \
  XDG_STATE_HOME="$ROOT/state" XDG_DATA_HOME="$ROOT/data" XDG_RUNTIME_DIR="$ROOT/runtime" \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent LD_LIBRARY_PATH="$TOOLS/usr/lib" \
  WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1 LIBSEAT_BACKEND=noop \
  bwrap --die-with-parent --ro-bind / / --dev /dev --proc /proc --tmpfs /run --bind "$ROOT" "$ROOT" --unshare-pid \
  "$TOOLS/usr/bin/sway" --unsupported-gpu -c "$ROOT/sway.conf"
