#!/bin/bash
# Runs inside an archlinux container in CI (see .github/workflows/ci.yml). As root it installs
# what Gannet needs, then runs the step again as an ordinary user with the checkout's uid.
# Not as root: root can read folders the permission tests expect to be unreadable.
#   scripts/ci.sh test     the unit tests, through scripts/test.sh (bubblewrap)
#   scripts/ci.sh clippy   clippy, failing only on more warnings than CLIPPY_BASELINE
set -euo pipefail
cd "$(dirname "$0")/.."
step=$1

if [ "$(id -u)" = 0 ]; then
  pacman -Syu --noconfirm --needed rust gcc pkgconf libxkbcommon dbus bubblewrap shared-mime-info
  useradd -m -u "$(stat -c %u .)" ci
  exec setpriv --reuid=ci --regid=ci --init-groups \
    env HOME=/home/ci USER=ci CARGO_HOME="$PWD/.ci/cargo" CLIPPY_BASELINE="${CLIPPY_BASELINE:-0}" "$0" "$step"
fi

case $step in
  test)
    scripts/test.sh --no-fail-fast
    ;;
  clippy)
    out=$(cargo clippy --workspace --all-targets --message-format=short 2>&1) || { echo "$out"; exit 1; }
    echo "$out"
    # One line per warning; a warning in the bin shows again in its test build, so dedupe.
    warnings=$(grep -E '^[^ ]+:[0-9]+:[0-9]+: warning: ' <<<"$out" | sort -u || true)
    count=$(grep -c . <<<"$warnings" || true)
    echo "clippy: $count warning(s), baseline $CLIPPY_BASELINE"
    if [ "$count" -gt "$CLIPPY_BASELINE" ]; then
      echo "::error::clippy has $count warnings, more than the $CLIPPY_BASELINE already there. Fix the new ones."
      exit 1
    elif [ "$count" -lt "$CLIPPY_BASELINE" ]; then
      echo "::notice::clippy has $count warnings, fewer than the baseline of $CLIPPY_BASELINE. Lower CLIPPY_BASELINE in .github/workflows/ci.yml."
    fi
    ;;
  *) echo "usage: $0 test|clippy" >&2; exit 2 ;;
esac
