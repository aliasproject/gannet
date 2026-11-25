#!/usr/bin/env python3
"""A tour of Gannet with the real Orca listening, written down as what Orca said at each step.

Everything runs in the e2e suite's private world: the headless compositor, a private session
bus with a stand-in at-spi-bus-launcher, a private accessibility bus with the real
at-spi2-registryd, the stand-in udisks2, and scratch folders for HOME and every XDG folder.
Orca runs in bubblewrap with no sound devices, the real session's runtime folder covered and
no speech-dispatcher to start, so it never speaks aloud. It still logs what it would have said,
and that's what the transcript is.

  python3 -B tests/orca_tour.py [TRANSCRIPT]

Needs orca, at-spi2-core and the e2e builds (see the README's Development section). The
transcript goes to .bench/orca-tour.txt unless another file is named, with Orca's own debug
log beside it as orca-debug.out. It fails if the passphrase typed at the drive-unlock prompt
appears anywhere in Orca's log.
"""
import os, re, shutil, subprocess, sys, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import e2e  # noqa: E402
from e2e import (ROOT, ROW_X, SCRATCH, App, Headless, ScreenReader, StandIn, bookmarks_file, click_place, fresh_fs, go_to,  # noqa: E402
                 holding, place_flags, png_bytes, row_y, sandboxed, stand_in_apps, tree)

ORCA = shutil.which("orca") or "/usr/bin/orca"
# Orca 51 logs each utterance as `SPEECH OUTPUT: 'text'`, followed by its voice if it has one.
SPOKEN = re.compile(r"SPEECH OUTPUT: '(.*)'(?:\{[^{}]*\}|None)?\s*$")


class Orca:
    """The real Orca on the screen reader's buses, speaking only into its debug log."""

    def __init__(self, sr, h, keep):
        root = SCRATCH / "orca"
        shutil.rmtree(root, ignore_errors=True)
        root.mkdir(parents=True)
        # Orca can only write inside the sandbox; the log is copied to `keep` when it stops.
        self.log, self.keep = root / "debug.out", keep
        env = {"PATH": "/usr/bin", "LANG": "C.UTF-8", "DBUS_SESSION_BUS_ADDRESS": sr.session.address, "AT_SPI_BUS_ADDRESS": sr.env["AT_SPI_BUS_ADDRESS"],
               # Settings live in memory only: nothing is read from or written to dconf.
               "GSETTINGS_BACKEND": "memory", "XDG_SESSION_TYPE": "wayland",
               "XDG_RUNTIME_DIR": str(h.root / "runtime"), "WAYLAND_DISPLAY": "wayland-1"}
        for k, d in {"HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state"}.items():
            (root / d).mkdir(parents=True, exist_ok=True)
            env[k] = str(root / d)
        home = os.path.expanduser("~")
        uid = os.geteuid()
        # No /dev/snd (bwrap's /dev has none), no sound server's socket (the real runtime folder
        # is covered), and no speech-dispatcher to start: Orca finds no speech server and only
        # logs what it would say.
        cmd = ["bwrap", "--die-with-parent", "--unshare-pid", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", home,
               "--tmpfs", f"/run/user/{uid}", "--ro-bind", "/dev/null", "/usr/bin/speech-dispatcher", "--bind", str(SCRATCH), str(SCRATCH),
               "--bind", str(h.root), str(h.root), ORCA, "--debug-file", str(self.log)]
        self.proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=open(SCRATCH / "orca-stderr.log", "w"))
        self.seen = 0
        deadline = time.monotonic() + 20
        while "Starting Atspi main event loop" not in self.text():
            assert self.proc.poll() is None, "Orca exited; see " + str(SCRATCH / "orca-stderr.log")
            assert time.monotonic() < deadline, "Orca never started"
            time.sleep(0.1)
        assert "Speech not available" in self.text(), "Orca found a speech server, and could speak aloud"

    def text(self):
        try:
            return self.log.read_text(errors="replace")
        except OSError:
            return ""

    def lines(self):
        out = []
        for line in self.text().splitlines():
            m = SPOKEN.search(line)
            if m:
                out.append(m.group(1).strip())
        return out[self.seen:]

    def said(self, settle=1.0, at_least=0.3):
        """What Orca has said since last asked, once it has been quiet for `settle` seconds."""
        time.sleep(at_least)
        last, quiet_since = None, time.monotonic()
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            lines = self.lines()
            if lines != last:
                last, quiet_since = lines, time.monotonic()
            elif time.monotonic() - quiet_since >= settle:
                break
            time.sleep(0.1)
        self.seen += len(last)
        return last

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.keep.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(self.log, self.keep)


class Tour:
    """The transcript: each step, then what Orca said after it."""

    def __init__(self, out):
        self.out = out
        self.text = []
        self.orca = None

    def section(self, title):
        self.text.append(f"\n## {title}\n")
        self.flush()

    def note(self, line):
        self.text.append(f"- ({line})\n")

    def step(self, what, act=None, settle=1.0):
        if act:
            act()
        spoken = self.orca.said(settle) if self.orca else []
        self.text.append(f"- {what}\n" + "".join(f"    « {s} »\n" for s in spoken) + ("    (silence)\n" if not spoken else ""))
        self.flush()
        return spoken

    def flush(self):
        self.out.parent.mkdir(parents=True, exist_ok=True)
        self.out.write_text("# What Orca said, step by step\n" + "".join(self.text))


def main():
    out = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / ".bench/orca-tour.txt"
    assert e2e.BIN.exists(), "run: CARGO_TARGET_DIR=target/bench cargo build --release -p gannet --features bench"
    assert e2e.STANDIN.exists(), "run: CARGO_TARGET_DIR=target/bench cargo build --release -p gannet-core --example udisks-standin"
    assert shutil.which("orca") and e2e.REGISTRYD.exists(), "needs orca and at-spi2-core"
    SCRATCH.mkdir(exist_ok=True)
    (SCRATCH / "stderr.log").write_text("")
    for d in ("client/state/gannet", "client/config/gtk-3.0", "client/cache/thumbnails", "client/data"):
        shutil.rmtree(SCRATCH / d, ignore_errors=True)
    h = Headless("/tmp/gne2e-h")
    sr = ScreenReader()
    tour = Tour(out)
    media = SCRATCH / "media"
    shutil.rmtree(media, ignore_errors=True)
    tree(media, {"SECRETS/diary.txt": "dear diary"})
    disks = StandIn(media)
    try:
        disks.say("drive Stick Kingston-1234", "luks sdb2 Stick hunter2 SECRETS")
        main_window(h, sr, tour, disks)
        save_dialog(h, sr, tour)
        open_with(h, sr, tour)
    finally:
        if tour.orca:
            tour.orca.stop()
        sr.stop()
        disks.stop()
        subprocess.run(["pkill", "-f", str(e2e.BIN)], capture_output=True)
        h.stop()
    log = tour.orca.keep.read_text(errors="replace") if tour.orca else ""
    leaks = [w for w in ("hunter2", "nope") if w in log]
    tour.section("The passphrase")
    tour.note(f"typed “nope”, then “hunter2”; found in Orca's whole debug log: {', '.join(leaks) or 'neither'}")
    tour.flush()
    print(out)
    sys.exit(1 if leaks else 0)


def main_window(h, sr, tour, disks):
    fs = fresh_fs()
    docs = fs / "docs"
    tree(fs, {"docs/projects/README.md": "# Projects\n", "docs/a.txt": "a", "docs/b.txt": "bb", "docs/c.txt": "ccc", "docs/notes.md": "# Notes\n\nHello.\n",
              "docs/old/README.md": "# Old\n", "other/b.txt": "other b", "dst/x": "", "ro/x": ""})
    (docs / "photo.png").write_bytes(png_bytes(40, 30))
    (fs / "ro").chmod(0o500)
    env = {"DBUS_SESSION_BUS_ADDRESS": sr.session.address, "DBUS_SYSTEM_BUS_ADDRESS": disks.bus.address}
    probe = App(h, docs, launch=False, extra_env=env)
    data = Path(probe.env["XDG_DATA_HOME"])
    data.mkdir(parents=True, exist_ok=True)
    # Two recently used files of the same name, in two folders.
    marks = "".join(f'<bookmark href="file://{p}" added="{t}" modified="{t}" visited="{t}"/>' for p, t in [(docs / "projects/README.md", "2026-01-01T00:00:00Z"), (docs / "old/README.md", "2026-02-01T00:00:00Z")])
    (data / "recently-used.xbel").write_text(f'<?xml version="1.0"?><xbel version="1.0">{marks}</xbel>')
    # A long pinned sidebar.
    pins = [f"Pin{i:02}" for i in range(20)]
    for n in pins:
        (fs / "pins" / n).mkdir(parents=True)
    conf = bookmarks_file(probe)
    conf.parent.mkdir(parents=True, exist_ok=True)
    conf.write_text("".join((fs / "pins" / n).as_uri() + "\n" for n in pins))

    a = App(h, docs, extra_env=env)
    a.wait(lambda s: s["names"] == ["old", "projects", "a.txt", "b.txt", "c.txt", "notes.md", "photo.png"], "listed")
    sr.screen_reader(True)
    sr.until("apps", lambda x: x == ["Gannet"], "registered")
    tour.orca = Orca(sr, h, tour.out.with_name("orca-debug.out"))
    step = tour.step

    def dismiss():
        # Escape first clears a selection, then dismisses the newest notice.
        for _ in range(3):
            a.key("Escape")
            try:
                return a.wait(lambda s: not s["notices"], "dismissed", timeout=1)
            except AssertionError:
                pass
        raise AssertionError("the notice stayed")

    def go(path):
        a.key("l", mods=("ctrl",))
        a.type(str(path))
        a.key("Return")
        a.wait(lambda s: s["dir"] == str(path), str(path))

    tour.section("Starting, and moving through the rows")
    step("Orca starts with Gannet open on docs", settle=2)
    for name in ["old", "projects", "a.txt"]:
        step(f"Down (to {name})", lambda n=name: (a.key("Down"), a.wait(lambda s: s["selected"] == [n], n)))
    step("End", lambda: (a.key("End"), a.wait(lambda s: s["selected"] == ["photo.png"], "end")))
    step("Home", lambda: (a.key("Home"), a.wait(lambda s: s["selected"] == ["old"], "home")))
    step("Return (into old)", lambda: (a.key("Return"), a.wait(lambda s: s["dir"] == str(docs / "old"), "old")))
    step("Down", lambda: a.key("Down"))
    step("Backspace (up)", lambda: (a.key("BackSpace"), a.wait(lambda s: s["dir"] == str(docs), "docs")))

    tour.section("Selecting several with Ctrl+click")

    def ctrl_click(i):
        held = holding(a, "ctrl")
        time.sleep(0.15)
        a.click(ROW_X, row_y(i))
        held.wait()

    step("Click a.txt", lambda: (a.click(ROW_X, row_y(2)), a.wait(lambda s: s["selected"] == ["a.txt"], "a.txt")))
    step("Ctrl+click b.txt", lambda: (ctrl_click(3), a.wait(lambda s: s["selected"] == ["a.txt", "b.txt"], "two")))
    step("Ctrl+click c.txt", lambda: (ctrl_click(4), a.wait(lambda s: len(s["selected"]) == 3, "three")))
    step("Ctrl+click b.txt again (out of the selection)", lambda: (ctrl_click(3), a.wait(lambda s: len(s["selected"]) == 2, "two again")))

    tour.section("Sorting")
    step("Ctrl+2 (by size)", lambda: a.key("2", mods=("ctrl",)))
    step("Ctrl+2 again (reversed)", lambda: a.key("2", mods=("ctrl",)))
    step("Ctrl+1 (by name)", lambda: a.key("1", mods=("ctrl",)))

    tour.section("The path bar")
    step("Ctrl+L", lambda: (a.key("l", mods=("ctrl",)), time.sleep(0.2)))
    step(f"Type {fs / 'other'}", lambda: a.type(str(fs / "other")))
    step("Return", lambda: (a.key("Return"), a.wait(lambda s: s["dir"] == str(fs / "other"), "other")))
    step("Alt+Left (back)", lambda: (a.key("Left", mods=("alt",)), a.wait(lambda s: s["dir"] == str(docs), "back")))

    tour.section("Filter and search")
    step("/", lambda: (a.key("slash"), time.sleep(0.2)))
    step("Type “txt”", lambda: (a.type("txt"), a.wait(lambda s: s["names"] == ["a.txt", "b.txt", "c.txt"], "filtered")))
    step("Down (into the list)", lambda: a.key("Down"))
    step("Escape", lambda: (a.key("Escape"), time.sleep(0.3)))
    step("Ctrl+F", lambda: (a.key("f", mods=("ctrl",)), time.sleep(0.2)))
    step("Type “README”", lambda: (a.type("README"), a.wait(lambda s: len(s["names"]) == 2, "found")))
    step("Down (to a match)", lambda: a.key("Down"))
    step("Down (to the other match)", lambda: a.key("Down"))
    step("Escape", lambda: (a.key("Escape"), a.wait(lambda s: s["dir"] == str(docs) and "a.txt" in s["names"], "the folder again")))

    tour.section("Renaming")
    step("Type “a” (to a.txt)", lambda: go_to(a, "a.txt"))
    step("F2", lambda: (a.key("F2"), time.sleep(0.2)))
    step("Type “alpha”", lambda: a.type("alpha"))
    step("Return", lambda: (a.key("Return"), a.wait(lambda s: "alpha.txt" in s["names"], "renamed")))

    tour.section("Renaming several")
    step("Click alpha.txt, Ctrl+click b.txt and c.txt", lambda: (a.click(ROW_X, row_y(2)), ctrl_click(3), ctrl_click(4), a.wait(lambda s: len(s["selected"]) == 3, "three")))
    step("F2", lambda: (a.key("F2"), a.wait(lambda s: s["dialog"].startswith("rename-many"), "the dialog")))
    step("Type “.txt” in Find", lambda: a.type(".txt"))
    step("Tab (to Replace with)", lambda: a.key("Tab"))
    step("Type “.md”", lambda: a.type(".md"))
    step("Escape", lambda: (a.key("Escape"), a.wait(lambda s: s["dialog"] == "", "closed")))

    tour.section("Delete for good, cancelled")
    step("Type “c” (to c.txt)", lambda: go_to(a, "c.txt"))
    step("Shift+Delete", lambda: (a.key("Delete", mods=("shift",)), a.wait(lambda s: s["dialog"] == "delete", "asks")))
    step("Tab", lambda: a.key("Tab"))
    step("Escape", lambda: (a.key("Escape"), a.wait(lambda s: s["dialog"] == "", "cancelled")))

    tour.section("The context menu")
    step("Menu key", lambda: (a.key("Menu"), a.wait(lambda s: s["menu"], "menu")))
    step("Down", lambda: a.key("Down"))
    step("Down", lambda: a.key("Down"))

    def to_item(label):
        items = a.state()["menu_items"]
        # The keyboard starts on the first item, and has moved down two.
        at, want = 2, items.index(label)
        for _ in range(abs(want - at)):
            a.key("Down" if want > at else "Up")

    step("Down to Copy", lambda: to_item("Copy"))
    step("Return (Copy)", lambda: (a.key("Return"), a.wait(lambda s: not s["menu"], "closed")))

    tour.section("Pasting onto a file that's there")
    step("Ctrl+L, other, Return", lambda: go(fs / "other"))
    step("Ctrl+V (c.txt, new here)", lambda: (a.key("v", mods=("ctrl",)), a.wait(lambda s: "c.txt" in s["names"] and s["jobs"] == 0, "pasted")))
    step("Ctrl+V again (c.txt is there now)", lambda: (a.key("v", mods=("ctrl",)), a.wait(lambda s: s["dialog"] == "conflict", "the clash")), settle=1.5)
    tour.note("the paste is a job waiting on the question, so it shows in the panel")
    step("Tab", lambda: (time.sleep(0.5), a.key("Tab")))
    step("S (Skip): the job ends", lambda: (a.type("s"), a.wait(lambda s: s["dialog"] == "" and s["jobs"] == 0, "skipped")), settle=1.5)
    step("Ctrl+V once more", lambda: (a.key("v", mods=("ctrl",)), a.wait(lambda s: s["dialog"] == "conflict", "the clash")), settle=1.5)
    step("Escape (cancel): the job is stopped", lambda: (time.sleep(0.5), a.key("Escape"), a.wait(lambda s: s["dialog"] == "" and s["jobs"] == 0, "cancelled")), settle=1.5)
    step("Escape (dismiss the notice)", dismiss)
    step("Type “c”, Delete (to the Trash)", lambda: (go_to(a, "c.txt"), a.key("Delete"), a.wait(lambda s: "c.txt" not in s["names"], "trashed")))

    tour.section("Places: Recent, Trash, pins")
    places = [p.split("|")[0] for p in a.state()["places"]]
    tour.note(f"the sidebar: {', '.join(places)}")
    for label in ["Recent", "Trash"]:
        n = places.index(label) + 1
        step(f"Alt+{n} ({label})", lambda n=n: (a.key(str(n), mods=("alt",)), time.sleep(0.4)))
        step("Down", lambda: a.key("Down"))
        step("Down", lambda: a.key("Down"))
    step("Alt+9", lambda: (a.key("9", mods=("alt",)), time.sleep(0.4)))
    step("Alt+Home", lambda: (a.key("Home", mods=("alt",)), time.sleep(0.4)))

    tour.section("A failure's notice")
    step("Ctrl+L, ro (can't be written to), Return", lambda: go(fs / "ro"))
    step("Ctrl+V (refused)", lambda: (a.key("v", mods=("ctrl",)), a.wait(lambda s: s["notices"], "the notice", timeout=10)), settle=2)
    step("Escape (dismiss it)", dismiss)

    tour.section("The preview")
    step("Ctrl+L, docs, Return; type “photo”", lambda: (go(docs), go_to(a, "photo.png")))
    step("Space", lambda: (a.key("space"), a.wait(lambda s: s["previewing"] is not None, "preview")), settle=2)
    step("Left (notes.md)", lambda: a.key("Left"), settle=1.5)
    step("Right (photo.png)", lambda: a.key("Right"), settle=1.5)
    step("Escape", lambda: (a.key("Escape"), a.wait(lambda s: s["previewing"] is None, "closed")), settle=1.5)

    tour.section("Unlocking a drive")
    def click_drive():
        # Below the pins: the sidebar is scrolled to its end first.
        a.move(60, 400)
        a.scroll(20)
        s = a.wait(lambda s: s["side_scroll"] == s["side_max"] and place_flags(s, "8.0 GB Volume") == {"locked", "eject"}, "the drive")
        click_place(a, s, "8.0 GB Volume")
        a.wait(lambda s: s["dialog"] == "unlock", "asked")

    step("Scroll the sidebar, click the locked drive", click_drive)
    step("Type a wrong passphrase", lambda: a.type("nope"))
    step("Return", lambda: (a.key("Return"), a.wait(lambda s: s["dialog"] == "unlock-again", "wrong", timeout=10)), settle=1.5)
    step("Type the right one", lambda: a.type("hunter2"))
    step("Return", lambda: (a.key("Return"), a.wait(lambda s: s["dialog"] == "" and s["dir"] == str(SCRATCH / "media/SECRETS"), "opened", timeout=10)), settle=1.5)
    a.close()
    (fs / "ro").chmod(0o755)


def other_window(h, sr, args, extra_env=None):
    """One of Gannet's other windows, as the portal starts it."""
    a = App(h, None, launch=False, extra_env={"DBUS_SESSION_BUS_ADDRESS": sr.session.address, **(extra_env or {})})
    a.proc = subprocess.Popen(sandboxed(args), env=a.env, stderr=open(SCRATCH / "stderr.log", "a"), stdout=subprocess.PIPE)
    h.wait_window(10)
    return a


def save_dialog(h, sr, tour):
    fs = fresh_fs()
    docs = fs / "docs"
    tree(fs, {"docs/sub/x.txt": "x", "docs/a.txt": "a"})
    step = tour.step
    tour.section("The Save dialog")
    a = None

    def start():
        nonlocal a
        a = other_window(h, sr, ["--choose", "save", "--title", "Save the report", "--folder", str(docs), "--name", "report.txt", "--filter", "Text", "--glob", "*.txt", "--filter", "All files", "--glob", "*"])
        a.wait(lambda s: s["names"] == ["sub", "a.txt"], "listed")

    step("A program asks to save “report.txt”", start, settle=2)
    step("Ctrl+A, type “a.txt”", lambda: (a.key("a", mods=("ctrl",)), a.type("a.txt")))
    step("Return (a.txt is there)", lambda: (a.key("Return"), a.wait(lambda s: s["dialog"] == "replace", "Replace?")), settle=1.5)
    step("Escape", lambda: (a.key("Escape"), a.wait(lambda s: s["dialog"] == "", "back")))
    step("Ctrl+A, type “report.txt”", lambda: (a.key("a", mods=("ctrl",)), a.type("report.txt")))
    step("Return (saved)", lambda: (a.key("Return"), a.proc.wait(5)), settle=1.5)


def open_with(h, sr, tour):
    share, _ = stand_in_apps()
    config = SCRATCH / "client/config"
    config.mkdir(parents=True, exist_ok=True)
    (config / "mimeapps.list").write_text("[Default Applications]\ntext/markdown=marked.desktop\n")
    (SCRATCH / "state.json.openwith").unlink(missing_ok=True)
    step = tour.step
    tour.section("Open With")
    a = None

    def start():
        nonlocal a
        a = other_window(h, sr, ["--open-with", "--what", "“notes.md”", "--type", "text/markdown", "--offer-always"], {"XDG_DATA_DIRS": str(share)})

    step("Open with… for notes.md", start, settle=2)
    step("Down", lambda: a.key("Down"))
    step("Down", lambda: a.key("Down"))
    step("Type “wri”", lambda: a.type("wri"))
    step("Escape (clears the search)", lambda: a.key("Escape"))
    step("Space (Always use…)", lambda: a.key("space"))
    step("Return (open)", lambda: (a.key("Return"), a.proc.wait(5)), settle=1.5)


if __name__ == "__main__":
    main()
