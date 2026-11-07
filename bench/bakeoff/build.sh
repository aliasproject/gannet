#!/bin/bash
# Builds the bake-off probes into .bench/bakeoff.
set -euo pipefail
cd "$(dirname "$0")"
OUT=$PWD/../../.bench/bakeoff
mkdir -p "$OUT"
g++ -O2 -std=c++17 -fPIC qtwidgets/main.cpp -o "$OUT/qtwidgets" $(pkg-config --cflags --libs Qt6Widgets)
g++ -O2 -std=c++17 -fPIC qtquick/main.cpp -o "$OUT/qtquick" $(pkg-config --cflags --libs Qt6Quick Qt6Qml Qt6Gui)
gcc -O2 gtk4/main.c -o "$OUT/gtk4" $(pkg-config --cflags --libs gtk4) -w
(cd rawshm && CARGO_TARGET_DIR=../../../target/bakeoff cargo build --release)
cp ../../target/bakeoff/release/rawshm "$OUT/"
