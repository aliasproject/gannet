# Benchmarks and the headless harness

Graphical checks run in `harness/headless.sh`: a private sway on the pixman renderer,
in bubblewrap with an empty `/dev`. It has no GPU, DRM or input device, and no link to
the live session. `harness/fetch-tools.sh` unpacks sway into `.tools/` (git-ignored);
nothing is installed on the system.

Headless Hyprland was tried first and won't start here. Its backend needs a usable
DRM device, and giving it one means touching the desktop's GPU.

## Stack bake-off

    bench/harness/fetch-tools.sh
    bench/bakeoff/build.sh
    python3 -B bench/bakeoff/run.py 7

Each probe holds a 100,000-row list in a 1200x760 window. "First window" is process
spawn to sway reporting the mapped window, which happens on the first committed buffer.

## Budgets

    cargo build --release -p gannet
    CARGO_TARGET_DIR=target/bench cargo build --release -p gannet --features bench
    CARGO_TARGET_DIR=target/bench cargo build --release -p gannet-core --example copybench --example trashbench --example udisks-standin
    (cd bench/harness/vptr && CARGO_TARGET_DIR=../../../target/vptr cargo build --release)
    python3 -B bench/run.py

Reads only: the fixture lives in `.bench/`, the app runs with HOME and XDG folders in a
scratch root, and Nautilus runs in bubblewrap with the real home hidden. The one thing it
writes outside `.bench/` is a temporary folder for the copy test on `/tmp`. The Trash test
uses a Trash folder inside `.bench/`, not the real one.
