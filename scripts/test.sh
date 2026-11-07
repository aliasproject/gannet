#!/bin/bash
# Runs `cargo test` with each test binary inside bubblewrap: the real home is hidden
# behind an empty tmpfs and only a scratch folder is writable, so a bad test can't
# reach real files, the real Trash, or the real config.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=$PWD/target/test
arch=$(uname -m | tr a-z A-Z)
export "CARGO_TARGET_${arch}_UNKNOWN_LINUX_GNU_RUNNER=$PWD/scripts/sandbox-run.sh"
exec cargo test "$@"
