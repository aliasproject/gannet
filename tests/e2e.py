#!/usr/bin/env python3
"""End-to-end checks: the real app in a headless compositor, driven by real key and
pointer events, on scratch folders only. The app runs in bubblewrap with the real home
hidden and read-only everything else, so a bug here can't reach real files.

  python3 -B tests/e2e.py [filter]
"""
import hashlib, json, os, re, shutil, signal, stat, struct, subprocess, sys, time, zlib
from pathlib import Path
from urllib.parse import quote

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "bench/harness"))
from hl import Headless, Ipc, tree as hl_tree

BIN = ROOT / "target/bench/release/gannet"
PREVIEW_APP_ID = "com.aliasproject.Gannet.Preview"
SCRATCH = Path("/tmp/gne2e")
SHOTS = ROOT / ".bench/shots"


class App:
    def __init__(self, h, start, extra_env=None, path_prefix=None, args=None, bwrap_extra=(), state_name="state.json", window=True, launch=True):
        self.h = h
        self.fs = SCRATCH / "fs"
        self.state_file = SCRATCH / state_name
        # The preview window writes its own beside it.
        self.preview_file = SCRATCH / (state_name + ".preview")
        for f in (self.state_file, self.preview_file):
            if f.exists():
                f.unlink()
        self.env = h.env(SCRATCH / "client", {"GANNET_STATE": str(self.state_file), **(extra_env or {})})
        if path_prefix:
            self.env["PATH"] = f"{path_prefix}:{self.env['PATH']}"
        self.proc = None
        if not launch:
            # Something else starts the window, and this only watches it.
            return
        cmd = sandboxed(args if args is not None else [str(start)], bwrap_extra)
        self.proc = subprocess.Popen(cmd, env=self.env, stderr=open(SCRATCH / "stderr.log", "a"), stdout=subprocess.DEVNULL)
        if window:
            h.wait_window(10)

    def state(self):
        """The list's state, with "preview" being what its preview window is showing, if one's open."""
        try:
            s = json.loads(self.state_file.read_text())
        except (OSError, ValueError):
            return {}
        if s.get("previewing") is None:
            # The window has gone, so whatever it last wrote is out of date.
            self.preview_file.unlink(missing_ok=True)
            s["preview"] = None
        else:
            try:
                s["preview"] = json.loads(self.preview_file.read_text()).get("preview")
            except (OSError, ValueError):
                s["preview"] = None
        return s

    def preview_window(self):
        """The preview window's place on screen, as sway has it: x, y, w, h."""
        def find(node):
            if node.get("app_id") == PREVIEW_APP_ID:
                return node
            for child in node.get("nodes", []) + node.get("floating_nodes", []):
                found = find(child)
                if found:
                    return found
            return None
        node = find(self.h.cmd.call(4))
        assert node, "no preview window"
        r = node["rect"]
        return r["x"], r["y"], r["width"], r["height"]

    def resize_preview(self, w, h):
        self.sway(f'[app_id="{PREVIEW_APP_ID}"] resize set {w} {h}')

    def focus_list(self):
        self.sway('[app_id="^com\\.aliasproject\\.Gannet$"] focus')

    def wait(self, pred, what="", timeout=6):
        end = time.monotonic() + timeout
        s = {}
        while time.monotonic() < end:
            s = self.state()
            try:
                if s and pred(s):
                    return s
            except (KeyError, IndexError, TypeError):
                pass
            time.sleep(0.01)
        raise AssertionError(f"timed out waiting for {what or pred}: {json.dumps(s)[:1500]}")

    def key(self, *keys, mods=()):
        """Each key is a keysym name, like Down or F2."""
        cmd = ["wtype"]
        for m in mods:
            cmd += ["-M", m]
        for k in keys:
            cmd += ["-k", k]
        for m in mods:
            cmd += ["-m", m]
        r = subprocess.run(cmd, env=self.env, capture_output=True, timeout=10)
        assert r.returncode == 0, r.stderr

    def type(self, text):
        r = subprocess.run(["wtype", "--", text], env=self.env, capture_output=True, timeout=10)
        assert r.returncode == 0, r.stderr

    def sway(self, *cmd):
        return self.h.cmd.call(0, " ".join(cmd).encode())

    def move(self, x, y):
        self.h.pointer(f"move {x} {y} 1280 800")
        time.sleep(0.03)

    def click(self, x, y, button=0x110):
        self.move(x, y)
        self.h.pointer(f"button {button} 1")
        self.h.pointer(f"button {button} 0")
        time.sleep(0.03)

    def scroll(self, lines):
        self.h.pointer(f"scroll {lines}")
        time.sleep(0.03)

    def shot(self, name):
        SHOTS.mkdir(parents=True, exist_ok=True)
        time.sleep(0.1)  # The state changes a frame or so before the picture does.
        subprocess.run(["grim", str(SHOTS / f"{name}.png")], env=self.env, capture_output=True, timeout=10)

    def close(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        # Let the compositor finish removing the window, or the next one shares its
        # layout for a moment and pointer events go astray.
        time.sleep(0.3)

    def alive(self):
        return self.proc is not None and self.proc.poll() is None


def sandboxed(args, bwrap_extra=(), die_with_parent=True):
    """The command that runs Gannet with the real home hidden and only the scratch folder writable."""
    home = os.path.expanduser("~")
    # A full path, because D-Bus starts services without searching PATH.
    return [shutil.which("bwrap") or "/usr/bin/bwrap", *(["--die-with-parent"] if die_with_parent else []), "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", home, *hide_real_trash(),
            "--ro-bind", str(BIN), str(BIN), "--bind", str(SCRATCH), str(SCRATCH), *bwrap_extra, str(BIN), *args]


def hide_real_trash():
    """The Trash view looks on every disk, so real disks' Trash folders are covered over."""
    uid = os.geteuid()
    out = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        point = line.split()[4].replace("\\040", " ")
        for t in (Path(point) / f".Trash-{uid}", Path(point) / ".Trash" / str(uid)):
            try:
                if t.is_dir() and not str(t).startswith(str(SCRATCH)):
                    out += ["--tmpfs", str(t)]
            except OSError:
                pass
    return out


def AppWithDisk(h, start, disk):
    return App(h, start, bwrap_extra=("--tmpfs", str(disk)))


def AppWithDisk2(h, disk):
    """A second window, on a folder that's a disk of its own."""
    return App(h, disk, state_name="state2.json", bwrap_extra=("--tmpfs", str(disk)))


def fresh_fs():
    fs = SCRATCH / "fs"
    assert str(fs).startswith("/tmp/gne2e/")
    if fs.exists():
        subprocess.run(["chmod", "-R", "u+rwx", str(fs)])
        shutil.rmtree(fs)
    fs.mkdir(parents=True)
    return fs


def tree(fs, spec):
    for name, content in spec.items():
        p = fs / name
        p.parent.mkdir(parents=True, exist_ok=True)
        if content is None:
            p.mkdir(exist_ok=True)
        else:
            p.write_bytes(content if isinstance(content, bytes) else content.encode())


TESTS = []


def check(f):
    TESTS.append(f)
    return f


# Row and column positions, in logical pixels at scale 1.
SIDE, BAR, HEAD, ROW = 190, 44, 28, 26


def row_y(i):
    return BAR + HEAD + i * ROW + ROW // 2


# On a short name, where a drag carries the file. Beside the name, it draws a box.
ROW_X = 235
BESIDE_X = 900


@check
def browse_sort_and_history(h):
    fs = fresh_fs()
    tree(fs, {"b10.txt": "x", "b9.txt": "xxxxx", "A.TXT": "xxx", ".hidden": "", "zdir/inner.txt": "", "adir/deep/er.txt": ""})
    a = App(h, fs)
    s = a.wait(lambda s: not s["loading"] and s["count"] == 5, "first listing")
    assert s["names"] == ["adir", "zdir", "A.TXT", "b9.txt", "b10.txt"], s["names"]
    a.shot("browse")
    # Hidden files
    a.key("h", mods=["ctrl"])
    s = a.wait(lambda s: s["count"] == 6, "hidden shown")
    assert ".hidden" in s["names"] and s["hidden"]
    a.key("h", mods=["ctrl"])
    a.wait(lambda s: s["count"] == 5, "hidden hidden")
    # Sort by size, both ways
    a.key("2", mods=["ctrl"])
    s = a.wait(lambda s: s["sort"] == "Size" and s["names"][2:] == ["b10.txt", "A.TXT", "b9.txt"], "size ascending")
    a.key("2", mods=["ctrl"])
    a.wait(lambda s: s["desc"] and s["names"][2:] == ["b9.txt", "A.TXT", "b10.txt"], "size descending")
    a.key("1", mods=["ctrl"])
    a.wait(lambda s: s["sort"] == "Name" and s["names"][2] == "A.TXT", "name again")
    # Into a folder with the keyboard, back, forward, up
    a.key("Down")
    s = a.wait(lambda s: s["selected"] == ["adir"], "first press selects the first row")
    a.key("Return")
    s = a.wait(lambda s: s["dir"] == str(fs / "adir") and s["names"] == ["deep"], "entered adir")
    assert s["can_back"]
    a.key("Left", mods=["alt"])
    s = a.wait(lambda s: s["dir"] == str(fs) and s["selected"] == ["adir"], "back keeps the cursor")
    assert s["can_forward"]
    a.key("Right", mods=["alt"])
    a.wait(lambda s: s["dir"] == str(fs / "adir"), "forward")
    a.key("BackSpace")
    a.wait(lambda s: s["dir"] == str(fs) and s["selected"] == ["adir"], "up lands on the folder it came from")
    a.close()


@check
def typed_path_filter_and_type_ahead(h):
    fs = fresh_fs()
    tree(fs, {"apple.txt": "", "banana.txt": "", "Blueberry.md": "", "cherry.txt": "", "sub/x": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 5)
    # Filter
    a.key("f", mods=["ctrl"])
    a.wait(lambda s: s["edit"].startswith("filter:"))
    a.type("BERRY")
    s = a.wait(lambda s: s["filter"] == "BERRY" and s["names"] == ["Blueberry.md", "cherry.txt"] or s["names"] == ["Blueberry.md"], "filter applied")
    a.shot("filter")
    a.key("Escape")
    a.wait(lambda s: s["filter"] == "" and s["count"] == 5, "filter cleared")
    # Type-ahead
    a.type("ch")
    a.wait(lambda s: s["selected"] == ["cherry.txt"], "type-ahead found it")
    time.sleep(1.0)
    a.type("b")
    a.wait(lambda s: s["selected"] == ["banana.txt"] or s["selected"] == ["Blueberry.md"], "type-ahead restarted")
    # Typed path
    a.key("l", mods=["ctrl"])
    a.wait(lambda s: s["edit"].startswith("path:"))
    a.type(str(fs / "sub"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "sub") and s["names"] == ["x"], "typed path")
    # A folder that doesn't exist reports, and back still works
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "nope"))
    a.key("Return")
    s = a.wait(lambda s: s["dir"] == str(fs / "nope") and s["error"], "error for a missing folder")
    assert "No such file" in s["error"]
    a.shot("missing-folder")
    a.key("Left", mods=["alt"])
    a.wait(lambda s: s["dir"] == str(fs / "sub") and s["names"] == ["x"], "back from the error")
    a.close()


@check
def mouse_selection_and_columns(h):
    fs = fresh_fs()
    tree(fs, {f"f{i}.txt": "" for i in range(6)} | {"dir1/x": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 7)
    a.click(ROW_X, row_y(1))
    a.wait(lambda s: s["selected"] == ["f0.txt"], "click selects")
    a.key("Down", mods=["shift"])
    a.wait(lambda s: s["selected"] == ["f0.txt", "f1.txt"], "shift extends")
    a.key("a", mods=["ctrl"])
    a.wait(lambda s: len(s["selected"]) == 7, "select all")
    a.key("Escape")
    a.wait(lambda s: s["selected"] == [], "escape clears")
    # Header click sorts
    a.click(1200, BAR + HEAD // 2)
    a.wait(lambda s: s["sort"] == "Modified", "header sorts")
    a.click(SIDE + 40, BAR + HEAD // 2)
    a.wait(lambda s: s["sort"] == "Name", "name header")
    # Double click enters a folder
    a.click(ROW_X, row_y(0))
    a.click(ROW_X, row_y(0))
    a.wait(lambda s: s["dir"] == str(fs / "dir1"), "double click opens")
    # Toolbar back button
    a.click(6 + 18, BAR // 2)
    a.wait(lambda s: s["dir"] == str(fs), "back button")
    # Right click menu, chosen by keyboard
    a.click(ROW_X, row_y(2), 0x111)
    a.wait(lambda s: s["menu"] and s["selected"] == ["f1.txt"], "context menu on a row")
    a.shot("menu")
    a.key("Escape")
    a.wait(lambda s: not s["menu"], "menu closes")
    a.close()


@check
def scrolling_with_wheel_keys_and_scrollbar(h):
    fs = fresh_fs()
    for i in range(500):
        (fs / f"f{i:03}").touch()
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 500 and not s["loading"])
    a.move(ROW_X, row_y(3))
    a.scroll(2)
    s = a.wait(lambda s: s["scroll"] > 0, "wheel scrolls")
    first = s["scroll"]
    a.scroll(-2)
    a.wait(lambda s: s["scroll"] == 0, "wheel back")
    assert first == 6 * ROW, first
    a.key("End")
    s = a.wait(lambda s: s["cursor"] == 499 and s["scroll"] > 0, "End goes to the bottom")
    a.shot("scrolled")
    a.key("Home")
    a.wait(lambda s: s["cursor"] == 0 and s["scroll"] == 0, "Home goes to the top")
    a.key("Next")
    a.wait(lambda s: s["cursor"] > 10, "Page Down")
    # Scrollbar: clicking low on the track jumps down.
    a.click(1278, 700)
    a.wait(lambda s: s["scroll"] > 5000, "scrollbar click")
    # After release, moving the pointer doesn't drag it.
    now = a.state()["scroll"]
    a.move(1278, 200)
    time.sleep(0.1)
    assert a.state()["scroll"] == now
    a.close()


@check
def opening_uses_the_default_app(h):
    fs = fresh_fs()
    tree(fs, {"doc.txt": "hello", "bad.xyz": ""})
    bin_dir = SCRATCH / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    log = SCRATCH / "opened.log"
    log.unlink(missing_ok=True)
    opener = bin_dir / "xdg-open"
    opener.write_text(f'#!/bin/sh\necho "$1" >> {log}\ncase "$1" in *.xyz) exit 3;; esac\nexit 0\n')
    opener.chmod(0o755)
    a = App(h, fs, path_prefix=str(bin_dir))
    a.wait(lambda s: s["count"] == 2)
    a.type("doc")
    a.wait(lambda s: s["selected"] == ["doc.txt"])
    a.key("Return")
    end = time.monotonic() + 5
    while time.monotonic() < end and not log.exists():
        time.sleep(0.02)
    assert log.read_text().strip() == str(fs / "doc.txt"), log.read_text()
    assert a.wait(lambda s: s["notices"] == [], "no complaint")
    time.sleep(1.0)
    a.type("bad")
    a.wait(lambda s: s["selected"] == ["bad.xyz"])
    a.key("Return")
    s = a.wait(lambda s: s["notices"], "failure shown")
    assert "bad.xyz" in s["notices"][0] and "No app" in s["notices"][0], s["notices"]
    a.shot("open-failed")
    a.close()


@check
def live_refresh_keeps_selection(h):
    fs = fresh_fs()
    tree(fs, {"a": "", "b": "", "c": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    a.type("b")
    a.wait(lambda s: s["selected"] == ["b"])
    (fs / "aa").write_text("")
    s = a.wait(lambda s: s["count"] == 4 and s["selected"] == ["b"], "new file appears, selection stays")
    assert s["names"] == ["a", "aa", "b", "c"]
    (fs / "b").unlink()
    a.wait(lambda s: s["count"] == 3 and s["selected"] == [], "removed file goes")
    a.close()


@check
def theme_follows_swaps_and_works_without_any(h):
    fs = fresh_fs()
    client = SCRATCH / "client"
    if client.exists():
        shutil.rmtree(client)
    a = App(h, fs)
    s = a.wait(lambda s: "base" in s)
    assert s["base"] == 0xfafafa, "no theme at all means the light default"
    a.shot("theme-none")
    # A theme appears where aliasOS keeps it.
    cur = client / "home/.local/state/aliasos/current"
    cur.mkdir(parents=True)
    next_dir = cur / "next-theme"
    next_dir.mkdir()
    (next_dir / "colors.toml").write_text('background = "#112233"\nforeground = "#ddeeff"\naccent = "#ff8800"\n')
    os.rename(next_dir, cur / "theme")
    a.wait(lambda s: s["base"] == 0x112233, "first theme applied")
    a.shot("theme-one")
    # The same swap aliasos-theme-apply does.
    for color, val in (("#445566", 0x445566), ("#778899", 0x778899)):
        next_dir.mkdir()
        (next_dir / "colors.toml").write_text(f'background = "{color}"\nforeground = "#000000"\n')
        shutil.rmtree(cur / "theme")
        os.rename(next_dir, cur / "theme")
        a.wait(lambda s: s["base"] == val, f"swap to {color}")
    a.shot("theme-two")
    a.close()
    shutil.rmtree(client)


@check
def asks_for_a_minimum_window_size(h):
    # Headless sway doesn't report floating resizes, so this checks the request that
    # makes a compositor stop the drag.
    log = SCRATCH / "stderr.log"
    log.write_text("")
    a = App(h, fresh_fs(), extra_env={"WAYLAND_DEBUG": "1"})
    try:
        a.wait(lambda s: s["count"] == 0)
        a.close()
        text = log.read_text()
    finally:
        log.write_text("")  # The trace would bury the app's real errors in the report.
    assert "xdg_toplevel" in text, "precondition: the log has the protocol trace"
    assert re.search(r"xdg_toplevel@\d+\.set_min_size\(480, 320\)", text), "no set_min_size(480, 320) request"


@check
def huge_names_odd_names_and_a_resize(h):
    fs = fresh_fs()
    tree(fs, {"n" * 250 + ".txt": "", "日本語のファイル.txt": "", "emoji 😀 file.png": "", "tab\tname": ""})
    raw = str(fs).encode() + b"/bad\xff\xfename"
    with open(raw, "wb"):
        pass
    a = App(h, fs)
    s = a.wait(lambda s: s["count"] == 5)
    time.sleep(0.5)
    a.shot("odd-names")
    a.sway("output HEADLESS-1 scale 1.5")
    time.sleep(0.3)
    assert a.alive()
    a.shot("scale-1.5")
    a.sway("output HEADLESS-1 scale 1")
    time.sleep(0.2)
    a.sway("output HEADLESS-1 resolution 300x200")
    time.sleep(0.3)
    assert a.alive(), "survives a tiny window"
    a.sway("output HEADLESS-1 resolution 300x40")
    time.sleep(0.3)
    assert h.cmd.call(3)[0]["rect"]["height"] == 40, "precondition: the output really shrank"
    assert a.alive(), "survives a window shorter than its bars"
    a.key("Down")
    a.wait(lambda s: s["cursor_shown"])
    a.sway("output HEADLESS-1 resolution 1280x800")
    time.sleep(0.2)
    assert a.alive()
    a.close()


@check
def slow_folder_doesnt_block_the_ui(h):
    fs = fresh_fs()
    (fs / "big").mkdir()
    for i in range(3000):
        (fs / "big" / f"f{i}").touch()
    (fs / "quick").mkdir()
    (fs / "quick" / "done").touch()
    a = App(h, fs / "big", {"GANNET_SLOW_READ_MS": "150"})
    s = a.wait(lambda s: s["loading"] and 0 < s["count"] < 3000, "partial rows while it reads")
    a.shot("slow-partial")
    # The UI stays responsive, and leaving doesn't wait for the slow read.
    a.key("Down")
    a.wait(lambda s: s["cursor_shown"], "keys work during the read")
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "quick"))
    start = time.monotonic()
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "quick") and s["names"][:1] == ["done"], "can leave at once")
    assert time.monotonic() - start < 1.0
    a.close()


@check
def permission_denied_is_shown(h):
    fs = fresh_fs()
    tree(fs, {"locked/secret": "", "ok": ""})
    (fs / "locked").chmod(0o000)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    a.key("Down")
    a.wait(lambda s: s["selected"] == ["locked"])
    a.key("Return")
    s = a.wait(lambda s: s["error"] and "denied" in s["error"], "denied message")
    a.shot("denied")
    a.key("Left", mods=["alt"])
    a.wait(lambda s: s["dir"] == str(fs) and s["count"] == 2, "back out")
    a.close()
    (fs / "locked").chmod(0o755)


def wl_tool(a, argv, **kw):
    return subprocess.run(argv, env=a.env, capture_output=True, timeout=10, **kw)


@check
def copy_paste_within_the_app(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "AAA", "src/b.txt": "BBB", "dst/keep": "k"})
    a = App(h, fs / "src")
    a.wait(lambda s: s["count"] == 2)
    a.key("a", mods=["ctrl"])
    a.wait(lambda s: len(s["selected"]) == 2)
    a.key("c", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    s = a.wait(lambda s: s["count"] == 3 and sorted(s["selected"]) == ["a.txt", "b.txt"] and s["jobs"] == 0, "pasted and selected")
    assert (fs / "dst/a.txt").read_text() == "AAA" and (fs / "src/a.txt").exists()
    # Pasting again asks, and Keep both makes a copy name.
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["dialog"] == "conflict", "conflict question")
    a.shot("conflict")
    time.sleep(0.45)  # A new conflict ignores input for a moment.
    a.type("a")
    a.type("k")
    a.wait(lambda s: s["jobs"] == 0 and s["count"] == 5, "kept both for all")
    assert (fs / "dst/b (copy).txt").read_text() == "BBB"
    # Replace overwrites only when asked.
    (fs / "src/a.txt").write_text("NEW")
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["dialog"] == "conflict")
    time.sleep(0.45)
    a.key("Return")  # Enter means Skip.
    time.sleep(0.1)
    a.wait(lambda s: s["dialog"] == "conflict", "second file asks too")
    time.sleep(0.45)
    a.type("s")
    a.wait(lambda s: s["jobs"] == 0)
    assert (fs / "dst/a.txt").read_text() == "AAA", "skip must not overwrite"
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["dialog"] == "conflict")
    time.sleep(0.45)
    a.type("a")
    a.key("Left")
    a.key("Return")
    a.wait(lambda s: s["jobs"] == 0)
    assert (fs / "dst/a.txt").read_text() == "NEW"
    a.close()


@check
def cut_paste_moves_and_undo_brings_it_back(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "AAA", "src/other": "", "dst/x": ""})
    a = App(h, fs / "src")
    a.wait(lambda s: s["count"] == 2)
    a.type("a")
    a.wait(lambda s: s["selected"] == ["a.txt"])
    a.key("x", mods=["ctrl"])
    time.sleep(0.1)
    assert (fs / "src/a.txt").exists(), "a cut does nothing until the paste"
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["selected"] == ["a.txt"] and s["jobs"] == 0, "moved")
    assert not (fs / "src/a.txt").exists() and (fs / "dst/a.txt").read_text() == "AAA"
    a.key("z", mods=["ctrl"])
    a.wait(lambda s: s["count"] == 1 and s["jobs"] == 0, "undone")
    assert (fs / "src/a.txt").read_text() == "AAA" and not (fs / "dst/a.txt").exists()
    a.close()


@check
def clipboard_works_with_other_programs(h):
    fs = fresh_fs()
    tree(fs, {"my file.txt": "hello", "dst/k": "", "outside/from-elsewhere.txt": "other"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    a.type("my")
    a.wait(lambda s: s["selected"] == ["my file.txt"])
    a.key("c", mods=["ctrl"])
    time.sleep(0.2)
    types = wl_tool(a, ["wl-paste", "-l"]).stdout.decode()
    assert "x-special/gnome-copied-files" in types and "text/uri-list" in types, types
    got = wl_tool(a, ["wl-paste", "-n", "-t", "x-special/gnome-copied-files"]).stdout.decode()
    assert got == f"copy\nfile://{fs}/my%20file.txt", got
    # And from another program, as Nautilus would put it there, this time a cut.
    payload = f"cut\nfile://{fs}/outside/from-elsewhere.txt"
    p = subprocess.Popen(["wl-copy", "-t", "x-special/gnome-copied-files", "--", payload], env=a.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    p.wait(5)
    time.sleep(0.2)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: "from-elsewhere.txt" in s["names"] and s["jobs"] == 0, "pasted from another program")
    assert not (fs / "outside/from-elsewhere.txt").exists(), "a cut from elsewhere moves"
    # Plain text on the clipboard isn't files.
    subprocess.Popen(["wl-copy", "--", "just words"], env=a.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).wait(5)
    time.sleep(0.2)
    a.key("v", mods=["ctrl"])
    s = a.wait(lambda s: s["notices"], "says so")
    assert "no files" in s["notices"][0]
    a.close()


@check
def rename_new_folder_and_bad_names(h):
    fs = fresh_fs()
    tree(fs, {"report.pdf": "x", "taken.pdf": "y"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    a.type("rep")
    a.wait(lambda s: s["selected"] == ["report.pdf"])
    a.key("F2")
    a.wait(lambda s: s["edit"] == "rename:report.pdf")
    a.shot("rename")
    a.type("summary")
    a.wait(lambda s: s["edit"] == "rename:summary.pdf", "typing replaces the stem only")
    a.key("Return")
    a.wait(lambda s: s["selected"] == ["summary.pdf"] and s["jobs"] == 0, "renamed and selected")
    assert (fs / "summary.pdf").read_text() == "x" and not (fs / "report.pdf").exists()
    a.key("z", mods=["ctrl"])
    a.wait(lambda s: s["selected"] == ["report.pdf"], "undo rename")
    assert (fs / "report.pdf").exists()
    # Escape cancels.
    a.key("F2")
    a.wait(lambda s: s["edit"].startswith("rename:"))
    a.type("zzz")
    a.key("Escape")
    a.wait(lambda s: s["edit"] == "none")
    assert (fs / "report.pdf").exists() and not (fs / "zzz.pdf").exists()
    # A name that's taken reports and overwrites nothing.
    a.key("F2")
    a.wait(lambda s: s["edit"].startswith("rename:"))
    a.key("a", mods=["ctrl"])
    a.type("taken.pdf")
    a.key("Return")
    s = a.wait(lambda s: s["notices"], "taken name reported")
    assert (fs / "taken.pdf").read_text() == "y" and (fs / "report.pdf").read_text() == "x", s
    # Escape clears the selection, then dismisses the notice.
    a.key("Escape")
    a.wait(lambda s: s["selected"] == [])
    a.key("Escape")
    a.wait(lambda s: s["notices"] == [])
    # A name with a slash is refused.
    a.key("n", mods=["ctrl", "shift"])
    a.wait(lambda s: s["edit"] == "newfolder:New Folder", "new folder field")
    a.shot("new-folder")
    a.type("a/b")
    a.key("Return")
    a.wait(lambda s: any("/" in n for n in s["notices"]), "bad name refused")
    assert not (fs / "a").exists()
    a.key("n", mods=["ctrl", "shift"])
    a.wait(lambda s: s["edit"].startswith("newfolder:"))
    a.type("Made Here")
    a.key("Return")
    a.wait(lambda s: s["selected"] == ["Made Here"] and s["jobs"] == 0, "folder made and selected")
    assert (fs / "Made Here").is_dir()
    a.close()


@check
def trash_undo_and_delete_for_good(h):
    fs = fresh_fs()
    tree(fs, {"doomed.txt": "bye", "gone-for-good.txt": "x", "stay.txt": "s"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    trash = SCRATCH / "client/data/Trash"
    shutil.rmtree(trash, ignore_errors=True)
    a.type("doomed")
    a.wait(lambda s: s["selected"] == ["doomed.txt"])
    a.key("Delete")
    a.wait(lambda s: s["count"] == 2 and s["jobs"] == 0, "trashed")
    assert not (fs / "doomed.txt").exists()
    assert (trash / "files/doomed.txt").read_text() == "bye"
    info = (trash / "info/doomed.txt.trashinfo").read_text().splitlines()
    assert info[0] == "[Trash Info]" and info[1] == f"Path={fs}/doomed.txt" and info[2].startswith("DeletionDate="), info
    a.key("z", mods=["ctrl"])
    a.wait(lambda s: s["count"] == 3 and s["selected"] == ["doomed.txt"], "restored by undo")
    assert (fs / "doomed.txt").read_text() == "bye" and not (trash / "files/doomed.txt").exists()
    # Delete for good asks first, and Enter on the default does nothing.
    time.sleep(1.0)
    a.type("gone")
    a.wait(lambda s: s["selected"] == ["gone-for-good.txt"])
    a.key("Delete", mods=["shift"])
    a.wait(lambda s: s["dialog"] == "delete", "asks")
    a.shot("confirm-delete")
    a.key("Return")
    a.wait(lambda s: s["dialog"] == "", "enter takes the safe default")
    assert (fs / "gone-for-good.txt").exists()
    a.key("Delete", mods=["shift"])
    a.wait(lambda s: s["dialog"] == "delete")
    a.type("d")
    a.wait(lambda s: s["count"] == 2 and s["jobs"] == 0, "deleted")
    assert not (fs / "gone-for-good.txt").exists() and not (trash / "files/gone-for-good.txt").exists()
    a.close()


@check
def trash_on_another_disk_and_a_real_cross_disk_move(h):
    fs = fresh_fs()
    disk = SCRATCH / "otherdisk"
    disk.mkdir(exist_ok=True)
    tree(fs, {"here/file.txt": "move me", "here/d/inner": "inside"})
    # bubblewrap gives the app a real second filesystem at this path. The test can't see
    # it from outside, so the app itself makes and checks everything there.
    a = AppWithDisk(h, fs / "here", disk)
    a.wait(lambda s: s["count"] == 2)
    a.key("a", mods=["ctrl"])
    a.key("x", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(disk))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(disk))
    a.key("v", mods=["ctrl"])
    s = a.wait(lambda s: sorted(s["names"]) == ["d", "file.txt"] and s["jobs"] == 0, "moved across disks")
    assert s["notices"] == [], s["notices"]
    assert not (fs / "here/file.txt").exists() and not (fs / "here/d").exists(), "sources removed after copying"
    # Trash on that disk uses its own .Trash folder, not the home one.
    a.type("file")
    a.wait(lambda s: s["selected"] == ["file.txt"])
    a.key("Delete")
    a.wait(lambda s: s["count"] == 1 and s["jobs"] == 0, "trashed on the other disk")
    a.key("h", mods=["ctrl"])
    s = a.wait(lambda s: any(n.startswith(".Trash-") for n in s["names"]), "per-disk Trash folder")
    assert not (SCRATCH / "client/data/Trash/files/file.txt").exists()
    a.close()


@check
def the_trash_view_restores_and_empties_across_disks(h):
    fs = fresh_fs()
    disk = SCRATCH / "otherdisk"
    disk.mkdir(exist_ok=True)
    tree(fs, {"home/a.txt": "a", "home/b.txt": "b", "home/c.txt": "c"})
    trash = SCRATCH / "client/data/Trash"
    shutil.rmtree(trash, ignore_errors=True)
    a = AppWithDisk(h, fs / "home", disk)
    a.wait(lambda s: s["count"] == 3)

    def go(path):
        a.key("l", mods=["ctrl"])
        a.type(str(path))
        a.key("Return")
        a.wait(lambda s: s["dir"] == str(path) and not s["loading"], f"at {path}")

    def pick(name):
        time.sleep(1.0)  # Let type-ahead forget the last name.
        a.type(name)
        a.wait(lambda s: s["selected"] == [name], f"{name} selected")

    def menu_pick(downs):
        a.key("F10")
        a.wait(lambda s: s["menu"], "menu open")
        for _ in range(downs):
            a.key("Down")
        a.key("Return")

    for n in ["a.txt", "b.txt"]:
        pick(n)
        a.key("Delete")
        a.wait(lambda s, n=n: n not in s["names"] and s["jobs"] == 0, f"{n} trashed")
    # c.txt goes to the other disk's own Trash. The app makes everything there itself.
    pick("c.txt")
    a.key("x", mods=["ctrl"])
    time.sleep(0.1)
    go(disk)
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["names"] == ["c.txt"] and s["jobs"] == 0, "moved to the other disk")
    pick("c.txt")
    a.key("Delete")
    a.wait(lambda s: s["names"] == [] and s["jobs"] == 0, "trashed on the other disk")

    go(trash / "files")
    s = a.wait(lambda s: sorted(s["names"]) == ["a.txt", "b.txt", "c.txt"], "every Trash folder in one list")
    a.shot("trash-view")
    pick("a.txt")
    a.key("F10")
    a.wait(lambda s: s["menu"])
    a.shot("trash-menu")
    a.key("Escape")
    # The menu opens on Open, then Preview, Copy, and Restore.
    menu_pick(3)
    a.wait(lambda s: sorted(s["names"]) == ["b.txt", "c.txt"] and s["jobs"] == 0, "a.txt restored")
    assert (fs / "home/a.txt").read_text() == "a"
    assert not (trash / "files/a.txt").exists() and not (trash / "info/a.txt.trashinfo").exists()
    pick("c.txt")
    menu_pick(3)
    s = a.wait(lambda s: s["names"] == ["b.txt"] and s["jobs"] == 0, "c.txt restored to its disk")
    assert s["notices"] == [], s["notices"]
    # Empty Trash is sixth, and asks first.
    pick("b.txt")
    menu_pick(5)
    a.wait(lambda s: s["dialog"] == "empty", "asks before emptying")
    a.shot("confirm-empty")
    a.key("Return")
    a.wait(lambda s: s["dialog"] == "", "enter takes the safe default")
    assert (trash / "files/b.txt").exists()
    # The toolbar's Empty Trash button, at the far right, asks the same.
    a.click(1220, 22)
    a.wait(lambda s: s["dialog"] == "empty", "the button asks too")
    a.type("e")
    a.wait(lambda s: s["names"] == [] and s["jobs"] == 0, "emptied")
    a.shot("trash-emptied")
    assert list((trash / "files").iterdir()) == [] and list((trash / "info").iterdir()) == []
    go(disk)
    a.wait(lambda s: s["names"] == ["c.txt"], "c.txt is back on its disk")
    a.close()


class PrivateBus:
    """A D-Bus session bus of our own, so nothing reaches the real one. Services in
    `services` are started by the bus itself, as on a desktop, with `env`."""

    def __init__(self, services=None, env=None):
        conf = SCRATCH / "bus.conf"
        sock = SCRATCH / "bus.sock"
        sdir = SCRATCH / "bus-services"
        shutil.rmtree(sdir, ignore_errors=True)
        sdir.mkdir()
        for name, text in (services or {}).items():
            (sdir / name).write_text(text)
        if sock.exists():
            sock.unlink()
        conf.write_text(f"""<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>session</type><listen>unix:path={sock}</listen><auth>EXTERNAL</auth>
<servicedir>{sdir}</servicedir>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy>
</busconfig>
""")
        self.proc = subprocess.Popen(["dbus-daemon", "--nofork", "--print-address=1", f"--config-file={conf}"], stdout=subprocess.PIPE, stderr=open(SCRATCH / "stderr.log", "a"), env=env)
        self.address = self.proc.stdout.readline().decode().strip()
        assert self.address, "dbus-daemon printed no address"

    def call(self, *args):
        return subprocess.run(["busctl", f"--address={self.address}", *args], capture_output=True, text=True, timeout=20)

    def stop(self):
        self.proc.kill()
        self.proc.wait()


FM1 = ["org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1"]


@check
def show_in_folder_starts_the_service_and_opens_the_items_selected(h):
    fs = fresh_fs()
    tree(fs, {"dl/a.txt": "a", "dl/b.txt": "b", "dl/.hidden": "h", "other/c": "c"})
    # Nobody owns the name, so the bus starts the service from its file, as a desktop's does.
    watch = App(h, None, launch=False)
    # Not --die-with-parent: dbus-daemon starts services through a helper that exits soon
    # after, which would take the service with it about one run in seven. The suite's
    # pkill clears it up instead.
    service = "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=" + " ".join(sandboxed(["--file-manager-service"], die_with_parent=False)) + "\n"
    bus = PrivateBus({"com.aliasproject.Gannet.FileManager1.service": service}, env=watch.env)
    try:
        uris = [f"file://{quote(str(fs / 'dl/b.txt'))}", f"file://{quote(str(fs / 'dl/.hidden'))}"]
        r = bus.call("call", *FM1, "ShowItems", "ass", "2", *uris, "")
        assert r.returncode == 0, r.stderr
        h.wait_window(10)
        s = watch.wait(lambda s: s["dir"] == str(fs / "dl") and sorted(s["selected"]) == [".hidden", "b.txt"], "the folder, with both selected")
        assert s["hidden"], "a hidden item asked for by name shows"
        watch.shot("show-in-folder")
        # What Gannet can't do is refused, rather than answered as if done.
        r = bus.call("call", *FM1, "ShowItemProperties", "ass", "1", uris[0], "")
        assert r.returncode != 0 and "properties" in r.stderr, r.stderr
        r = bus.call("call", *FM1, "ShowItems", "ass", "1", f"file://{fs}/dl/missing", "")
        assert r.returncode != 0, "nothing to show is an error"
    finally:
        bus.stop()


@check
def long_copies_show_progress_and_cancel(h):
    fs = fresh_fs()
    tree(fs, {"dst/x": ""})
    # A big sparse file on tmpfs takes real time to copy, so the panel has time to show.
    big = fs / "big.bin"
    with open(big, "wb") as f:
        f.truncate(4 << 30)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    a.type("big")
    a.wait(lambda s: s["selected"] == ["big.bin"])
    a.key("c", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    s = a.wait(lambda s: s["visible_jobs"] == 1, "progress panel shows", timeout=10)
    a.shot("progress")
    # The UI answers during the copy.
    a.key("h", mods=["ctrl"])
    a.wait(lambda s: s["hidden"], "keys work during a copy")
    a.key("period", mods=["ctrl"])
    s = a.wait(lambda s: s["jobs"] == 0, "cancelled", timeout=20)
    assert any("Stopped" in n for n in s["notices"]), s["notices"]
    leftovers = [p.name for p in (fs / "dst").iterdir()]
    assert leftovers == ["x"], leftovers
    a.close()


@check
def failures_are_visible(h):
    fs = fresh_fs()
    tree(fs, {"ok.txt": "fine", "locked.txt": "secret", "ro/x": ""})
    (fs / "locked.txt").chmod(0)
    (fs / "ro").chmod(0o500)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    a.key("a", mods=["ctrl"])
    a.key("c", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "ro"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "ro"))
    a.key("v", mods=["ctrl"])
    s = a.wait(lambda s: s["jobs"] == 0 and len(s["notices"]) >= 2, "each failure reported")
    a.shot("failures")
    assert any("denied" in n for n in s["notices"]), s["notices"]
    assert [p.name for p in (fs / "ro").iterdir()] == ["x"]
    # They stay until dismissed.
    time.sleep(0.5)
    n = len(a.state()["notices"])
    assert n >= 2
    a.key("Escape")
    a.wait(lambda s: len(s["notices"]) == n - 1, "escape dismisses one")
    a.close()
    (fs / "ro").chmod(0o755)
    (fs / "locked.txt").chmod(0o644)


@check
def closing_during_a_job_asks_first(h):
    fs = fresh_fs()
    tree(fs, {"dst/x": ""})
    with open(fs / "big.bin", "wb") as f:
        f.truncate(4 << 30)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    a.type("big")
    a.wait(lambda s: s["selected"] == ["big.bin"])
    a.key("c", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["jobs"] == 1, "running")
    a.key("q", mods=["ctrl"])
    a.wait(lambda s: s["dialog"] == "quit", "asks before closing")
    assert a.alive()
    a.key("Escape")
    a.wait(lambda s: s["dialog"] == "", "keep working")
    a.key("q", mods=["ctrl"])
    a.wait(lambda s: s["dialog"] == "quit")
    a.type("s")
    a.proc.wait(20)
    assert [p.name for p in (fs / "dst").iterdir()] == ["x"], "nothing half-copied stays"


@check
def keyboard_works_after_being_unplugged(h):
    fs = fresh_fs()
    tree(fs, {".h": "", "a": ""})
    # No held keyboard here: each wtype is a plug and an unplug, like a KVM switch.
    h2 = Headless("/tmp/gne2e-h3", keep_keyboard=False)
    try:
        a = App(h2, fs)
        a.wait(lambda s: s["count"] == 1)
        # The first keys of a fresh keyboard can beat the rebind, so send them until one lands.
        for want in (True, False):
            end = time.monotonic() + 6
            while a.state().get("hidden") != want and time.monotonic() < end:
                a.key("h", mods=["ctrl"])
                time.sleep(0.2)
            assert a.state().get("hidden") == want, f"key ignored after replug, wanted hidden={want}"
    finally:
        a.close()
        h2.stop()


@check
def regressions_from_review(h):
    fs = fresh_fs()
    for i in range(300):
        (fs / f"a{i:03}").touch()
    tree(fs, {"z-small/one": "", "z-small/two": "", "z-small/three": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 301)
    # A stale selection anchor used to crash on Shift in a smaller folder.
    a.key("End")
    a.wait(lambda s: s["cursor"] == 300, "at the bottom")
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "z-small"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "z-small") and s["count"] == 3)
    a.key("Down", mods=["shift"])
    time.sleep(0.2)
    assert a.alive(), "shift in a smaller folder must not crash"
    s = a.state()
    assert len(s["selected"]) <= 2, s["selected"]
    a.key("BackSpace")
    a.wait(lambda s: s["dir"] == str(fs))
    # A folder change mustn't show a cursor or move the view.
    a.close()
    a = App(h, fs)
    s = a.wait(lambda s: s["count"] == 301 and not s["cursor_shown"])
    a.move(ROW_X, row_y(3))
    a.scroll(20)
    s = a.wait(lambda s: s["scroll"] > 1000, "scrolled")
    at = s["scroll"]
    (fs / "a000-new").touch()
    a.wait(lambda s: s["count"] == 302, "change seen")
    time.sleep(0.2)
    s = a.state()
    assert s["scroll"] == at and not s["cursor_shown"], f"view moved: {s['scroll']} from {at}, cursor {s['cursor_shown']}"
    # Going in and back with no cursor selects nothing.
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "z-small"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "z-small"))
    a.key("Left", mods=["alt"])
    s = a.wait(lambda s: s["dir"] == str(fs))
    assert s["selected"] == [], s["selected"]
    # A replaced folder: F5 finds the new one, and changes in it are noticed.
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "z-small"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "z-small") and s["count"] == 3)
    shutil.rmtree(fs / "z-small")
    (fs / "z-small").mkdir()
    a.key("F5")
    a.wait(lambda s: s["count"] == 0 and not s["loading"], "reloaded")
    time.sleep(0.1)
    (fs / "z-small/appeared").touch()
    a.wait(lambda s: s["names"] == ["appeared"], "watch follows the new folder")
    a.close()


@check
def repeated_letters_walk_and_the_top_row_counts(h):
    fs = fresh_fs()
    tree(fs, {"apple": "", "avocado": "", "banana": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    a.type("a")
    a.wait(lambda s: s["selected"] == ["apple"], "the top row counts when no cursor shows")
    a.type("a")
    a.wait(lambda s: s["selected"] == ["avocado"], "same letter goes on to the next")
    a.type("a")
    a.wait(lambda s: s["selected"] == ["apple"], "and wraps")
    a.close()


@check
def slow_reloads_keep_things_steady(h):
    fs = fresh_fs()
    (fs / "big").mkdir()
    for i in range(3000):
        (fs / "big" / f"f{i:04}").touch()
    # Named on the command line, and not read yet when the first rows arrive.
    a = App(h, fs / "big" / "f0005", {"GANNET_SLOW_READ_MS": "15"})
    s = a.wait(lambda s: s["dir"] == str(fs / "big") and s["selected"] == ["f0005"] and not s["loading"], "named file gets the cursor", timeout=15)
    # A reload keeps showing everything while it reads.
    (fs / "big/new").touch()
    seen_partial = False
    end = time.monotonic() + 8
    while time.monotonic() < end:
        s = a.state()
        if s.get("loading"):
            seen_partial = True
            assert s["count"] >= 3000 and s["selected"] == ["f0005"], f"reload shrank the view: {s['count']}, {s['selected']}"
        if seen_partial and not s.get("loading") and s.get("count") == 3001:
            break
        time.sleep(0.005)
    assert a.state()["count"] == 3001 and a.state()["selected"] == ["f0005"]
    a.close()
    # Constant writes can't keep a slow folder from ever finishing.
    a = App(h, fs / "big", {"GANNET_SLOW_READ_MS": "15"})
    stop = time.monotonic() + 12
    done = False
    n = 0
    while time.monotonic() < stop and not done:
        (fs / "big" / f"w{n % 3}").write_text(str(n))
        n += 1
        time.sleep(0.02)
        s = a.state()
        done = bool(s) and not s["loading"] and s["count"] >= 3000
    assert done, "never settled under constant writes"
    a.close()


@check
def odd_arguments(h):
    fs = fresh_fs()
    tree(fs, {"folder/file.txt": ""})
    # A file: its folder, with the file selected.
    a = App(h, fs / "folder/file.txt")
    a.wait(lambda s: s["dir"] == str(fs / "folder") and s["selected"] == ["file.txt"], "file argument")
    a.close()
    # A folder name that isn't UTF-8.
    raw = str(fs).encode() + b"/caf\xe9"
    os.mkdir(raw)
    a = App(h, raw.decode("utf-8", "surrogateescape"))
    a.wait(lambda s: s["count"] == 0 and not s["loading"] and not s["error"], "non-UTF-8 argument")
    assert a.alive()
    a.close()


@check
def f10_in_a_tiny_window(h):
    fs = fresh_fs()
    tree(fs, {"a": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1)
    a.sway("output HEADLESS-1 resolution 200x60")
    time.sleep(0.3)
    a.key("F10")
    time.sleep(0.3)
    assert a.alive(), "F10 in a tiny window"
    a.key("Escape")
    a.sway("output HEADLESS-1 resolution 1280x800")
    time.sleep(0.2)
    assert a.alive()
    a.close()


def png_bytes(w, h, rgb=(40, 120, 200), text=None, exif=None, claim=None):
    """A real PNG, made by hand so these checks need nothing installed. `rgb` may be a function
    of the row, for a picture with something in it."""
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    rows = bytearray()
    for y in range(h):
        c = rgb(y) if callable(rgb) else rgb
        rows += b"\0" + bytes(c) * w
    out = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", *(claim or (w, h)), 8, 2, 0, 0, 0))
    for key, value in (text or {}).items():
        out += chunk(b"tEXt", key.encode() + b"\0" + str(value).encode())
    if exif:
        out += chunk(b"eXIf", exif)
    return out + chunk(b"IDAT", zlib.compress(bytes(rows), 1)) + chunk(b"IEND", b"")


def write_images(fs):
    def bands(y):
        return (200, 60, 60) if y % 200 < 100 else (60, 60, 200)
    (fs / "big.png").write_bytes(png_bytes(3000, 2000, bands))
    (fs / "small.png").write_bytes(png_bytes(40, 20))
    good = png_bytes(64, 64)
    (fs / "broken.png").write_bytes(good[:len(good) // 2])
    (fs / "claims.png").write_bytes(png_bytes(64, 64, claim=(60000, 60000)))
    (fs / "notes.txt").write_text("not a picture")


def preview_of(s):
    return s["preview"] or {}


def go_to(a, name):
    """Puts the cursor on a file by typing its name, which is also how it's used."""
    a.type(name.split(".")[0])
    return a.wait(lambda s: s["selected"] == [name], f"cursor on {name}")


@check
def space_previews_images_and_walks_the_folder(h):
    fs = fresh_fs()
    write_images(fs)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 5, "listing")
    go_to(a, "big.png")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "image", "the big image")
    assert (preview_of(s)["w"], preview_of(s)["h"]) == (3000, 2000), s["preview"]
    assert preview_of(s)["size"] == (fs / "big.png").stat().st_size
    a.wait(lambda s: preview_of(s)["scaled"], "the smooth copy for fitting it to the popup")
    a.shot("preview-big")
    # On through the folder, including the ones that can't be shown.
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "broken.png" and preview_of(s)["state"] == "message", "a damaged image")
    assert "damaged" in preview_of(s)["message"], s["preview"]
    assert s["selected"] == ["broken.png"]
    a.shot("preview-message")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "claims.png" and preview_of(s)["state"] == "message", "a huge image")
    assert "too large" in preview_of(s)["message"], s["preview"]
    a.key("Right")
    s = a.wait(lambda s: preview_of(s).get("name") == "notes.txt" and preview_of(s)["state"] == "text", "a text file")
    assert preview_of(s)["kind"] == "text/plain", s["preview"]
    a.key("Right")
    s = a.wait(lambda s: preview_of(s).get("name") == "small.png" and preview_of(s)["state"] == "image", "a small image")
    assert not preview_of(s)["scaled"], "shown at its own size, so nothing to shrink"
    a.key("Up")
    a.wait(lambda s: preview_of(s).get("name") == "notes.txt", "back again")
    # The list's cursor followed the window, and moving it moves the window.
    s = a.state()
    assert s["selected"] == ["notes.txt"], s["selected"]
    a.focus_list()
    a.key("Down")
    a.wait(lambda s: s["selected"] == ["small.png"] and preview_of(s).get("name") == "small.png", "the window follows the list")
    a.key("Up")
    a.wait(lambda s: preview_of(s).get("name") == "notes.txt", "and back")
    # Space in the list closes it, as Space and Escape in the window do, and none of them
    # touches the selection or anything else.
    a.key("space")
    s = a.wait(lambda s: s["preview"] is None, "Space in the list closes")
    assert s["selected"] == ["notes.txt"]
    a.key("space")
    a.wait(lambda s: s["preview"], "open again")
    a.key("space")
    s = a.wait(lambda s: s["preview"] is None, "Space in the window closes")
    a.key("space")
    a.wait(lambda s: s["preview"], "open again")
    a.key("Escape")
    s = a.wait(lambda s: s["preview"] is None, "Escape closes")
    assert s["selected"] == ["notes.txt"], "Escape only closed the window"
    assert sorted(p.name for p in fs.iterdir()) == ["big.png", "broken.png", "claims.png", "notes.txt", "small.png"]
    assert a.alive()
    a.close()


def preview_pid(a):
    for pid in hl_tree(a.proc.pid):
        try:
            if Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[1:2] == [b"--preview"]:
                return pid
        except OSError:
            pass
    return None


@check
def any_file_says_what_it_is_and_opens_in_its_app(h):
    fs = fresh_fs()
    (fs / "a-archive.zip").write_bytes(b"PK\x03\x04" + b"\0" * 100)
    (fs / "b-no-extension").write_bytes(b"PK\x03\x04" + b"\0" * 100)
    (fs / "c-blob.xyz").write_bytes(bytes(range(256)))
    bin_dir = SCRATCH / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    log = SCRATCH / "opened.log"
    log.unlink(missing_ok=True)
    opener = bin_dir / "xdg-open"
    opener.write_text(f'#!/bin/sh\necho "$1" >> {log}\ncase "$1" in *.xyz) exit 3;; esac\nexit 0\n')
    opener.chmod(0o755)
    a = App(h, fs, path_prefix=str(bin_dir), extra_env={"LANG": "C.UTF-8"})
    a.wait(lambda s: s["count"] == 3, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "info", "the archive")
    assert preview_of(s)["kind"] == "application/zip" and preview_of(s)["message"] == "Zip archive", s["preview"]
    a.shot("preview-any-file")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "b-no-extension" and preview_of(s)["state"] == "info", "known by its contents")
    assert preview_of(s)["kind"] == "application/zip", s["preview"]
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "c-blob.xyz" and preview_of(s)["state"] == "info", "unknown bytes")
    assert preview_of(s)["kind"] == "application/octet-stream", s["preview"]
    # Enter, or the Open button, opens it in its app and leaves the preview up.
    a.key("Up", "Up")
    a.wait(lambda s: preview_of(s).get("name") == "a-archive.zip", "back to the archive")
    a.key("Return")
    end = time.monotonic() + 5
    while time.monotonic() < end and not log.exists():
        time.sleep(0.02)
    assert log.read_text().strip() == str(fs / "a-archive.zip"), log.read_text()
    assert a.state()["preview"], "still previewing"
    a.key("Down", "Down")
    a.wait(lambda s: preview_of(s).get("name") == "c-blob.xyz" and preview_of(s)["state"] == "info", "the blob")
    x, y, w, h_ = a.preview_window()
    # The button sits below the picture and two lines of words, in the middle.
    a.click(x + w // 2, y + 533)
    a.wait(lambda s: json.loads(a.preview_file.read_text())["notices"], "a failure shows in the window", timeout=5)
    assert "c-blob.xyz" in log.read_text()
    a.shot("preview-open-failed")
    a.close()


def ffmpeg(*args):
    subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-y", *map(str, args)], check=True, capture_output=True)


def doc_of(s):
    return preview_of(s).get("doc") or {}


@check
def text_and_markdown_show_as_documents(h):
    fs = fresh_fs()
    (fs / "a-notes.txt").write_bytes(b"first line\r\nsecond\tline\r\n" + "日本語のテキスト\n".encode() + b"x" * 400 + b"\n")
    (fs / "b-readme.md").write_text("# Gannet\n\nA *fast* file manager with **bold** ideas and `code`.\n\n- one\n- two\n  1. nested\n\n> A quote.\n\n```\nfn main() {}\n```\n\n---\n\nThe end, see [the site](https://example.com).\n")
    (fs / "c-long.log").write_bytes(b"".join(b"%06d a line of a long log file\n" % i for i in range(6000)))
    (fs / "d-binary.dat").write_bytes(b"MZ\0\0" + bytes(range(256)) * 4)
    (fs / "e-late-zero.txt").write_bytes(b"text " * 2000 + b"\0" + b"more")
    a = App(h, fs, extra_env={"LANG": "C.UTF-8"})
    a.wait(lambda s: s["count"] == 5, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "text" and doc_of(s).get("lines", 0) > 0, "the text, laid out")
    d = doc_of(s)
    assert d["text"].startswith("first line\nsecond\tline\n"), d
    assert d["lines"] >= 5, f"the long line wraps: {d}"
    assert not d["cut"], d
    a.shot("preview-text")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "b-readme.md" and preview_of(s)["state"] == "markdown" and doc_of(s).get("lines", 0) > 0, "the Markdown")
    assert preview_of(s)["kind"] == "text/markdown", s["preview"]
    assert doc_of(s)["text"].startswith("GannetA "), doc_of(s)
    a.shot("preview-markdown")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "c-long.log" and doc_of(s).get("lines", 0) > 0, "the long file")
    d = doc_of(s)
    assert d["cut"] and d["height"] > 20000, f"only the first 128 kB: {d}"
    assert d["scroll"] == 0
    # Page Down, the wheel and End scroll it; the arrows still go to other files.
    a.key("Next")
    s = a.wait(lambda s: doc_of(s).get("scroll", 0) > 300, "a page down")
    paged = doc_of(s)["scroll"]
    x, y, w, h_ = a.preview_window()
    a.move(x + w // 2, y + h_ // 2)
    a.scroll(3)
    s = a.wait(lambda s: doc_of(s).get("scroll", 0) > paged, "the wheel")
    a.key("End")
    s = a.wait(lambda s: doc_of(s)["scroll"] >= doc_of(s)["height"] - h_, "the end")
    a.shot("preview-text-end")
    a.key("Home")
    a.wait(lambda s: doc_of(s)["scroll"] == 0, "back to the top")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "d-binary.dat" and preview_of(s)["state"] == "info", "binary isn't text")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "e-late-zero.txt" and preview_of(s)["state"] == "text", "a zero far in is still text")
    assert a.alive()
    a.close()


@check
def animations_move(h):
    fs = fresh_fs()
    ffmpeg("-f", "lavfi", "-i", "testsrc=size=120x90:rate=10", "-t", "1", fs / "a-moving.gif")
    ffmpeg("-f", "lavfi", "-i", "testsrc=size=120x90:rate=10", "-t", "1", "-plays", "0", "-f", "apng", fs / "b-moving.png")
    ffmpeg("-f", "lavfi", "-i", "testsrc=size=120x90:rate=10", "-t", "1", "-c:v", "libwebp_anim", "-loop", "0", fs / "c-moving.webp")
    (fs / "d-still.png").write_bytes(png_bytes(40, 30))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 4, "listing")
    a.key("space")
    for name in ("a-moving.gif", "b-moving.png", "c-moving.webp"):
        s = a.wait(lambda s: preview_of(s).get("name") == name and preview_of(s)["state"] == "image", name)
        assert preview_of(s)["frames"] == 10 and (preview_of(s)["w"], preview_of(s)["h"]) == (120, 90), s["preview"]
        seen = {preview_of(a.state())["frame"]}
        end = time.monotonic() + 3
        while len(seen) < 4 and time.monotonic() < end:
            seen.add(preview_of(a.state())["frame"])
            time.sleep(0.02)
        assert len(seen) >= 4, f"{name} should move: {seen}"
        a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "d-still.png" and preview_of(s)["state"] == "image", "a still one")
    assert preview_of(s)["frames"] == 1, s["preview"]
    a.close()


@check
def sound_plays_with_its_controls(h):
    fs = fresh_fs()
    ffmpeg("-f", "lavfi", "-i", "sine=frequency=440:duration=20", "-c:a", "libmp3lame", fs / "a-tone.mp3")
    ffmpeg("-f", "lavfi", "-i", "sine=frequency=330:duration=20", "-c:a", "flac", fs / "b-tone.flac")
    ffmpeg("-f", "lavfi", "-i", "sine=frequency=220:duration=20", "-c:a", "libvorbis", fs / "c-tone.ogg")
    a = App(h, fs, extra_env={"LANG": "C.UTF-8"})
    a.wait(lambda s: s["count"] == 3, "listing")
    a.key("space")
    for name, kind in (("a-tone.mp3", "audio/mpeg"), ("b-tone.flac", "audio/flac"), ("c-tone.ogg", "audio/")):
        s = a.wait(lambda s: preview_of(s).get("name") == name and preview_of(s)["state"] == "sound" and video_of(s).get("position", 0) > 0.3, f"{name} playing", timeout=10)
        assert preview_of(s)["kind"].startswith(kind), s["preview"]
        assert abs(video_of(s)["duration"] - 20) < 1, video_of(s)
        if name == "a-tone.mp3":
            a.shot("preview-sound")
            a.key("p")
            s = a.wait(lambda s: video_of(s).get("paused"), "paused")
            at = video_of(s)["position"]
            time.sleep(0.5)
            assert abs(video_of(a.state())["position"] - at) < 0.05, "it stays paused"
        a.key("Down")
    assert a.alive()
    a.close()


@check
def a_folder_says_what_it_holds(h):
    fs = fresh_fs()
    tree(fs, {"box/a": "x" * 1000, "box/.hidden": "y" * 10, "box/inner/b": "z" * 5000, "box/inner/deeper/c": "w" * 100, "empty": None})
    a = App(h, fs, extra_env={"LANG": "C.UTF-8"})
    a.wait(lambda s: s["count"] == 2, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("name") == "box" and (preview_of(s).get("folder") or {}).get("done"), "counted")
    assert preview_of(s)["folder"] == {"items": 3, "bytes": 6110, "done": True, "incomplete": False}, s["preview"]
    assert preview_of(s)["kind"] == "inode/directory", s["preview"]
    a.shot("preview-folder")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "empty" and (preview_of(s).get("folder") or {}).get("done"), "the empty one")
    assert preview_of(s)["folder"]["items"] == 0
    a.close()


def make_pdf(path, pages, password=None):
    ps = path.with_suffix(".ps")
    ps.write_text("%!PS\n/Helvetica findfont 48 scalefont setfont\n1 1 " + str(pages) + " { /n exch def 72 650 moveto (Page ) show n 4 string cvs show showpage } for\n")
    extra = [f"-sOwnerPassword=o{password}", f"-sUserPassword={password}"] if password else []
    subprocess.run(["ps2pdf", *extra, str(ps), str(path)], check=True, capture_output=True)
    ps.unlink()


def plain_pdf(path, rotate=0):
    """A one-page PDF written by hand, all in plain ASCII, as some programs write them."""
    content = b"BT /F1 24 Tf 72 700 Td (Hello) Tj ET"
    objs = [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /CropBox [0 0 300 400] /Rotate %d /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>" % rotate,
            b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream", b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    out, offsets = b"%PDF-1.4\n", []
    for i, o in enumerate(objs):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % (i + 1) + o + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1) + b"".join(b"%010d 00000 n \n" % o for o in offsets)
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    path.write_bytes(out)


def pdf_of(s):
    return preview_of(s).get("pdf") or {}


def drawn(s):
    """The pages drawn, with whether each worked."""
    return {i: ok for i, _, ok in pdf_of(s).get("drawn", [])}


@check
def pdfs_draw_their_pages_as_they_scroll(h):
    fs = fresh_fs()
    make_pdf(fs / "a-long.pdf", 12)
    make_pdf(fs / "b-locked.pdf", 2, password="secret")
    make_pdf(fs / "c-broken.pdf", 3)
    data = (fs / "c-broken.pdf").read_bytes()
    (fs / "c-broken.pdf").write_bytes(data[: len(data) // 3])
    plain_pdf(fs / "d-plain.pdf")
    plain_pdf(fs / "e-turned.pdf", rotate=90)
    a = App(h, fs, extra_env={"LANG": "C.UTF-8"})
    a.wait(lambda s: s["count"] == 5, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "pdf" and drawn(s).get(0), "the first page", timeout=10)
    assert pdf_of(s)["pages"] == 12, s["preview"]
    assert preview_of(s)["kind"] == "application/pdf"
    assert not drawn(s).get(8), "pages far down aren't drawn yet"
    a.shot("preview-pdf")
    a.key("End")
    s = a.wait(lambda s: drawn(s).get(11), "the last page, once scrolled to", timeout=10)
    assert 0 not in drawn(s), f"pages far up are let go: {pdf_of(s)}"
    a.shot("preview-pdf-end")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "b-locked.pdf" and preview_of(s)["state"] == "message", "locked", timeout=10)
    assert "password" in preview_of(s)["message"], s["preview"]
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "c-broken.pdf" and preview_of(s)["state"] == "message", "broken", timeout=10)
    assert "can't be read" in preview_of(s)["message"], s["preview"]
    # All plain words, and cropped: still a PDF, not text.
    for name in ("d-plain.pdf", "e-turned.pdf"):
        a.key("Down")
        s = a.wait(lambda s: preview_of(s).get("name") == name and drawn(s).get(0), name, timeout=10)
        assert preview_of(s)["state"] == "pdf", s["preview"]
    a.shot("preview-pdf-turned")
    assert a.alive()
    a.close()


@check
def thumbnailers_show_what_nothing_else_can(h):
    fs = fresh_fs()
    (fs / "a-drawing.svg").write_text('<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200"><rect width="300" height="200" fill="#c33"/><circle cx="150" cy="100" r="70" fill="#fff"/></svg>')
    (fs / "src.png").write_bytes(png_bytes(320, 240, rgb=(30, 160, 90)))
    subprocess.run(["cjxl", "--quiet", str(fs / "src.png"), str(fs / "b-photo.jxl")], check=True, capture_output=True)
    subprocess.run(["heif-enc", "-q", "50", "-o", str(fs / "c-photo.heic"), str(fs / "src.png")], check=True, capture_output=True)
    (fs / "src.png").unlink()
    # An office document carries its own picture, which is what its thumbnailer shows.
    import zipfile
    with zipfile.ZipFile(fs / "d-letter.odt", "w") as z:
        z.writestr(zipfile.ZipInfo("mimetype"), "application/vnd.oasis.opendocument.text")
        z.writestr("META-INF/manifest.xml", '<?xml version="1.0"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/></manifest:manifest>')
        z.writestr("Thumbnails/thumbnail.png", png_bytes(181, 256, rgb=(200, 200, 60)))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 4, "listing")
    a.key("space")
    for name, w, h_, by in (("a-drawing.svg", 300, 200, "glycin-thumbnailer"), ("b-photo.jxl", 320, 240, "glycin-thumbnailer"), ("c-photo.heic", 320, 240, "glycin-thumbnailer"), ("d-letter.odt", 181, 256, "gsf-office-thumbnailer")):
        s = a.wait(lambda s: preview_of(s).get("name") == name and preview_of(s)["state"] in ("image", "info", "message"), name, timeout=15)
        p = preview_of(s)
        assert p["state"] == "image" and p["by"] == by, p
        assert abs(p["w"] * h_ - p["h"] * w) <= max(w, h_), f"{name} keeps its shape: {p}"
        if name == "a-drawing.svg":
            a.shot("preview-svg")
        a.key("Down")
    a.close()


@check
def a_crash_in_the_preview_closes_only_the_preview_and_closing_the_list_closes_both(h):
    fs = fresh_fs()
    tree(fs, {"a": "x", "b": "y"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2, "listing")
    a.key("space")
    a.wait(lambda s: s["preview"], "the preview window")
    pid = preview_pid(a)
    assert pid, "a process of its own"
    os.kill(pid, signal.SIGKILL)
    s = a.wait(lambda s: s["previewing"] is None, "the list notices")
    assert any("stopped unexpectedly" in n for n in s["notices"]), s["notices"]
    assert a.alive()
    a.focus_list()
    a.key("space")
    a.wait(lambda s: s["preview"], "a new one opens")
    pid = preview_pid(a)
    a.focus_list()
    a.key("q", mods=["ctrl"])
    a.proc.wait(5)
    end = time.monotonic() + 3
    while time.monotonic() < end and Path(f"/proc/{pid}").exists():
        time.sleep(0.02)
    assert not Path(f"/proc/{pid}").exists(), "the preview went with the list"


@check
def ctrl_space_selects_and_typing_still_jumps(h):
    fs = fresh_fs()
    tree(fs, {"apple": "", "my file": "", "pear": ""})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3, "listing")
    a.key("space", mods=["ctrl"])
    s = a.wait(lambda s: s["selected"] == ["apple"], "Ctrl+Space selects")
    assert s["preview"] is None
    a.key("Down", mods=["ctrl"])
    a.key("Down", mods=["ctrl"])
    a.key("space", mods=["ctrl"])
    s = a.wait(lambda s: s["selected"] == ["apple", "pear"], "and picks a second, skipping the middle")
    assert s["preview"] is None
    a.key("space", mods=["ctrl"])
    a.wait(lambda s: s["selected"] == ["apple"], "and unpicks it")
    a.type("pe")
    a.wait(lambda s: s["selected"] == ["pear"], "letters still jump to a name")
    # Select all, then look at something: the selection survives both opening and walking.
    a.key("Escape")
    a.key("a", mods=["ctrl"])
    a.wait(lambda s: len(s["selected"]) == 3, "select all")
    a.key("space")
    a.wait(lambda s: s["preview"], "popup")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") != "pear" or s["cursor"] != 0, "walking")
    assert len(s["selected"]) == 3, s["selected"]
    a.close()


@check
def zooming_and_panning_a_preview(h):
    fs = fresh_fs()
    (fs / "wide.png").write_bytes(png_bytes(4000, 3000, lambda y: (y % 256, 80, 160)))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    a.wait(lambda s: preview_of(s).get("state") == "image" and preview_of(s)["scaled"], "fitted")
    # Fit is the floor: smaller than that shows nothing more.
    a.key("minus")
    time.sleep(0.05)
    assert preview_of(a.state())["zoom"] is None
    a.key("plus")
    s = a.wait(lambda s: preview_of(s)["zoom"] is not None, "zoomed in")
    first = preview_of(s)["zoom"]
    a.key("plus")
    a.wait(lambda s: preview_of(s)["zoom"] > first, "further in")
    for _ in range(9):
        a.key("plus")
    s = a.wait(lambda s: preview_of(s)["zoom"] >= 1, "past its own size")
    a.wait(lambda s: not preview_of(s)["scaled"], "drawn from the real pixels at this size")
    a.shot("preview-zoomed")
    # Shift+arrows pan, and don't move to another file.
    a.key("Right", mods=["shift"])
    s = a.wait(lambda s: preview_of(s)["pan"][0] < 0, "panned with the keyboard")
    assert preview_of(s)["name"] == "wide.png"
    pan = preview_of(s)["pan"][0]
    # The mouse drags it, and a click on it doesn't close it.
    x0, y0, _, _ = a.preview_window()
    a.move(x0 + 480, y0 + 360)
    h.pointer("button 272 1")
    a.move(x0 + 580, y0 + 360)
    s = a.wait(lambda s: preview_of(s)["pan"][0] > pan, "dragged")
    h.pointer("button 272 0")
    time.sleep(0.03)
    pan = preview_of(a.state())["pan"][0]
    a.move(x0 + 740, y0 + 360)
    time.sleep(0.05)
    assert preview_of(a.state())["pan"][0] == pan, "the drag ended with the button"
    a.click(x0 + 480, y0 + 360)
    time.sleep(0.05)
    assert a.state()["preview"], "a click on the window leaves it open"
    a.key("0")
    a.wait(lambda s: preview_of(s)["zoom"] is None and preview_of(s)["pan"] == [0, 0] and preview_of(s)["scaled"], "0 fits again")
    # The mouse's back and forward buttons step through the folder.
    (a.fs / "x.png").write_bytes(png_bytes(40, 40))
    a.wait(lambda s: s["count"] == 2, "a second file")
    a.click(x0 + 480, y0 + 360, button=0x114)
    a.wait(lambda s: preview_of(s).get("name") == "x.png", "forward")
    a.click(x0 + 480, y0 + 360, button=0x113)
    a.wait(lambda s: preview_of(s).get("name") == "wide.png", "back")
    a.close()


@check
def an_open_preview_follows_a_theme_swap(h):
    fs = fresh_fs()
    client = SCRATCH / "client"
    if client.exists():
        shutil.rmtree(client)
    (fs / "a.png").write_bytes(png_bytes(200, 100))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    a.wait(lambda s: preview_of(s).get("state") == "image", "open")
    cur = client / "home/.local/state/aliasos/current"
    cur.mkdir(parents=True)
    next_dir = cur / "next-theme"
    next_dir.mkdir()
    (next_dir / "colors.toml").write_text('background = "#112233"\nforeground = "#ddeeff"\naccent = "#ff8800"\n')
    os.rename(next_dir, cur / "theme")
    s = a.wait(lambda s: s["base"] == 0x112233, "the theme arrives")
    assert preview_of(s).get("state") == "image", "and the window stays open through it"
    a.wait(lambda s: json.loads(a.preview_file.read_text())["base"] == 0x112233, "in the preview window too")
    a.shot("preview-dark")
    a.close()


@check
def the_preview_fills_its_window_through_a_folder(h):
    fs = fresh_fs()
    (fs / "a-wide.png").write_bytes(png_bytes(2000, 500))
    (fs / "b-tall.png").write_bytes(png_bytes(500, 2000))
    (fs / "c-small.png").write_bytes(png_bytes(40, 20))
    (fs / "d-notes.txt").write_text("not a picture")
    a = App(h, fs, extra_env={"GANNET_SLOW_PREVIEW_MS": "300"})
    a.wait(lambda s: s["count"] == 4, "listing")
    rect = lambda s: preview_of(s)["rect"]
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "image", "wide image")
    assert rect(s) == [0, 0, 960, 720], "the whole window"
    assert a.preview_window()[2:] == (960, 720), "as the window rule sized it"
    a.shot("static-wide")
    for name, state in (("b-tall.png", "image"), ("c-small.png", "image"), ("d-notes.txt", "text")):
        a.key("Down")
        s = a.wait(lambda s: preview_of(s).get("name") == name, f"moved to {name}")
        s = a.wait(lambda s: preview_of(s)["state"] == state, name)
        assert rect(s) == [0, 0, 960, 720], f"showing {name}"
        a.shot(f"static-{name.split('.')[0]}")
    # The window can be resized, and what's in it follows.
    a.resize_preview(640, 480)
    a.wait(lambda s: rect(s) == [0, 0, 640, 480], "resized")
    a.close()


@check
def a_job_can_be_stopped_while_previewing(h):
    fs = fresh_fs()
    tree(fs, {"dst/x": ""})
    with open(fs / "big.bin", "wb") as f:
        f.truncate(4 << 30)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    a.type("big")
    a.wait(lambda s: s["selected"] == ["big.bin"])
    a.key("c", mods=["ctrl"])
    time.sleep(0.1)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "dst"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "dst"))
    a.key("v", mods=["ctrl"])
    a.wait(lambda s: s["jobs"] == 1, "the copy starts", timeout=10)
    a.key("space")
    a.wait(lambda s: s["preview"], "the preview window")
    # The list is still there to use.
    a.focus_list()
    a.key("period", mods=["ctrl"])
    s = a.wait(lambda s: s["jobs"] == 0, "the copy ends", timeout=20)
    assert not (fs / "dst/big.bin").exists(), "Ctrl+. stopped it, rather than it finishing"
    assert s["preview"], "and the preview stays"
    a.close()


def make_video(path, seconds=6, size="1280x720", rate=30, audio=True):
    cmd = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i", f"testsrc2=size={size}:rate={rate}"]
    if audio:
        cmd += ["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]
    cmd += ["-t", str(seconds), "-c:v", "libx264", "-preset", "ultrafast"]
    if audio:
        cmd += ["-c:a", "aac"]
    subprocess.run(cmd + [str(path)], check=True, capture_output=True)


def video_of(s):
    return preview_of(s).get("video") or {}


@check
def a_video_plays_in_the_popup_and_answers_its_controls(h):
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=20)
    (fs / "next.png").write_bytes(png_bytes(40, 20))
    log = SCRATCH / "bench.log"
    a = App(h, fs, extra_env={"GANNET_BENCH_LOG": str(log)})
    a.wait(lambda s: s["count"] == 2, "listing")
    go_to(a, "clip.mp4")
    before = log.read_text().count("\n")
    started = time.monotonic()
    a.key("space")
    s = a.wait(lambda s: video_of(s).get("shown", 0) >= 1, "the first frame", timeout=10)
    print(f"      first frame after {(time.monotonic() - started) * 1000:.0f} ms", flush=True)
    assert preview_of(s)["state"] == "video"
    # It's moving, at about the right speed.
    first = a.wait(lambda s: video_of(s).get("position", 0) > 0.3, "playback")
    time.sleep(2)
    later = a.state()
    played = video_of(later)["position"] - video_of(first)["position"]
    shown = video_of(later)["shown"] - video_of(first)["shown"]
    assert 1.5 < played < 2.6, played
    assert shown > played * 30 * 0.85, f"{shown} frames in {played:.2f} s"
    assert abs(video_of(later)["duration"] - 20) < 0.5
    assert preview_of(later)["w"] > 0 and preview_of(later)["w"] * 720 == preview_of(later)["h"] * 1280 or abs(preview_of(later)["w"] / preview_of(later)["h"] - 16 / 9) < 0.01, later["preview"]
    a.shot("video-playing")
    busy = [int(l.split()[1]) for l in log.read_text().splitlines()[before:] if l.startswith("busy ")]
    worst = max(busy, default=0)
    print(f"      longest UI stall during playback: {worst / 1000:.1f} ms", flush=True)
    assert worst < 16_000, f"the UI stalled for {worst} us"
    # The keyboard: pause and resume.
    a.key("p")
    s = a.wait(lambda s: video_of(s)["paused"], "paused")
    held = a.wait(lambda s: True)
    time.sleep(0.2)
    shown_then = video_of(a.state())["shown"]
    time.sleep(0.4)
    assert video_of(a.state())["shown"] == shown_then, "no frames while paused"
    a.shot("video-paused")
    a.key("Return")
    a.wait(lambda s: not video_of(s)["paused"] and video_of(s)["shown"] > shown_then, "playing again")
    # Seeking and volume with the keyboard.
    at = video_of(a.state())["position"]
    a.key("Right", mods=["shift"])
    a.wait(lambda s: video_of(s)["position"] > at + 3, "seeked forward")
    assert preview_of(a.state())["name"] == "clip.mp4", "Shift+arrow doesn't change file"
    a.key("Down", mods=["shift"])
    a.wait(lambda s: video_of(s)["volume"] == 95, "volume down")
    a.key("Up", mods=["shift"])
    a.wait(lambda s: video_of(s)["volume"] == 100, "volume up")
    a.key("m")
    a.wait(lambda s: video_of(s)["muted"], "muted")
    a.key("m")
    a.wait(lambda s: not video_of(s)["muted"], "unmuted")
    # The mouse: the frame is the whole window, with the footer at the bottom.
    wx, wy, _, _ = a.preview_window()
    x, y, w, h_ = preview_of(a.state())["rect"]
    x, y = x + wx, y + wy
    foot = y + h_ - 16
    a.click(x + w // 2, y + h_ // 2)
    a.wait(lambda s: video_of(s)["paused"], "a click on the picture pauses")
    a.click(x + 23, foot)
    a.wait(lambda s: not video_of(s)["paused"], "the play button resumes")
    a.click(x + 160, foot)
    a.wait(lambda s: video_of(s)["position"] < 8, "a click near the start of the bar seeks there")
    a.click(x + w - 10, foot)
    a.wait(lambda s: video_of(s)["volume"] > 90, "the end of the volume bar")
    a.click(x + w - 80 + 35, foot)
    a.wait(lambda s: 30 < video_of(s)["volume"] < 70, "the middle of the volume bar")
    a.click(x + w - 80 - 15, foot)
    a.wait(lambda s: video_of(s)["muted"], "the speaker mutes")
    # Going to the next file stops it, and closing frees everything.
    a.key("Down")
    a.wait(lambda s: preview_of(s).get("name") == "next.png" and preview_of(s)["state"] == "image", "next file")
    end = time.monotonic() + 3
    while players(a) and time.monotonic() < end:
        time.sleep(0.01)
    assert not players(a), "moving on stops the player"
    assert mapped_video(a.proc.pid) == 0, "and hands its memory back"
    # A resize mid-play asks for frames of the new size.
    a.key("Up")
    a.wait(lambda s: video_of(s).get("shown", 0) > 2, "playing again")
    w_before = preview_of(a.state())["w"]
    a.resize_preview(640, 480)
    a.wait(lambda s: preview_of(s)["w"] < w_before, "smaller frames after a resize")
    a.key("space")
    a.wait(lambda s: s["preview"] is None, "closed")
    end = time.monotonic() + 3
    while players(a) and time.monotonic() < end:
        time.sleep(0.01)
    assert not players(a), "closing stops the player"
    assert a.alive()
    a.close()


def players(a):
    found = []
    for pid in hl_tree(a.proc.pid):
        try:
            if Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[1:2] == [b"--video-helper"]:
                found.append(pid)
        except OSError:
            pass
    return found


def mapped_video(pid):
    count = 0
    for p in hl_tree(pid):
        try:
            count += Path(f"/proc/{p}/maps").read_text().count("memfd:gannet-video")
        except OSError:
            pass
    return count


@check
def the_player_is_really_jailed(h):
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=20)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    assert not players(a), "no player until something plays"
    a.key("space")
    a.wait(lambda s: video_of(s).get("shown", 0) > 2, "playing", timeout=10)
    pid = players(a)[0]
    status = Path(f"/proc/{pid}/status").read_text()
    assert re.search(r"^Seccomp:\s+2$", status, re.M), "filter on"
    assert re.search(r"^NoNewPrivs:\s+1$", status, re.M)
    assert re.search(r"^CapEff:\s+0+$", status, re.M), "no capabilities"
    for ns in ("net", "mnt", "pid", "user"):
        assert os.readlink(f"/proc/{pid}/ns/{ns}") != os.readlink(f"/proc/self/ns/{ns}"), f"own {ns} namespace"
    a.close()
    # Each thing a takeover would want, tried in a real jail by a real player.
    def attempt(what):
        env = {"PATH": "/usr/bin"}
        return subprocess.run([str(BIN), "--video-jail-try", what], capture_output=True, env=env, timeout=20).returncode

    assert attempt("thread") == 41, "threads work, as libmpv needs them"
    home = os.path.expanduser("~")
    for what in (f"see:{home}", "see:/etc/passwd", f"see:/run/user/{os.getuid()}", "see:/proc/self", "write-usr", "network"):
        assert attempt(what) == 40, f"{what} should be refused"
    assert attempt("see:/usr/lib") == 1, "and the check can tell the difference"
    for what in ("exec", "fork", "exec-memory", "make-executable"):
        assert attempt(what) == 128 + signal.SIGSYS, f"{what} should kill it, got {attempt(what)}"


@check
def a_video_keeps_playing_through_a_stream_of_resizes(h):
    """Dragging a window edge. Once killed the player, by way of a script mpv runs by itself."""
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=40)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    a.wait(lambda s: video_of(s).get("shown", 0) > 2, "playing", timeout=10)
    # Nothing in there but libmpv's own workers: no script threads to go wrong.
    pid = players(a)[0]
    threads = [Path(f"/proc/{pid}/task/{t}/comm").read_text().strip() for t in os.listdir(f"/proc/{pid}/task")]
    assert not [t for t in threads if t.startswith("lua")], threads
    sizes = [(960 - i * 6, 720 - i * 3) for i in range(70)] + [(540 + i * 6, 510 + i * 3) for i in range(70)]
    for w, h_ in sizes * 2:
        a.resize_preview(w, h_)
        s = a.state()
        assert preview_of(s).get("state") == "video", s["preview"]
    shown = video_of(a.state())["shown"]
    a.wait(lambda s: video_of(s)["shown"] > shown + 10, "still playing afterwards")
    assert players(a), "the same player is still there"
    a.close()


@check
def quick_key_presses_build_on_each_other(h):
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=30)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    a.wait(lambda s: video_of(s).get("shown", 0) > 2, "playing", timeout=10)
    # Volume, faster than the reports come back, then two quick taps of pause, many times over.
    a.key("Down", "Down", "Down", "Down", "Down", "Down", mods=["shift"])
    a.wait(lambda s: video_of(s)["volume"] == 70, "six steps down", timeout=2)
    for _ in range(8):
        a.key("p", "p")
        time.sleep(0.09)
    time.sleep(0.5)
    s = a.state()
    assert not video_of(s)["paused"], "an even number of presses ends up playing"
    assert video_of(s)["volume"] == 70, "and the volume stayed"
    shown = video_of(s)["shown"]
    a.wait(lambda s: video_of(s)["shown"] > shown + 3, "really playing")
    a.close()


@check
def a_pause_before_the_file_has_loaded_still_counts(h):
    """A real jailed player, told to pause before it has had time to load anything."""
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=10)
    env = {"PATH": "/usr/bin"}  # No sound socket.
    p = subprocess.Popen([str(BIN), "--video-jail-run", str(fs / "clip.mp4"), "pause"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True)
    time.sleep(1.6)
    p.kill()
    out = p.communicate()[0]
    p.wait()
    # The jail is still shutting down, and there's nothing to clean up but that.
    states = re.findall(r"position: ([0-9.]+).*?paused: (true|false)", out)
    assert states and states[-1][1] == "true", f"it should have stayed paused: {out[-400:]}"
    assert float(states[-1][0]) < 0.3, f"and it shouldn't have played: {states[-1]}"
    assert "frame" in out, "showing the first frame"


@check
def a_video_still_being_written_keeps_playing(h):
    fs = fresh_fs()
    clip = fs / "recording.mp4"
    make_video(clip, seconds=20)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    a.wait(lambda s: video_of(s).get("position", 0) > 1, "playing", timeout=10)
    shown = video_of(a.state())["shown"]
    for _ in range(4):
        with open(clip, "ab") as f:
            f.write(b"\0" * 4096)
        time.sleep(0.5)
    s = a.state()
    assert video_of(s)["position"] > 2, f"went back to the start: {s['preview']}"
    assert video_of(s)["shown"] > shown, "and it's the same player, still counting"
    a.close()


@check
def a_heic_photo_isnt_treated_as_a_video(h):
    fs = fresh_fs()
    (fs / "photo.heic").write_bytes(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic" + b"\0" * 200)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "info", "what it is")
    assert preview_of(s)["kind"].startswith("image/hei"), s["preview"]
    a.close()


@check
def files_that_wont_play_say_why(h):
    fs = fresh_fs()
    good = fs / "good.mp4"
    make_video(good, seconds=2)
    data = good.read_bytes()
    (fs / "a-cut-short.mp4").write_bytes(data[:600])
    (fs / "b-rubbish.mp4").write_bytes(b"\0\0\0\x20ftypisom" + os.urandom(4000))
    subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "1", "-c:a", "aac", str(fs / "c-sound-only.m4a")], check=True, capture_output=True)
    good.unlink()
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3, "listing")
    a.key("space")
    for name, words in (("a-cut-short.mp4", "can't be played"), ("b-rubbish.mp4", "can't be played")):
        s = a.wait(lambda s: preview_of(s).get("name") == name and preview_of(s)["state"] == "message", name, timeout=10)
        assert words in preview_of(s)["message"], s["preview"]
        a.key("Down")
    # No picture, so it plays as sound, and says what it is.
    s = a.wait(lambda s: preview_of(s).get("name") == "c-sound-only.m4a" and preview_of(s)["state"] == "sound", "sound only", timeout=10)
    assert preview_of(s)["kind"].startswith("audio/"), s["preview"]
    assert a.alive()
    a.close()


@check
def video_says_what_it_needs_when_a_program_is_missing(h):
    fs = fresh_fs()
    make_video(fs / "clip.mp4", seconds=2)
    make_pdf(fs / "report.pdf", 1)
    # Everything else on the PATH, but no bubblewrap.
    bin_dir = SCRATCH / "no-bwrap"
    shutil.rmtree(bin_dir, ignore_errors=True)
    bin_dir.mkdir()
    for tool in ("fc-match", "xdg-open"):
        if shutil.which(tool):
            os.symlink(shutil.which(tool), bin_dir / tool)
    a = App(h, fs, bwrap_extra=("--setenv", "PATH", str(bin_dir)))
    a.wait(lambda s: s["count"] == 2, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "message", "a clear message", timeout=10)
    assert "bubblewrap" in preview_of(s)["message"], s["preview"]
    a.shot("video-no-bwrap")
    a.key("Down")
    s = a.wait(lambda s: preview_of(s).get("name") == "report.pdf" and preview_of(s)["state"] == "message", "a PDF says so too", timeout=10)
    assert "bubblewrap" in preview_of(s)["message"], s["preview"]
    a.close()
    shutil.rmtree(bin_dir, ignore_errors=True)


@check
def a_sideways_photo_comes_out_upright(h):
    fs = fresh_fs()
    # Big-endian TIFF with one tag, Orientation = 6: a quarter turn clockwise.
    exif = b"MM\0\x2a\0\0\0\x08\0\x01" + b"\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0"
    (fs / "phone.png").write_bytes(png_bytes(300, 100, exif=exif))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 1, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "image", "the photo")
    assert (preview_of(s)["w"], preview_of(s)["h"]) == (100, 300), s["preview"]
    a.close()


def cache_thumbnail(a_cache, path, w, h, mtime, size="large"):
    uri = "file://" + quote(str(path), safe="!$&'()*+,-./:=@_~")
    folder = a_cache / "thumbnails" / size
    folder.mkdir(parents=True, exist_ok=True)
    (folder / (hashlib.md5(uri.encode()).hexdigest() + ".png")).write_bytes(png_bytes(w, h, (0, 200, 0), text={"Thumb::MTime": mtime, "Thumb::URI": uri}))


@check
def a_cached_thumbnail_shows_first_unless_its_out_of_date(h):
    fs = fresh_fs()
    photo = fs / "my photo (1).png"
    stale = fs / "stale.png"
    photo.write_bytes(png_bytes(800, 600))
    stale.write_bytes(png_bytes(800, 600))
    cache = SCRATCH / "client/cache"
    shutil.rmtree(cache / "thumbnails", ignore_errors=True)
    cache_thumbnail(cache, photo, 128, 96, int(photo.stat().st_mtime))
    cache_thumbnail(cache, stale, 128, 96, int(stale.stat().st_mtime) - 100)
    a = App(h, fs, extra_env={"GANNET_SLOW_PREVIEW_MS": "500"})
    a.wait(lambda s: s["count"] == 2, "listing")
    a.key("space")
    s = a.wait(lambda s: preview_of(s).get("state") == "thumbnail", "the thumbnail, at once")
    assert (preview_of(s)["w"], preview_of(s)["h"]) == (128, 96), s["preview"]
    a.shot("preview-thumbnail")
    s = a.wait(lambda s: preview_of(s)["state"] == "image", "the real one replaces it")
    assert preview_of(s)["w"] == 800
    # An out-of-date thumbnail would show the wrong picture, so it never appears.
    a.key("Down")
    seen = set()
    end = time.monotonic() + 3
    while time.monotonic() < end:
        p = preview_of(a.state())
        if p.get("name") == "stale.png":
            seen.add(p["state"])
            if p["state"] == "image":
                break
        time.sleep(0.005)
    assert "image" in seen and "thumbnail" not in seen, seen
    shutil.rmtree(cache / "thumbnails", ignore_errors=True)
    a.close()


def helpers(a):
    """This app's running decoder processes, however far along."""
    found = []
    for pid in hl_tree(a.proc.pid):
        try:
            if Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[1:2] == [b"--preview-helper"]:
                found.append(pid)
        except OSError:
            pass
    return found


def mapped_previews(pid):
    """How many images a process has mapped."""
    count = 0
    for p in hl_tree(pid):
        try:
            count += Path(f"/proc/{p}/maps").read_text().count("memfd:gannet-preview")
        except OSError:
            pass
    return count


@check
def the_decoder_is_locked_down_and_moving_on_frees_everything(h):
    fs = fresh_fs()
    # Big enough that the decoder is still at it when we go and look.
    (fs / "a-huge.png").write_bytes(png_bytes(9000, 9000, lambda y: (y % 256, (y >> 8) % 256, 90)))
    (fs / "b-small.png").write_bytes(png_bytes(50, 50))
    log = SCRATCH / "bench.log"
    a = App(h, fs, extra_env={"GANNET_BENCH_LOG": str(log)})
    a.wait(lambda s: s["count"] == 2, "listing")
    assert not helpers(a), "no decoder until something's previewed"
    assert mapped_previews(a.proc.pid) == 0
    before = log.read_text().count("\n")
    a.key("space")
    # Catch it running, and read how it's fenced in.
    limits = fds = None
    end = time.monotonic() + 5
    while time.monotonic() < end and limits is None:
        for pid in helpers(a):
            try:
                text = Path(f"/proc/{pid}/limits").read_text()
                status = Path(f"/proc/{pid}/status").read_text()
                if re.search(r"Max open files\s+3\s+3", text) and re.search(r"^Seccomp:\s+2$", status, re.M):
                    limits, fds = text, os.listdir(f"/proc/{pid}/fd")
                    no_new_privs = re.search(r"^NoNewPrivs:\s+1$", status, re.M)
            except OSError:
                pass
    assert limits, "never saw a locked-down decoder"
    assert sorted(fds) == ["0", "1", "2"], fds
    assert no_new_privs, "the sandbox can't be undone by running something privileged"
    assert re.search(r"Max core file size\s+0\s+0", limits), limits
    assert re.search(r"Max cpu time\s+20\s+20", limits), limits
    # The window answers while it works: the state updates and nothing's blocked.
    s = a.wait(lambda s: preview_of(s).get("state") == "image", "the huge image", timeout=20)
    a.wait(lambda s: preview_of(s)["scaled"], "the smooth copy", timeout=20)
    assert mapped_previews(a.proc.pid) >= 1, "the pixels are mapped"
    assert not helpers(a), "the decoder is finished and gone"
    busy = [int(l.split()[1]) for l in log.read_text().splitlines()[before:] if l.startswith("busy ")]
    worst = max(busy, default=0)
    print(f"      longest UI stall opening a {9000 * 9000 // 1000000} MP image: {worst / 1000:.1f} ms", flush=True)
    assert worst < 50_000, f"the UI stalled for {worst} us"
    # Moving on hands the memory back.
    a.key("Down")
    a.wait(lambda s: preview_of(s).get("name") == "b-small.png" and preview_of(s)["state"] == "image", "the next file")
    a.key("Escape")
    a.wait(lambda s: s["preview"] is None, "closed")
    end = time.monotonic() + 2
    while mapped_previews(a.proc.pid) and time.monotonic() < end:
        time.sleep(0.01)
    assert mapped_previews(a.proc.pid) == 0, "nothing still mapped after closing"
    # Leaving mid-decode kills the decoder rather than letting it finish.
    a.focus_list()
    a.key("Up")
    a.wait(lambda s: s["cursor"] == 0, "back on the huge one")
    a.key("space")
    end = time.monotonic() + 5
    while not helpers(a) and time.monotonic() < end:
        time.sleep(0.001)
    assert helpers(a), "never saw it start"
    a.key("Escape")
    end = time.monotonic() + 2
    while helpers(a) and time.monotonic() < end:
        time.sleep(0.005)
    assert not helpers(a), "closing stops the decoder"
    assert a.alive()
    a.close()


@check
def the_real_decoder_cant_do_what_a_takeover_would_want(h):
    """A real decoder process, locked down the real way, trying each thing itself."""
    def attempt(what):
        return subprocess.run([str(BIN), "--preview-helper-try", what], stdin=subprocess.DEVNULL, capture_output=True, env={}, cwd="/", timeout=20).returncode

    assert attempt("nothing") == 41, "it gets this far"
    assert attempt("fstat-is-fine") == 41
    for what in ("open", "stat-path", "statx-path"):
        assert attempt(what) == 40, f"{what} should be refused"
    for what in ("socket", "exec", "fork", "thread", "exec-memory", "make-executable", "signal-another"):
        assert attempt(what) == -signal.SIGSYS, f"{what} should kill it, got {attempt(what)}"
    assert attempt("out-of-memory") == -signal.SIGABRT, "running out of memory still reads as that"
    # A stricter limit from whatever started the app isn't a reason to refuse.
    for limit in ("-v 2000000", "-t 10", "-f 100000"):
        r = subprocess.run(["sh", "-c", f'ulimit {limit}; exec "{BIN}" --preview-helper-try nothing'], stdin=subprocess.DEVNULL, capture_output=True, env={}, cwd="/", timeout=20)
        assert r.returncode == 41, f"under ulimit {limit}: {r.returncode}"


@check
def the_sandboxed_decoder_reads_real_files_up_to_the_limit(h):
    fs = fresh_fs()
    near_limit = fs / "near-limit.png"
    near_limit.write_bytes(png_bytes(12000, 12000, lambda y: (y % 256, (y >> 8) % 256, 90)))
    out = fs / "out"
    with open(near_limit, "rb") as i, open(out, "wb") as o:
        r = subprocess.run([str(BIN), "--preview-helper"], stdin=i, stdout=o, stderr=subprocess.PIPE, env={}, cwd="/", timeout=60)
    assert r.returncode == 0, (r.returncode, r.stderr[-300:])
    assert out.stat().st_size == 12 + 12000 * 12000 * 4
    out.unlink()
    # Short of memory for real, under a limit from whatever started the app. It's only a soft
    # limit, so this also shows the decoder keeps it rather than raising it.
    with open(near_limit, "rb") as i:
        r = subprocess.run(["sh", "-c", f'ulimit -S -v 300000; exec "{BIN}" --preview-helper'], stdin=i, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={}, cwd="/", timeout=60)
    assert r.returncode in (5, -signal.SIGABRT), f"reads as too large either way, got {r.returncode}"
    near_limit.unlink()
    # Past the limit, with the pixels not even there.
    (fs / "past.png").write_bytes(png_bytes(64, 64, claim=(13000, 13000)))
    with open(fs / "past.png", "rb") as i:
        r = subprocess.run([str(BIN), "--preview-helper"], stdin=i, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={}, cwd="/", timeout=20)
    assert r.returncode == 5, r.returncode


@check
def odd_things_to_preview(h):
    fs = fresh_fs()
    tree(fs, {"folder/inside": "", "locked.png": "x", "ok.png": ""})
    (fs / "ok.png").write_bytes(png_bytes(30, 30))
    os.mkfifo(fs / "pipe")
    os.symlink(fs / "ok.png", fs / "shortcut.png")
    os.symlink(fs / "nowhere", fs / "dangling.png")
    (fs / "empty").write_bytes(b"")
    os.chmod(fs / "locked.png", 0)
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 7, "listing")
    a.key("space")
    # Words for what's wrong, a picture, or what the thing is.
    want = {
        "folder": ("info", "inode/directory"),
        "dangling.png": ("message", "no longer here"),
        "empty": ("info", "application/x-zerosize"),
        "locked.png": ("message", "permission"),
        "ok.png": ("image", None),
        "pipe": ("info", "inode/fifo"),
        "shortcut.png": ("image", None),
    }
    try:
        for i, (name, (state, words)) in enumerate(want.items()):
            if i:
                a.key("Down")
            s = a.wait(lambda s: preview_of(s).get("name") == name and preview_of(s)["state"] in ("image", "message", "info"), name)
            p = preview_of(s)
            assert p["state"] == state, p
            if state == "message":
                assert words in p["message"], p
            if state == "info":
                assert p["kind"] == words, p
    finally:
        os.chmod(fs / "locked.png", 0o600)
    assert a.alive()
    a.close()


@check
def a_file_deleted_while_previewing_and_a_tiny_window(h):
    fs = fresh_fs()
    (fs / "a.png").write_bytes(png_bytes(500, 500))
    (fs / "b.png").write_bytes(png_bytes(60, 60))
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2, "listing")
    a.key("space")
    a.wait(lambda s: preview_of(s).get("state") == "image", "first file")
    (fs / "a.png").unlink()
    a.wait(lambda s: preview_of(s).get("name") == "b.png", "moves on to what's still there")
    a.wait(lambda s: preview_of(s)["state"] == "image", "and shows it")
    # Resizing down to almost nothing and back, with every key that does arithmetic.
    a.resize_preview(60, 20)
    time.sleep(0.3)
    a.key("plus")
    a.key("minus")
    a.key("Right", mods=["shift"])
    a.key("0")
    time.sleep(0.2)
    assert a.alive() and a.state()["preview"], "tiny window"
    a.resize_preview(960, 720)
    time.sleep(0.3)
    assert a.alive() and a.state()["preview"]
    (fs / "b.png").unlink()
    a.wait(lambda s: s["preview"] is None, "nothing left to show")
    a.close()


@check
def exits_cleanly_when_the_compositor_goes(h):
    fs = fresh_fs()
    tree(fs, {"a": ""})
    # A compositor of its own, since this kills it.
    h2 = Headless("/tmp/gne2e-h2")
    try:
        a = App(h2, fs)
        a.wait(lambda s: s["count"] == 1)
        h2.stop()
        a.proc.wait(5)
    finally:
        try:
            h2.stop()
        except Exception:
            pass


BTN_LEFT = 272


def row_of(a, name):
    return a.state()["names"].index(name)


def press_and_carry(a, h, name, x, y):
    """Presses on a row and carries it to x, y, with the button still down."""
    a.move(ROW_X, row_y(row_of(a, name)))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(ROW_X + 20, row_y(row_of(a, name)) + 2)
    a.move(x, y)
    a.wait(lambda s: s["drag"] not in ("", "pressed"), "dragging")


def release(a, h):
    h.pointer(f"button {BTN_LEFT} 0")
    time.sleep(0.03)


def holding(a, *mods):
    """Holds keys for a moment, for a drop to read. Returns the process to wait on."""
    cmd = ["wtype"]
    for m in mods:
        cmd += ["-M", m]
    cmd += ["-s", "700"]
    for m in mods:
        cmd += ["-m", m]
    return subprocess.Popen(cmd, env=a.env)


@check
def drag_moves_files_onto_a_folder_and_undo_brings_them_back(h):
    fs = fresh_fs()
    tree(fs, {"d/keep": "k", "a.txt": "A", "b.txt": "B", "c.txt": "C"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 4)
    # One row: pressing it selects it, and dragging takes only it.
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "d")))
    s = a.wait(lambda s: s["drag"] == "Move “a.txt” to d", "says what the drop will do")
    a.shot("drag-over-folder")
    release(a, h)
    a.wait(lambda s: s["drag"] == "" and s["jobs"] == 0 and s["count"] == 3, "moved")
    assert (fs / "d/a.txt").read_text() == "A" and not (fs / "a.txt").exists()
    a.key("z", mods=["ctrl"])
    a.wait(lambda s: s["count"] == 4 and s["jobs"] == 0, "undone")
    assert (fs / "a.txt").read_text() == "A" and not (fs / "d/a.txt").exists()


@check
def dragging_one_selected_row_takes_the_whole_selection(h):
    fs = fresh_fs()
    tree(fs, {"d/keep": "k", "a.txt": "A", "b.txt": "B", "c.txt": "C"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 4)
    a.click(ROW_X, row_y(row_of(a, "a.txt")))
    a.key("Down", mods=["shift"])
    a.wait(lambda s: sorted(s["selected"]) == ["a.txt", "b.txt"], "two selected")
    press_and_carry(a, h, "b.txt", ROW_X, row_y(row_of(a, "d")))
    a.wait(lambda s: s["drag"] == "Move 2 items to d", "carries both")
    a.shot("drag-several")
    release(a, h)
    a.wait(lambda s: s["jobs"] == 0 and s["count"] == 2, "moved both")
    assert (fs / "d/a.txt").exists() and (fs / "d/b.txt").exists() and (fs / "c.txt").exists()


@check
def a_click_in_a_selection_narrows_it_on_release(h):
    fs = fresh_fs()
    tree(fs, {"a.txt": "A", "b.txt": "B", "c.txt": "C"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    a.click(ROW_X, row_y(0))
    a.key("Down", mods=["shift"])
    a.wait(lambda s: len(s["selected"]) == 2)
    a.move(ROW_X, row_y(1))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.05)
    assert len(a.state()["selected"]) == 2, "the press keeps the selection, in case it's a drag"
    release(a, h)
    a.wait(lambda s: s["selected"] == ["b.txt"], "narrowed")
    assert a.state()["drag"] == ""


@check
def a_box_selects_the_rows_it_touches(h):
    fs = fresh_fs()
    tree(fs, {f"f{i}.txt": "x" for i in range(6)})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 6)
    a.move(BESIDE_X, row_y(1))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(BESIDE_X - 200, row_y(3))
    s = a.wait(lambda s: s["drag"] == "band" and s["selected"] == ["f1.txt", "f2.txt", "f3.txt"], "selects as it goes")
    a.shot("band")
    a.move(BESIDE_X - 200, row_y(2))
    a.wait(lambda s: s["selected"] == ["f1.txt", "f2.txt"], "and lets go of rows it leaves")
    release(a, h)
    a.wait(lambda s: s["drag"] == "" and s["selected"] == ["f1.txt", "f2.txt"], "kept")
    # Escape puts back what was selected before the press.
    a.move(BESIDE_X, row_y(4))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(BESIDE_X, row_y(5))
    a.wait(lambda s: s["drag"] == "band" and s["selected"] == ["f4.txt", "f5.txt"], "replaces it")
    a.key("Escape")
    a.wait(lambda s: s["drag"] == "" and s["selected"] == ["f1.txt", "f2.txt"], "put back")
    release(a, h)
    assert a.state()["selected"] == ["f1.txt", "f2.txt"]
    # Ctrl adds.
    a.move(BESIDE_X, row_y(5))
    k = holding(a, "ctrl")
    time.sleep(0.2)  # The press has to come after the key.
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(BESIDE_X, row_y(4))
    a.wait(lambda s: s["drag"] == "band" and s["selected"] == ["f1.txt", "f2.txt", "f4.txt", "f5.txt"], "adds")
    release(a, h)
    k.wait(5)
    a.wait(lambda s: s["drag"] == "" and len(s["selected"]) == 4, "kept")
    # From below the rows, upwards.
    a.move(BESIDE_X, row_y(8))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    assert a.state()["selected"] == [], "the press still clears"
    a.move(BESIDE_X, row_y(5))
    a.wait(lambda s: s["drag"] == "band" and s["selected"] == ["f5.txt"], "from empty space")
    release(a, h)
    # A press and release beside a name is still a click.
    a.click(BESIDE_X, row_y(2))
    a.wait(lambda s: s["drag"] == "" and s["selected"] == ["f2.txt"], "a click selects")
    # On the name, a drag still carries the file.
    press_and_carry(a, h, "f1.txt", ROW_X + 200, row_y(4))
    assert a.state()["drag"] not in ("band", "pressed")
    a.key("Escape")
    release(a, h)
    assert sorted(p.name for p in fs.iterdir()) == [f"f{i}.txt" for i in range(6)], "nothing moved"


@check
def a_box_scrolls_past_the_end_of_the_list(h):
    fs = fresh_fs()
    tree(fs, {f"f{i:03}.txt": "x" for i in range(200)})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 200)
    a.move(BESIDE_X, row_y(2))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(BESIDE_X, row_y(5))
    a.wait(lambda s: s["drag"] == "band")
    a.move(BESIDE_X, 790)
    s = a.wait(lambda s: len(s["selected"]) > 60 and s["selected"][0] == "f002.txt", "keeps going, from where it started", timeout=10)
    a.shot("band-scrolled")
    release(a, h)
    n = len(a.wait(lambda s: s["drag"] == "")["selected"])
    time.sleep(0.2)
    assert len(a.state()["selected"]) == n, "the scrolling stopped with it"


@check
def a_small_wobble_is_a_click_not_a_drag(h):
    fs = fresh_fs()
    tree(fs, {"d/keep": "k", "a.txt": "A"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    y = row_y(row_of(a, "a.txt"))
    a.move(ROW_X, y)
    h.pointer(f"button {BTN_LEFT} 1")
    a.move(ROW_X + 3, y + 2)
    assert a.state()["drag"] == "pressed"
    release(a, h)
    assert (fs / "a.txt").exists()
    a.wait(lambda s: s["drag"] == "" and s["selected"] == ["a.txt"], "just selected")


@check
def the_keys_held_at_the_drop_decide(h):
    fs = fresh_fs()
    tree(fs, {"d/keep": "k", "a.txt": "A", "b.txt": "B"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 3)
    # Ctrl copies.
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "d")))
    a.wait(lambda s: s["drag"].startswith("Move "), "moves with no keys")
    held = holding(a, "ctrl")
    a.wait(lambda s: s["drag"] == "Copy “a.txt” to d", "Ctrl says copy")
    release(a, h)
    held.wait(5)
    a.wait(lambda s: s["jobs"] == 0 and (fs / "d/a.txt").exists(), "copied")
    assert (fs / "a.txt").read_text() == "A"
    # Ctrl+Shift links.
    press_and_carry(a, h, "b.txt", ROW_X, row_y(row_of(a, "d")))
    held = holding(a, "ctrl", "shift")
    a.wait(lambda s: s["drag"] == "Link “b.txt” in d", "Ctrl+Shift says link")
    a.shot("drag-link")
    release(a, h)
    held.wait(5)
    a.wait(lambda s: s["jobs"] == 0 and (fs / "d/b.txt").is_symlink(), "linked")
    assert os.readlink(fs / "d/b.txt") == str(fs / "b.txt") and (fs / "b.txt").read_text() == "B"


@check
def another_disk_copies_unless_shift_moves(h):
    fs = fresh_fs()
    disk = fs / "disk"
    tree(fs, {"disk/.placeholder": "", "a.txt": "A", "b.txt": "B"})
    a = AppWithDisk(h, fs, disk)
    a.wait(lambda s: s["count"] == 3)
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "disk")))
    a.wait(lambda s: s["drag"] == "Copy “a.txt” to disk", "copies across disks")
    release(a, h)
    a.wait(lambda s: s["jobs"] == 0 and s["drag"] == "", "copied")
    assert (fs / "a.txt").exists(), "the source stays"
    press_and_carry(a, h, "b.txt", ROW_X, row_y(row_of(a, "disk")))
    held = holding(a, "shift")
    a.wait(lambda s: s["drag"] == "Move “b.txt” to disk", "Shift says move")
    release(a, h)
    held.wait(5)
    a.wait(lambda s: s["jobs"] == 0 and s["count"] == 2, "moved")
    assert not (fs / "b.txt").exists(), "the source goes"
    # A folder reached through a symlink is on the disk it really lives on, whatever its path says.
    (fs / "link").symlink_to(disk)
    a.key("l", mods=["ctrl"])
    a.type(str(fs / "link"))
    a.key("Return")
    a.wait(lambda s: s["dir"] == str(fs / "link") and "a.txt" in s["names"], "in the other disk, through a symlink")
    home = Path(a.env["HOME"])
    (home / "a.txt").unlink(missing_ok=True)
    time.sleep(0.5)
    press_and_carry(a, h, "a.txt", 60, BAR + 8 + 15)
    a.wait(lambda s: s["drag"].startswith("Copy “a.txt” to"), "its real disk decides")
    release(a, h)
    a.wait(lambda s: s["jobs"] == 0 and s["drag"] == "", "copied")
    assert (home / "a.txt").exists() and "a.txt" in a.state()["names"], "the source stays"


@check
def escape_or_nowhere_cancels_a_drag(h):
    fs = fresh_fs()
    tree(fs, {"d/keep": "k", "a.txt": "A"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    # Escape, and the release that follows it does nothing.
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "d")))
    a.key("Escape")
    a.wait(lambda s: s["drag"] == "", "cancelled")
    release(a, h)
    time.sleep(0.1)
    assert (fs / "a.txt").exists() and not (fs / "d/a.txt").exists()
    # Empty space below the rows.
    press_and_carry(a, h, "a.txt", ROW_X, row_y(5))
    a.wait(lambda s: s["drag"] == "“a.txt”", "no target says no action")
    release(a, h)
    a.wait(lambda s: s["drag"] == "", "ended")
    # A file isn't a folder.
    tree(fs, {"z.txt": "Z"})
    a.wait(lambda s: s["count"] == 3)
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "z.txt")))
    a.wait(lambda s: s["drag"] == "“a.txt”", "a file is no target")
    release(a, h)
    a.wait(lambda s: s["drag"] == "", "ended")
    assert (fs / "a.txt").exists() and (fs / "z.txt").read_text() == "Z"
    assert a.state()["jobs"] == 0 and a.state()["dialog"] == ""


@check
def a_folder_cant_be_dropped_on_itself(h):
    fs = fresh_fs()
    tree(fs, {"d/inner/keep": "k", "e/keep": "k"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    y = row_y(row_of(a, "d"))
    a.move(ROW_X, y)
    h.pointer(f"button {BTN_LEFT} 1")
    a.move(ROW_X + 30, y)
    a.wait(lambda s: s["drag"] == "“d”", "its own row is no target")
    release(a, h)
    a.wait(lambda s: s["drag"] == "", "ended")
    assert (fs / "d/inner/keep").exists() and a.state()["notices"] == []


@check
def dropping_on_the_sidebar_moves_there_and_trash_really_trashes(h):
    fs = fresh_fs()
    tree(fs, {"work/a.txt": "A", "work/b.txt": "B"})
    a = App(h, fs / "work")
    a.wait(lambda s: s["count"] == 2)
    home = Path(a.env["HOME"])
    (home / "a.txt").unlink(missing_ok=True)
    shutil.rmtree(Path(a.env["XDG_DATA_HOME"]) / "Trash", ignore_errors=True)
    # Home is the first place.
    place_y = BAR + 8 + 15
    press_and_carry(a, h, "a.txt", 60, place_y)
    a.wait(lambda s: s["drag"] == f"Move “a.txt” to {home.name}" or s["drag"] == f"Copy “a.txt” to {home.name}", "over Home")
    a.shot("drag-over-place")
    release(a, h)
    a.wait(lambda s: s["jobs"] == 0 and s["count"] == 1, "left this folder")
    assert (home / "a.txt").read_text() == "A"
    # Trash goes through the real operation, so an info file goes with it.
    data = Path(a.env["XDG_DATA_HOME"])
    press_and_carry(a, h, "b.txt", 60, BAR + 8 + 1 * 30 + 15)
    a.wait(lambda s: s["drag"] == "Move “b.txt” to Trash", "over Trash")
    # Ctrl asks for a copy, so Trash stops being a target while it's held.
    held = holding(a, "ctrl")
    a.wait(lambda s: s["drag"] == "“b.txt”", "Ctrl over Trash does nothing")
    held.wait(5)
    a.wait(lambda s: s["drag"] == "Move “b.txt” to Trash", "over Trash again")
    release(a, h)
    a.wait(lambda s: s["jobs"] == 0 and s["count"] == 0, "trashed")
    assert (data / "Trash/files/b.txt").exists()
    assert (data / "Trash/info/b.txt.trashinfo").exists(), "Trash's two folders stay paired"


@check
def dragging_out_of_the_trash_only_copies(h):
    fs = fresh_fs()
    tree(fs, {"work/out-of-trash.txt": "T"})
    a = App(h, fs / "work")
    a.wait(lambda s: s["count"] == 1)
    (Path(a.env["HOME"]) / "out-of-trash.txt").unlink(missing_ok=True)
    shutil.rmtree(Path(a.env["XDG_DATA_HOME"]) / "Trash", ignore_errors=True)
    a.click(ROW_X, row_y(0))
    a.key("Delete")
    a.wait(lambda s: s["count"] == 0 and s["jobs"] == 0, "trashed")
    data = Path(a.env["XDG_DATA_HOME"])
    time.sleep(0.5)  # So the press below isn't a double-click on that name.
    a.click(60, BAR + 8 + 1 * 30 + 15)
    a.wait(lambda s: s["dir"].endswith("Trash/files") and "out-of-trash.txt" in s["names"], "in the Trash")
    press_and_carry(a, h, "out-of-trash.txt", 60, BAR + 8 + 15)
    held = holding(a, "shift")
    a.wait(lambda s: s["drag"].startswith("Copy “out-of-trash.txt” to"), "copies even with Shift")
    release(a, h)
    held.wait(5)
    a.wait(lambda s: s["jobs"] == 0, "done")
    assert (data / "Trash/files/out-of-trash.txt").exists() and (data / "Trash/info/out-of-trash.txt.trashinfo").exists()
    assert (Path(a.env["HOME"]) / "out-of-trash.txt").read_text() == "T"


@check
def conflicts_in_a_drag_ask_like_paste(h):
    fs = fresh_fs()
    tree(fs, {"d/a.txt": "old", "a.txt": "new"})
    a = App(h, fs)
    a.wait(lambda s: s["count"] == 2)
    press_and_carry(a, h, "a.txt", ROW_X, row_y(row_of(a, "d")))
    release(a, h)
    a.wait(lambda s: s["dialog"] == "conflict", "asks")
    time.sleep(0.5)  # The dialog ignores keys at first, so a stray press can't answer it.
    a.key("Escape")
    a.wait(lambda s: s["dialog"] == "" and s["jobs"] == 0, "cancelled")
    assert (fs / "d/a.txt").read_text() == "old" and (fs / "a.txt").read_text() == "new"

# Two windows tile side by side, so a drag can leave one and reach the other.
OTHER_X = 640


def two_windows(h, fs):
    """Windows on fs/src and fs/dst. The second has the keyboard."""
    a = App(h, fs / "src")
    a.wait(lambda s: s["count"] > 0, "the first window")
    b = App(h, fs / "dst", state_name="state2.json")
    b.wait(lambda s: s["dir"] == str(fs / "dst"), "the second window")
    time.sleep(0.3)  # The first window has to take its new size.
    return a, b


def carry_out(a, h, name, x, y):
    """Presses on a row in the left window and carries it out of the window, button down."""
    a.move(ROW_X, row_y(row_of(a, name)))
    h.pointer(f"button {BTN_LEFT} 1")
    time.sleep(0.03)
    a.move(ROW_X + 20, row_y(row_of(a, name)) + 2)
    a.wait(lambda s: s["drag"] not in ("", "pressed"), "dragging")
    a.move(OTHER_X - 20, 500)
    a.move(OTHER_X + 20, 500)
    a.wait(lambda s: s["drag"] == "out", "handed to the compositor")
    a.move(x, y)


@check
def a_plain_drag_moves_to_another_window_on_the_same_disk(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "src/b.txt": "B", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Move to dst", "the other window says it will move")
    b.shot("drag-between-windows")
    release(a, h)
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" in s["names"], "arrived")
    a.wait(lambda s: s["drag"] == "" and s["names"] == ["b.txt"], "gone from the first")
    assert (fs / "dst/a.txt").read_text() == "A" and not (fs / "src/a.txt").exists()
    assert b.state()["drag"] == ""


@check
def ctrl_copies_to_another_window(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    a.move(300, 600)
    time.sleep(0.1)
    held = holding(a, "ctrl")
    time.sleep(0.1)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Copy to dst", "says it will copy")
    release(a, h)
    held.wait(5)
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" in s["names"], "arrived")
    assert (fs / "dst/a.txt").read_text() == "A" and (fs / "src/a.txt").read_text() == "A"


@check
def ctrl_and_shift_link_in_another_window(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    a.move(300, 600)
    time.sleep(0.1)
    held = holding(a, "ctrl", "shift")
    time.sleep(0.1)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Link in dst", "says it will link")
    release(a, h)
    held.wait(5)
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" in s["names"], "arrived")
    assert (fs / "dst/a.txt").is_symlink() and (fs / "src/a.txt").read_text() == "A"


@check
def another_window_on_another_disk_copies_unless_shift_moves(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "src/b.txt": "B", "disk/.placeholder": ""})
    a = App(h, fs / "src")
    a.wait(lambda s: s["count"] == 2, "the first window")
    b = AppWithDisk2(h, fs / "disk")
    b.wait(lambda s: s["dir"] == str(fs / "disk"), "the second window")
    time.sleep(0.3)
    # Shift first: a press with Shift held extends a selection, and nothing's selected yet.
    a.move(300, 600)
    time.sleep(0.1)
    held = holding(a, "shift")
    time.sleep(0.1)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Move to disk", "Shift says move")
    release(a, h)
    held.wait(5)
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" in s["names"], "arrived")
    a.wait(lambda s: s["drag"] == "" and "a.txt" not in s["names"], "gone from the first")
    assert not (fs / "src/a.txt").exists(), "the source goes"
    carry_out(a, h, "b.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Copy to disk", "copies across disks")
    release(a, h)
    b.wait(lambda s: s["jobs"] == 0 and "b.txt" in s["names"], "arrived")
    a.wait(lambda s: s["drag"] == "", "the first window is free")
    assert (fs / "src/b.txt").read_text() == "B", "the source stays"


@check
def shift_moves_to_another_window_and_it_can_be_undone(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "src/b.txt": "B", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    # Keyboard focus follows the pointer here, so hold Shift once it's in the first window.
    a.move(300, 600)
    time.sleep(0.1)
    held = holding(a, "shift")
    time.sleep(0.1)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Move to dst", "says it will move")
    release(a, h)
    held.wait(5)
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" in s["names"], "arrived")
    a.wait(lambda s: s["drag"] == "" and s["names"] == ["b.txt"], "gone from the first")
    assert (fs / "dst/a.txt").read_text() == "A" and not (fs / "src/a.txt").exists()
    # The window that did the work can undo it, once a click has given it the keyboard.
    b.click(OTHER_X + 410, 600)
    time.sleep(0.1)
    b.key("z", mods=["ctrl"])
    b.wait(lambda s: s["jobs"] == 0 and "a.txt" not in s["names"], "undone")
    assert (fs / "src/a.txt").read_text() == "A"


@check
def a_drag_can_land_on_a_folder_in_another_window(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "dst/inner/keep": "k", "dst/z.txt": "Z"})
    a, b = two_windows(h, fs)
    carry_out(a, h, "a.txt", OTHER_X + 400, row_y(row_of(b, "inner")))
    b.wait(lambda s: s["drag"] == "Move to inner", "over the folder")
    release(a, h)
    b.wait(lambda s: s["jobs"] == 0 and (fs / "dst/inner/a.txt").exists(), "in the folder")
    assert (fs / "dst/inner/a.txt").read_text() == "A"


@check
def several_files_go_together_to_another_window(h):
    fs = fresh_fs()
    tree(fs, {"src/a b.txt": "A", "src/c#d.txt": "C", "src/é.txt": "E", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    # Keyboard focus follows the pointer here.
    a.click(ROW_X, row_y(0))
    time.sleep(0.1)
    a.key("a", mods=["ctrl"])
    a.wait(lambda s: len(s["selected"]) == 3, "all selected")
    time.sleep(0.5)
    carry_out(a, h, "c#d.txt", OTHER_X + 400, 500)
    release(a, h)
    b.wait(lambda s: s["jobs"] == 0 and len(s["names"]) == 4, "all arrived")
    for name, text in (("a b.txt", "A"), ("c#d.txt", "C"), ("é.txt", "E")):
        assert (fs / "dst" / name).read_text() == text, name
        assert not (fs / "src" / name).exists(), "a plain drag moves on the same disk"


@check
def releasing_outside_any_target_moves_nothing(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    # The other window's path bar takes nothing.
    carry_out(a, h, "a.txt", OTHER_X + 400, 20)
    time.sleep(0.1)
    assert b.state()["drag"] == ""
    release(a, h)
    a.wait(lambda s: s["drag"] == "", "the drag ends")
    time.sleep(0.2)
    assert (fs / "src/a.txt").exists() and not (fs / "dst/a.txt").exists()
    assert a.state()["jobs"] == 0 and b.state()["jobs"] == 0


@check
def a_drag_can_come_back_and_land_in_its_own_window(h):
    fs = fresh_fs()
    tree(fs, {"src/d/keep": "k", "src/a.txt": "A", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Move to dst", "over the other window")
    a.move(ROW_X, row_y(row_of(a, "d")))
    a.wait(lambda s: s["drag"] == "Move “a.txt” to d", "back over its own window")
    b.wait(lambda s: s["drag"] == "", "the other window lets go")
    release(a, h)
    a.wait(lambda s: s["drag"] == "" and s["jobs"] == 0 and (fs / "src/d/a.txt").exists(), "moved")
    assert not (fs / "dst/a.txt").exists()
    # And the pointer works normally afterwards.
    a.move(ROW_X, row_y(0))
    a.click(ROW_X, row_y(0))
    a.wait(lambda s: s["selected"] == ["d"], "clicks work again")


@check
def dropping_back_where_they_came_from_does_nothing(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "A", "src/b.txt": "B", "dst/keep": "k"})
    a, b = two_windows(h, fs)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    a.move(ROW_X, row_y(5))
    a.wait(lambda s: s["drag"] == "“a.txt”", "its own folder is no target")
    release(a, h)
    a.wait(lambda s: s["drag"] == "", "ended")
    time.sleep(0.2)
    assert sorted(os.listdir(fs / "src")) == ["a.txt", "b.txt"]
    assert a.state()["dialog"] == "" and a.state()["jobs"] == 0


@check
def a_conflict_from_another_window_asks_in_the_window_doing_the_work(h):
    fs = fresh_fs()
    tree(fs, {"src/a.txt": "new", "dst/a.txt": "old"})
    a, b = two_windows(h, fs)
    carry_out(a, h, "a.txt", OTHER_X + 400, 500)
    b.wait(lambda s: s["drag"] == "Move to dst", "over the other window")
    release(a, h)
    b.wait(lambda s: s["dialog"] == "conflict", "asks")
    a.wait(lambda s: s["drag"] == "", "the first window is free")
    assert (fs / "dst/a.txt").read_text() == "old" and (fs / "src/a.txt").read_text() == "new"
    # The dialog ignores keys at first, and a click gives this window the keyboard.
    time.sleep(0.5)
    b.click(OTHER_X + 410, 700)
    time.sleep(0.1)
    b.key("Escape")
    b.wait(lambda s: s["dialog"] == "" and s["jobs"] == 0, "cancelled")
    assert (fs / "dst/a.txt").read_text() == "old" and (fs / "src/a.txt").read_text() == "new"


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else ""
    assert BIN.exists(), "run: CARGO_TARGET_DIR=target/bench cargo build --release -p gannet --features bench"
    SCRATCH.mkdir(exist_ok=True)
    (SCRATCH / "stderr.log").write_text("")
    h = Headless("/tmp/gne2e-h")
    failed = 0
    try:
        for t in TESTS:
            if want not in t.__name__:
                continue
            try:
                t(h)
                print(f"ok    {t.__name__}", flush=True)
            except Exception as e:
                failed += 1
                print(f"FAIL  {t.__name__}: {e}", flush=True)
            subprocess.run(["pkill", "-f", str(BIN)], capture_output=True)
            time.sleep(0.05)
    finally:
        h.stop()
    print((SCRATCH / "stderr.log").read_text()[-2000:], end="")
    print("failed" if failed else "all passed", failed)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
