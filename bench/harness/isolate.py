"""Keeps a test run away from the desktop's own buses.

Every harness that starts a bus, a screen reader or a toolkit app (tests/e2e.py,
tests/orca_tour.py, bench/run.py, bench/bakeoff/run.py) calls `isolate()` first. Outside the sandbox, it records the real session's
bus sockets and runs the script again inside bubblewrap, where:

- /run/user/$UID is an empty tmpfs, and XDG_RUNTIME_DIR points at it, so anything that
  starts a bus or an at-spi-bus-launcher puts its socket there and never replaces the
  desktop's `at-spi/bus_0` or `bus`;
- /tmp/.X11-unix is empty, DISPLAY and WAYLAND_DISPLAY are unset, and the network is
  unshared, so nothing reaches Xwayland, even through its abstract socket;
- DBUS_SESSION_BUS_ADDRESS points nowhere and AT_SPI_BUS_ADDRESS is unset.

Inside, it checks that the sandbox is really there and stops the run if it isn't. When the
run ends, the outer half checks the real sockets again, and fails the run loudly if any was
replaced, removed or added, even when every check passed.

  python3 -B bench/harness/isolate.py --self-test
"""
import os, shutil, signal, socket, subprocess, sys, tempfile
from pathlib import Path

MARK = "GANNET_ISOLATED"
NOWHERE = "unix:path=/nonexistent"


def real_runtime():
    """The desktop's runtime folder, where its session bus and accessibility bus live."""
    return Path(os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.geteuid()}")


def fingerprint(runtime):
    """Each bus socket in `runtime` with its inode and times. A replaced socket has a new inode."""
    out = {}
    paths = [runtime / "bus", runtime / "at-spi"]
    if (runtime / "at-spi").is_dir():
        paths += sorted((runtime / "at-spi").iterdir())
    for p in paths:
        try:
            st = os.lstat(p)
        except FileNotFoundError:
            continue
        out[str(p)] = (st.st_ino, st.st_mtime_ns, st.st_ctime_ns)
    return out


def changes(before, after):
    """What happened to the sockets between two fingerprints, one line each."""
    out = []
    for p in sorted(set(before) | set(after)):
        if p not in after:
            out.append(f"{p} was removed")
        elif p not in before:
            out.append(f"{p} appeared")
        elif before[p][0] != after[p][0]:
            out.append(f"{p} was replaced (inode {before[p][0]} -> {after[p][0]})")
        elif before[p] != after[p]:
            out.append(f"{p} changed")
    return out


def problems():
    """Why this process isn't safely sandboxed; empty when it is."""
    uid = os.geteuid()
    out = []
    rt = os.environ.get("XDG_RUNTIME_DIR", "")
    for p in (f"/run/user/{uid}/bus", f"/run/user/{uid}/at-spi", f"/run/user/{uid}/wayland-0", f"/run/user/{uid}/wayland-1"):
        if os.path.lexists(p):
            out.append(f"the desktop's {p} is visible")
    if rt and rt != f"/run/user/{uid}":
        # Anywhere else may be a real runtime folder left over from somewhere.
        out.append(f"XDG_RUNTIME_DIR is {rt}, not the sandbox's empty /run/user/{uid}")
    if os.path.isdir("/tmp/.X11-unix") and any(Path("/tmp/.X11-unix").iterdir()):
        out.append("X11 sockets in /tmp/.X11-unix are visible")
    for k in ("DISPLAY", "WAYLAND_DISPLAY", "AT_SPI_BUS_ADDRESS"):
        if os.environ.get(k):
            out.append(f"{k} is set")
    if os.environ.get("DBUS_SESSION_BUS_ADDRESS", NOWHERE) != NOWHERE:
        out.append("DBUS_SESSION_BUS_ADDRESS points at a bus")
    return out


def sandbox_command(argv):
    uid = os.geteuid()
    if not os.path.isdir(f"/run/user/{uid}"):
        # bwrap would have to make it on the real disk, and can't. That's a machine with no
        # logind session (ssh, a container), where the harnesses' headless sway can't run either.
        sys.exit(f"refusing to run: /run/user/{uid} doesn't exist, so there's no runtime folder to cover with the sandbox's own")
    bwrap = shutil.which("bwrap") or "/usr/bin/bwrap"
    # Only covered if it's there: bwrap would otherwise make it on the real /tmp, owned by
    # this user, and Xwayland refuses a /tmp/.X11-unix it doesn't trust.
    x11 = ["--tmpfs", "/tmp/.X11-unix"] if os.path.isdir("/tmp/.X11-unix") else []
    return [bwrap, "--die-with-parent", "--unshare-pid", "--unshare-net", "--dev-bind", "/", "/", "--proc", "/proc",
            "--perms", "0700", "--tmpfs", f"/run/user/{uid}", *x11,
            "--setenv", MARK, "1", "--setenv", "XDG_RUNTIME_DIR", f"/run/user/{uid}",
            "--setenv", "DBUS_SESSION_BUS_ADDRESS", NOWHERE, "--setenv", "DBUS_SYSTEM_BUS_ADDRESS", NOWHERE,
            "--unsetenv", "DISPLAY", "--unsetenv", "WAYLAND_DISPLAY", "--unsetenv", "AT_SPI_BUS_ADDRESS",
            "--unsetenv", "DBUS_STARTER_ADDRESS", "--unsetenv", "DBUS_STARTER_BUS_TYPE", *argv]


def _sandboxed_processes(bwrap_pid):
    """The processes running inside the sandbox: bwrap's descendants that aren't bwrap."""
    out, todo = [], [bwrap_pid]
    while todo:
        pid = todo.pop()
        try:
            for task in os.listdir(f"/proc/{pid}/task"):
                with open(f"/proc/{pid}/task/{task}/children") as f:
                    todo += [int(c) for c in f.read().split()]
            if pid != bwrap_pid:
                with open(f"/proc/{pid}/comm") as f:
                    if f.read().strip() != "bwrap":
                        out.append(pid)
        except OSError:
            pass
    return out


def run_guarded(argv, runtime=None):
    """Runs argv in the sandbox, then fails if the real sockets changed. Returns the exit code.
    The sandbox only keeps argv off the desktop's buses and display: files, the real home
    among them, are as writable as they are outside it."""
    runtime = runtime or real_runtime()
    before = fingerprint(runtime)
    # bwrap has its own session, so the terminal's Ctrl-C doesn't reach it: bwrap would die at
    # once and take the sandbox with it, cutting off the harness's cleanup. The signal is passed
    # to what runs inside instead, and the check comes after the run has stopped. A second
    # Ctrl-C kills the whole run.
    proc = subprocess.Popen(sandbox_command(argv), start_new_session=True)

    def forward(signum, _frame):
        for pid in _sandboxed_processes(proc.pid):
            try:
                if os.getpgid(pid) == proc.pid:
                    os.kill(pid, signum)
            except OSError:
                pass

    old = {s: signal.signal(s, forward) for s in (signal.SIGTERM, signal.SIGHUP)}
    interrupted = False
    try:
        while True:
            try:
                code = proc.wait()
                break
            except KeyboardInterrupt:
                if interrupted:
                    os.killpg(proc.pid, signal.SIGKILL)
                else:
                    forward(signal.SIGINT, None)
                interrupted = True
    finally:
        for s, h in old.items():
            signal.signal(s, h)
    if code < 0:
        # Killed by a signal: report it as a shell would, never as a pass.
        code = 128 - code
    changed = changes(before, fingerprint(runtime))
    if changed:
        bar = "!" * 78
        print(f"\n{bar}\nTHE DESKTOP'S BUS SOCKETS CHANGED DURING THIS RUN:", file=sys.stderr)
        for line in changed:
            print("  " + line, file=sys.stderr)
        print("Unless the desktop itself started or restarted a bus meanwhile, something in the run\n"
              "reached the real runtime folder. If bus_0 was replaced, a screen reader will fail to\n"
              "connect until the bus is restarted; see the README's Development section.\n" + bar, file=sys.stderr)
        return code or 3
    return code


def isolate():
    """Call first thing in a harness's main(). Returns inside the sandbox; outside, never returns."""
    if os.environ.get(MARK) == "1":
        bad = problems()
        if bad:
            sys.exit("refusing to run, the sandbox isn't isolating the desktop's buses:\n  " + "\n  ".join(bad))
        return
    if not shutil.which("bwrap"):
        sys.exit("refusing to run: needs bubblewrap (bwrap) to keep the run off the desktop's buses")
    # orig_argv keeps the interpreter's flags, like -B.
    sys.exit(run_guarded([sys.executable, *sys.orig_argv[1:]]))


def self_test():
    """Shows that the guard notices a replaced socket and that the sandbox hides the real ones.
    Writes nothing outside a scratch folder and the sandbox's own tmpfs."""
    # In /tmp, not TMPDIR: a socket path must fit in 108 bytes.
    scratch = Path(tempfile.mkdtemp(prefix="gnisolate-", dir="/tmp"))
    try:
        rt = scratch / "runtime"
        (rt / "at-spi").mkdir(parents=True)
        socks = []
        for p in (rt / "bus", rt / "at-spi/bus_0"):
            s = socket.socket(socket.AF_UNIX)
            s.bind(str(p))
            socks.append(s)
        before = fingerprint(rt)
        assert changes(before, fingerprint(rt)) == [], "an untouched folder shows no changes"
        # What at-spi-bus-launcher does when it starts: unlink the old socket and bind a new one.
        os.unlink(rt / "at-spi/bus_0")
        s = socket.socket(socket.AF_UNIX)
        s.bind(str(rt / "at-spi/bus_0"))
        socks.append(s)
        got = changes(before, fingerprint(rt))
        assert any("at-spi/bus_0 was replaced" in line for line in got), got
        # A guarded run that does the same in the scratch "runtime" fails, even though it exits 0.
        print("(the warning below is the self-test checking that a guarded run fails)", file=sys.stderr)
        code = run_guarded(["/bin/sh", "-c", f"rm -f {rt}/at-spi/bus_1 && : > {rt}/at-spi/bus_1"], runtime=rt)
        assert code == 3, f"a run that added a socket exited {code}, not 3"
        # Inside the sandbox the real sockets can't be seen, and the check agrees.
        r = subprocess.run(sandbox_command([sys.executable, "-I", "-B", "-c", f"import sys; sys.path.insert(0, {str(Path(__file__).parent)!r}); import isolate; print(isolate.problems())"]),
                           capture_output=True, text=True)
        assert r.returncode == 0 and r.stdout.strip() == "[]", (r.returncode, r.stdout, r.stderr)
        # And a file made in the runtime folder's at-spi lands in the sandbox's tmpfs, never the
        # real one. It's a new name, never bus_0, and it's only made once the sandbox checks out.
        real = real_runtime()
        probe = real / "at-spi/gannet-isolate-probe"
        inner = (f"import os, sys; sys.path.insert(0, {str(Path(__file__).parent)!r}); import isolate; "
                 "sys.exit(1) if isolate.problems() else None; "
                 "d = os.environ['XDG_RUNTIME_DIR'] + '/at-spi'; os.makedirs(d, exist_ok=True); open(d + '/gannet-isolate-probe', 'w').close()")
        try:
            r = subprocess.run(sandbox_command([sys.executable, "-I", "-B", "-c", inner]))
            assert r.returncode == 0, f"the probe run exited {r.returncode}"
            assert not probe.exists(), "the sandbox let a write reach the real runtime folder"
        finally:
            # Only if the sandbox failed; never left behind to trip the next guarded run.
            probe.unlink(missing_ok=True)
        for s in socks:
            s.close()
        print("isolate self-test passed")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    else:
        sys.exit("usage: python3 -B bench/harness/isolate.py --self-test")
