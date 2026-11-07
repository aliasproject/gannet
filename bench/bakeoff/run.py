#!/usr/bin/env python3
"""Stack bake-off: time to mapped window, PSS and CPU for one probe per toolkit,
each holding a 100,000-row list in a 1200x760 window, all in the same headless sway.
Everything renders in software here, so GPU backends aren't represented."""
import subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "harness"))
from hl import *
ROOT = Path(__file__).resolve().parent.parent.parent
B = ROOT / ".bench/bakeoff"
PROBES = [("raw wl_shm+swash", [B / "rawshm"], {}),
          ("Qt Widgets", [B / "qtwidgets"], {"QT_QPA_PLATFORM": "wayland"}),
          ("Qt Quick (software)", [B / "qtquick"], {"QT_QPA_PLATFORM": "wayland", "QT_QUICK_BACKEND": "software"}),
          ("GTK4 (cairo)", [B / "gtk4"], {"GSK_RENDERER": "cairo", "GDK_BACKEND": "wayland"})]
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 7
h = Headless("/tmp/gnbake")
try:
    print(f"{'probe':24s} {'first window ms':>16s} {'PSS MiB':>9s} {'CPU s':>7s}")
    for name, argv, extra in PROBES:
        t, p, c = [], [], []
        for run in range(RUNS):
            env = h.env("/tmp/gnbake-client", extra)
            start = time.monotonic()
            proc = subprocess.Popen([str(a) for a in argv], env=env, stdout=subprocess.DEVNULL, stderr=open("/tmp/gnbake-client.err", "w"))
            t.append((h.wait_window() - start) * 1000)
            time.sleep(1.0)
            p.append(pss_kib(proc.pid) / 1024)
            c.append(cpu_seconds(proc.pid))
            proc.terminate(); proc.wait(5)
            time.sleep(0.1)
        print(f"{name:24s} {median(t):16.1f} {median(p):9.1f} {median(c):7.2f}   runs {min(t):.0f}..{max(t):.0f} ms", flush=True)
finally:
    h.stop()
