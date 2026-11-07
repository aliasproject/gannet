#!/bin/bash
# Unpacks sway and wlroots into .tools/root for the headless harness. Installs
# nothing on the system: packages come from the mirror or pacman's cache and stay
# inside this checkout (.tools is git-ignored).
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p .tools/pkgs .tools/root
for p in sway wlroots0.20 libliftoff; do
  url=$(pacman -Sp --print-format '%l' "$p" 2>/dev/null | tail -1)
  case "$url" in
    file://*) cp "${url#file://}" .tools/pkgs/ ; f=.tools/pkgs/$(basename "$url") ;;
    *) f=.tools/pkgs/$(basename "$url"); curl -fsSL -o "$f" "$url" ;;
  esac
  tar -C .tools/root --zstd -xf "$f" 2>/dev/null
done
