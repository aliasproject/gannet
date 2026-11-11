#!/usr/bin/env python3
"""Measure Gannet against its performance budgets, in a headless compositor.

  python3 -B bench/run.py [--runs N] [--no-nautilus]

Prints every budget and exits non-zero if any is missed. The fixture is 100,000 empty
files on the project's own disk (btrfs on Mike's desktop). Times are warm page cache,
medians. Everything renders in software in sway's pixman backend, with no GPU; the time
a real compositor spends uploading the buffer isn't included.
"""
import argparse, json, os, re, shutil, struct, subprocess, sys, time, zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "bench/harness"))
from hl import Headless, cpu_seconds, median, pss_kib, tree

PLAIN = ROOT / "target/release/gannet"
BENCH = ROOT / "target/bench/release/gannet"
DATA = ROOT / ".bench"
BIG = DATA / "fixture-100k"
SMALL = DATA / "fixture-small"

# (metric, limit, unit)
BUDGETS = [
    ("first window, small folder", 25, "ms"),
    ("launch to sorted rows, 100k", 150, "ms"),
    ("open 100k from inside the app", 100, "ms"),
    ("PSS holding 100k, 1280x800", 30, "MiB"),
    ("idle CPU over 10 s", 0, "ticks"),
    ("idle wakeups", 1, "per s"),
    ("paint scrolling 100k p99, 1280x800", 4, "ms"),
    ("paint scrolling 100k p99, 3840x2160", 8.3, "ms"),
    ("longest UI stall, slow folder", 16, "ms"),
    ("theme change to new frame", 100, "ms"),
    ("copy 1 GiB across disks, vs cp", 1.10, "x cp"),
    ("move 5,000 files to Trash", 1000, "ms"),
    ("Space to the preview window", 25, "ms"),
    ("Space to a 1600x1200 photo in the preview", 100, "ms"),
    ("Space to 128 kB of text in the preview", 100, "ms"),
    ("Space to 128 kB of Markdown in the preview", 150, "ms"),
    ("Space to the first page of a PDF in the preview", 200, "ms"),
]


def fixture():
    marker = BIG / ".gannet-bench-fixture"
    if not marker.exists():
        assert str(BIG).startswith(str(ROOT)), BIG
        BIG.mkdir(parents=True, exist_ok=True)
        print("building the 100,000-file fixture…", flush=True)
        for i in range(100_000):
            (BIG / f"file-{i:06}.txt").touch()
        marker.touch()
    SMALL.mkdir(parents=True, exist_ok=True)
    for i in range(30):
        (SMALL / f"file-{i}.txt").touch()
    # Warm the page cache for everyone equally.
    for _ in os.scandir(BIG):
        pass


def env_for(h, name, extra=None):
    return h.env(f"/tmp/gnbench-client/{name}", {"XDG_SESSION_DESKTOP": "bench", **(extra or {})})


def settle(pid, quiet=0.5, limit=120):
    """Waits for the process tree's CPU use to stop for `quiet` seconds."""
    start = time.monotonic()
    last_cpu, last_change = cpu_seconds(pid), time.monotonic()
    while time.monotonic() - start < limit:
        time.sleep(0.02)
        c = cpu_seconds(pid)
        if c != last_cpu:
            last_cpu, last_change = c, time.monotonic()
        elif time.monotonic() - last_change >= quiet:
            return last_change - start
    return limit


def stop(p):
    p.terminate()
    try:
        p.wait(5)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()


def first_window(h, argv, env):
    t0 = time.monotonic()
    p = subprocess.Popen(argv, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return p, (h.wait_window(30) - t0) * 1000


def read_log(path):
    return path.read_text().splitlines() if path.exists() else []


def wait_log(path, prefix, timeout=30, after=0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        hits = [l for l in read_log(path) if l.startswith(prefix)]
        if len(hits) > after:
            return hits[after].split()
        time.sleep(0.002)
    raise AssertionError(f"no {prefix} in {path}")


def key(env, *keys, mods=()):
    cmd = ["wtype"]
    for m in mods:
        cmd += ["-M", m]
    for k in keys:
        cmd += ["-k", k]
    for m in mods:
        cmd += ["-m", m]
    subprocess.run(cmd, env=env, check=True, capture_output=True)


def log_env(h, name, extra=None):
    log = Path(f"/tmp/gnbench-client/{name}.log")
    log.parent.mkdir(parents=True, exist_ok=True)
    log.unlink(missing_ok=True)
    return log, env_for(h, name, {"GANNET_BENCH_LOG": str(log), **(extra or {})})


def p99(values):
    v = sorted(values)
    return v[min(len(v) - 1, int(len(v) * 0.99))]


def measure(runs, results, h):
    # First window
    v = []
    for _ in range(runs * 2):
        p, t = first_window(h, [str(PLAIN), str(SMALL)], env_for(h, "plain"))
        v.append(t)
        stop(p)
    results["first window, small folder"] = median(v)
    # Launch to sorted rows
    v = []
    for _ in range(runs):
        log, env = log_env(h, "rows")
        t0 = time.monotonic() * 1000
        p = subprocess.Popen([str(BENCH), str(BIG)], env=env, stderr=subprocess.DEVNULL)
        h.wait_window(30)
        f = wait_log(log, "rows")
        assert f[1] == "100000", f
        v.append(float(f[2]) - t0)
        stop(p)
    results["launch to sorted rows, 100k"] = median(v)
    # Open from inside: reported by the app, from starting the load to rows on screen.
    v = []
    for _ in range(runs):
        log, env = log_env(h, "inside")
        p = subprocess.Popen([str(BENCH), str(DATA)], env=env, stderr=subprocess.DEVNULL)
        h.wait_window(30)
        wait_log(log, "rows")
        time.sleep(0.1)
        # Type-ahead lands on the fixture, and Return opens it.
        subprocess.run(["wtype", "-d", "20", "--", "fixture-1"], env=env, check=True)
        time.sleep(0.05)
        key(env, "Return")
        f = wait_log(log, "rows 100000")
        v.append(float(f[3]))
        stop(p)
    results["open 100k from inside the app"] = median(v)
    # Memory and idle
    p, _ = first_window(h, [str(PLAIN), str(BIG)], env_for(h, "plain"))
    settle(p.pid)
    time.sleep(0.5)
    results["PSS holding 100k, 1280x800"] = pss_kib(p.pid) / 1024
    results["_rss"] = int(next(l for l in open(f"/proc/{p.pid}/status") if l.startswith("VmRSS")).split()[1]) / 1024

    def switches(pid):
        total = 0
        for t in os.listdir(f"/proc/{pid}/task"):
            for l in open(f"/proc/{pid}/task/{t}/status"):
                if "ctxt_switches" in l:
                    total += int(l.split()[1])
        return total

    def ticks(pid):
        f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return int(f[11]) + int(f[12])

    time.sleep(1)
    c0, s0 = ticks(p.pid), switches(p.pid)
    time.sleep(10)
    results["idle CPU over 10 s"] = ticks(p.pid) - c0
    results["idle wakeups"] = (switches(p.pid) - s0) / 10
    stop(p)


def scroll_paints(h, name):
    log, env = log_env(h, name)
    p = subprocess.Popen([str(BENCH), str(BIG)], env=env, stderr=subprocess.DEVNULL)
    h.wait_window(30)
    wait_log(log, "rows")
    time.sleep(0.3)
    start = len(read_log(log))
    for _ in range(6):
        key(env, *(["Next"] * 60))
    time.sleep(0.5)
    paints = [int(l.split()[1]) / 1000 for l in read_log(log)[start:] if l.startswith("paint")]
    assert len(paints) > 30, f"only {len(paints)} paints"
    stop(p)
    return paints


def measure_stall(h, results):
    log, env = log_env(h, "slow", {"GANNET_SLOW_READ_MS": "2"})
    p = subprocess.Popen([str(BENCH), str(BIG)], env=env, stderr=subprocess.DEVNULL)
    h.wait_window(30)
    # Keep the UI busy while the folder arrives in pieces.
    for _ in range(40):
        key(env, "Down")
        time.sleep(0.02)
    wait_log(log, "rows", timeout=120)
    busy = [int(l.split()[1]) / 1000 for l in read_log(log) if l.startswith("busy")]
    results["longest UI stall, slow folder"] = max(busy) if busy else 0
    results["_stall_count"] = len(busy)
    stop(p)


def measure_theme(h, results, runs):
    log, env = log_env(h, "theme")
    cur = Path(env["HOME"]) / ".local/state/aliasos/current"
    shutil.rmtree(cur, ignore_errors=True)
    cur.mkdir(parents=True)
    (cur / "theme").mkdir()
    (cur / "theme/colors.toml").write_text('background = "#101010"\nforeground = "#eeeeee"\n')
    p = subprocess.Popen([str(BENCH), str(SMALL)], env=env, stderr=subprocess.DEVNULL)
    h.wait_window(30)
    wait_log(log, "rows")
    v = []
    for i in range(runs * 2):
        time.sleep(0.2)
        before = len([l for l in read_log(log) if l.startswith("paint")])
        nxt = cur / "next-theme"
        nxt.mkdir()
        (nxt / "colors.toml").write_text(f'background = "#20{i:02x}20"\nforeground = "#eeeeee"\n')
        t0 = time.monotonic() * 1000
        shutil.rmtree(cur / "theme")
        os.rename(nxt, cur / "theme")
        f = wait_log(log, "paint", after=before)
        v.append(float(f[2]) - t0)
    results["theme change to new frame"] = median(v)
    stop(p)


def png(w, h):
    """A plain RGB PNG with some variety in it, so it isn't trivially small to decode."""
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    rows = b"".join(b"\0" + bytes((x * 7 + y * 3) % 256 for x in range(w * 3)) for y in range(h))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b"")


def measure_preview(h, results, runs):
    """Space on a 1600x1200 photo: until the preview window is mapped, and until the photo is in it."""
    folder = DATA / "fixture-preview"
    folder.mkdir(parents=True, exist_ok=True)
    photo = folder / "photo.png"
    if not photo.exists():
        photo.write_bytes(png(1600, 1200))
    state = Path("/tmp/gnbench-client/preview-state.json")
    shown = Path(str(state) + ".preview")
    log, env = log_env(h, "preview", {"GANNET_STATE": str(state)})
    window, image = [], []
    for _ in range(runs):
        shown.unlink(missing_ok=True)
        p = subprocess.Popen([str(BENCH), str(folder)], env=env, stderr=subprocess.DEVNULL)
        h.wait_window(30)
        wait_log(log, "rows")
        time.sleep(0.2)
        t0 = time.monotonic()
        key(env, "space")
        window.append((h.wait_window(30) - t0) * 1000)
        end = time.monotonic() + 10
        while time.monotonic() < end:
            try:
                if (json.loads(shown.read_text()).get("preview") or {}).get("state") == "image":
                    break
            except (OSError, ValueError):
                pass
            time.sleep(0.001)
        image.append((time.monotonic() - t0) * 1000)
        stop(p)
        time.sleep(0.3)
    results["Space to the preview window"] = median(window)
    results["Space to a 1600x1200 photo in the preview"] = median(image)


def measure_preview_documents(h, results, runs):
    """Space on a text file, and on a Markdown one, each longer than what's shown: until it's laid
    out and drawn in the preview window."""
    folder = DATA / "fixture-documents"
    folder.mkdir(parents=True, exist_ok=True)
    text = folder / "long.log"
    if not text.exists():
        text.write_bytes(b"".join(b"%06d some words in a line of a long log file, for the preview\n" % i for i in range(4000)))
    markdown = folder / "long.md"
    if not markdown.exists():
        section = "## A heading\n\nSome *words* in a paragraph, with **bold** ones and `code`, going on long enough to wrap a few times across the window. " * 2 + "\n\n- an item\n- another, with [a link](https://example.com)\n\n```\nlet code = 1;\n```\n\n"
        markdown.write_text(section * (200_000 // len(section)))
    state = Path("/tmp/gnbench-client/documents-state.json")
    shown = Path(str(state) + ".preview")
    log, env = log_env(h, "documents", {"GANNET_STATE": str(state)})
    for path, label in ((text, "Space to 128 kB of text in the preview"), (markdown, "Space to 128 kB of Markdown in the preview")):
        times = []
        for _ in range(runs):
            shown.unlink(missing_ok=True)
            p = subprocess.Popen([str(BENCH), str(path)], env=env, stderr=subprocess.DEVNULL)
            h.wait_window(30)
            wait_log(log, "rows")
            time.sleep(0.2)
            t0 = time.monotonic()
            key(env, "space")
            end = time.monotonic() + 10
            while time.monotonic() < end:
                try:
                    if ((json.loads(shown.read_text()).get("preview") or {}).get("doc") or {}).get("lines", 0) > 0:
                        break
                except (OSError, ValueError):
                    pass
                time.sleep(0.001)
            times.append((time.monotonic() - t0) * 1000)
            stop(p)
            time.sleep(0.3)
        results[label] = median(times)


def measure_preview_pdf(h, results, runs):
    """Space on a 20-page PDF: until its first page is drawn in the preview window."""
    folder = DATA / "fixture-pdf"
    folder.mkdir(parents=True, exist_ok=True)
    pdf = folder / "report.pdf"
    if not pdf.exists():
        ps = folder / "report.ps"
        ps.write_text("%!PS\n/Helvetica findfont 14 scalefont setfont\n1 1 20 { /n exch def 60 1 40 { /l exch def 72 760 l 16 mul sub moveto (Page ) show n 4 string cvs show ( line ) show l 4 string cvs show ( of some words set in a PDF.) show } for showpage } for\n")
        subprocess.run(["ps2pdf", str(ps), str(pdf)], check=True)
        ps.unlink()
    state = Path("/tmp/gnbench-client/pdf-state.json")
    shown = Path(str(state) + ".preview")
    log, env = log_env(h, "pdf", {"GANNET_STATE": str(state)})
    times = []
    for _ in range(runs):
        shown.unlink(missing_ok=True)
        p = subprocess.Popen([str(BENCH), str(folder)], env=env, stderr=subprocess.DEVNULL)
        h.wait_window(30)
        wait_log(log, "rows")
        time.sleep(0.2)
        t0 = time.monotonic()
        key(env, "space")
        end = time.monotonic() + 20
        while time.monotonic() < end:
            try:
                drawn = (((json.loads(shown.read_text()).get("preview") or {}).get("pdf") or {}).get("drawn") or [])
                if any(page[0] == 0 and page[2] for page in drawn):
                    break
            except (OSError, ValueError):
                pass
            time.sleep(0.002)
        times.append((time.monotonic() - t0) * 1000)
        stop(p)
        time.sleep(0.3)
    results["Space to the first page of a PDF in the preview"] = median(times)


def measure_copy(results):
    src = DATA / "copy-1g.bin"
    if not src.exists() or src.stat().st_size != 1 << 30:
        with open(src, "wb") as f:
            for _ in range(1024):
                f.write(os.urandom(1 << 20))
    open(src, "rb").read()
    dest = Path("/tmp/gnbench-copy")
    assert str(dest).startswith("/tmp/gnbench")
    tool = ROOT / "target/bench/release/examples/copybench"
    env = dict(os.environ)
    gannet, cp = [], []
    for _ in range(3):
        for which in ("gannet", "cp"):
            shutil.rmtree(dest, ignore_errors=True)
            dest.mkdir()
            if which == "gannet":
                out = subprocess.run([str(tool), str(src), str(dest)], capture_output=True, text=True, env=env, check=True)
                gannet.append(float(out.stdout))
            else:
                t = time.monotonic()
                subprocess.run(["cp", str(src), str(dest / src.name)], check=True)
                cp.append((time.monotonic() - t) * 1000)
    shutil.rmtree(dest, ignore_errors=True)
    results["copy 1 GiB across disks, vs cp"] = median(gannet) / median(cp)
    results["_copy"] = (median(gannet), median(cp))


def measure_trash(results):
    root = DATA / "trash-test"
    assert str(root).startswith(str(ROOT))
    tool = ROOT / "target/bench/release/examples/trashbench"
    v = []
    for _ in range(3):
        shutil.rmtree(root, ignore_errors=True)
        (root / "work").mkdir(parents=True)
        for i in range(5000):
            (root / "work" / f"file-{i}.txt").write_bytes(b"x" * 4096)
        out = subprocess.run([str(tool), str(root / "work"), str(root / "data")], capture_output=True, text=True, check=True)
        v.append(float(out.stdout))
    shutil.rmtree(root, ignore_errors=True)
    results["move 5,000 files to Trash"] = median(v)


def nautilus(h, runs):
    """Nautilus on the same folder, inside bubblewrap with the real home hidden and a
    private session bus. It browses and nothing else. GTK uses its cairo renderer here."""
    home = os.path.expanduser("~")
    out = {}
    for folder, tag in ((SMALL, "small"), (BIG, "100k")):
        first, settled, pss, cpu = [], [], [], []
        for _ in range(runs):
            env = env_for(h, "nautilus", {"GSK_RENDERER": "cairo", "GDK_BACKEND": "wayland", "GIO_USE_VFS": "local", "GSETTINGS_BACKEND": "memory"})
            cmd = ["bwrap", "--die-with-parent", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", home, "--tmpfs", "/tmp",
                   "--ro-bind", str(folder), str(folder), "--bind", "/tmp/gnbench-client", "/tmp/gnbench-client", "--bind", str(h.root), str(h.root),
                   "dbus-run-session", "--", "nautilus", "--new-window", str(folder)]
            t0 = time.monotonic()
            p = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            first.append((h.wait_window(60) - t0) * 1000)
            settled.append(settle(p.pid, limit=120))
            pss.append(pss_kib(p.pid) / 1024)
            cpu.append(cpu_seconds(p.pid))
            try:
                os.killpg(p.pid, 15)
            except ProcessLookupError:
                pass
            p.wait(10)
            time.sleep(0.3)
        out[tag] = dict(first=median(first), settled=median(settled), pss=median(pss), cpu=median(cpu))
    return out


def gannet_same_way(h, runs):
    out = {}
    for folder, tag in ((SMALL, "small"), (BIG, "100k")):
        first, settled, pss, cpu = [], [], [], []
        for _ in range(runs):
            t0 = time.monotonic()
            p = subprocess.Popen([str(PLAIN), str(folder)], env=env_for(h, "plain"), stderr=subprocess.DEVNULL)
            first.append((h.wait_window(30) - t0) * 1000)
            settled.append(settle(p.pid))
            pss.append(pss_kib(p.pid) / 1024)
            cpu.append(cpu_seconds(p.pid))
            stop(p)
        out[tag] = dict(first=median(first), settled=median(settled), pss=median(pss), cpu=median(cpu))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=7)
    ap.add_argument("--no-nautilus", action="store_true")
    a = ap.parse_args()
    fixture()
    results = {}
    h = Headless("/tmp/gnbench-h")
    try:
        measure(a.runs, results, h)
        results["paint scrolling 100k p99, 1280x800"] = p99(scroll_paints(h, "scroll"))
        measure_stall(h, results)
        measure_theme(h, results, a.runs)
        measure_preview(h, results, a.runs)
        measure_preview_documents(h, results, a.runs)
        measure_preview_pdf(h, results, a.runs)
    finally:
        h.stop()
    h = Headless("/tmp/gnbench-h", "3840x2160")
    try:
        paints = scroll_paints(h, "scroll4k")
        results["paint scrolling 100k p99, 3840x2160"] = p99(paints)
        results["_4k_median"] = median(paints)
    finally:
        h.stop()
    measure_copy(results)
    measure_trash(results)
    print(f"\n{'metric':40s} {'measured':>10s} {'budget':>8s}")
    missed = 0
    for metric, limit, unit in BUDGETS:
        v = results[metric]
        ok = v <= limit
        missed += not ok
        print(f"{metric:40s} {v:10.2f} {limit:>8} {unit:6s} {'ok' if ok else 'OVER BUDGET'}")
    print(f"\nRSS holding 100k: {results['_rss']:.1f} MiB. Main-loop passes over 0.5 ms during the slow load: {results['_stall_count']}.")
    print(f"Copy: Gannet {results['_copy'][0]:.0f} ms, cp {results['_copy'][1]:.0f} ms.")
    h = Headless("/tmp/gnbench-h")
    try:
        same = gannet_same_way(h, a.runs)
        naut = None if a.no_nautilus else nautilus(h, max(3, a.runs // 2))
    finally:
        h.stop()
    print(f"\n{'same harness, in sway, software render':40s} {'first window':>13s} {'settled':>9s} {'PSS':>10s} {'CPU':>8s}")
    for name, data in (("Gannet", same), ("Nautilus 51.1", naut)):
        if not data:
            continue
        for tag, d in data.items():
            print(f"{name + ', ' + tag:40s} {d['first']:10.1f} ms {d['settled']:7.2f} s {d['pss']:6.1f} MiB {d['cpu']:6.2f} s")
    shutil.rmtree("/tmp/gnbench-client", ignore_errors=True)
    print("\nmissed budgets:", missed)
    sys.exit(1 if missed else 0)


if __name__ == "__main__":
    main()
