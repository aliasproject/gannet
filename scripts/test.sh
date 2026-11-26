#!/bin/bash
# Runs `cargo test` with each test binary inside bubblewrap: the real home is hidden
# behind an empty tmpfs and only a scratch folder is writable, so a bad test can't
# reach real files, the real Trash, or the real config. /run is a tmpfs there too, so a test
# can't reach the desktop's session bus or accessibility bus. Doc-tests and build scripts run
# outside it, so the sockets are checked after the run, and the run fails loudly if one changed.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=$PWD/target/test
arch=$(uname -m | tr a-z A-Z)
export "CARGO_TARGET_${arch}_UNKNOWN_LINUX_GNU_RUNNER=$PWD/scripts/sandbox-run.sh"
# The disk tests run the stand-in udisks2 beside them. A filtered run doesn't build examples,
# so it's built here, or a filtered run would use an old one.
cargo build -q -p gannet-core --example udisks-standin
# Since cargo 1.100 the test binaries no longer sit beside the examples, so say where it is.
export GANNET_UDISKS_STANDIN=$CARGO_TARGET_DIR/debug/examples/udisks-standin
# Each bus socket in the desktop's runtime folder, with its inode and times.
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
sockets() { stat -c '%n %i %Y %Z' "$runtime/bus" "$runtime/at-spi" "$runtime"/at-spi/* 2>/dev/null || true; }
before=$(sockets)
code=0
# Not exec'd, so the sockets can be checked after it. Ctrl-C still reaches cargo and the tests,
# which are in the foreground; this script only waits for them to stop, then checks.
trap : INT TERM HUP
cargo test "$@" || code=$?
trap - INT TERM HUP
after=$(sockets)
if [ "$before" != "$after" ]; then
  bar=$(printf '!%.0s' {1..78})
  printf '\n%s\nTHE DESKTOP'"'"'S BUS SOCKETS CHANGED DURING THIS RUN:\n' "$bar" >&2
  diff <(echo "$before") <(echo "$after") >&2 || true
  printf 'Unless the desktop itself started or restarted a bus meanwhile, a test reached the real\nruntime folder. If bus_0 was replaced, a screen reader will fail to connect until the bus\nis restarted; see the README'"'"'s Development section.\n%s\n' "$bar" >&2
  [ "$code" = 0 ] && code=3
fi
exit "$code"
