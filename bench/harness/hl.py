"""Headless compositor helper shared by the bench and end-to-end checks.

Starts bench/harness/headless.sh (sandboxed sway on the pixman renderer), talks to
it over sway IPC, and gives clients an environment isolated from the real session:
HOME and every XDG variable point into a scratch root, the session bus is cut off.
"""
import json, os, shutil, signal, socket, struct, subprocess, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
MAGIC = b"i3-ipc"


class Ipc:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(str(path))

    def send(self, kind, payload=b""):
        self.s.sendall(MAGIC + struct.pack("<II", len(payload), kind) + payload)

    def recv(self, timeout=None):
        self.s.settimeout(timeout)
        hdr = self._read(14)
        n, kind = struct.unpack("<II", hdr[6:])
        return kind, json.loads(self._read(n))

    def _read(self, n):
        b = b""
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c:
                raise EOFError
            b += c
        return b

    def call(self, kind, payload=b""):
        self.send(kind, payload)
        return self.recv(5)[1]


class Headless:
    def __init__(self, root, size="1280x800", keep_keyboard=True):
        root = Path(root)
        assert root.is_absolute() and str(root).startswith("/tmp/gn"), root
        self.root = root
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        self.log = open(root.parent / (root.name + ".log"), "w")
        self.proc = subprocess.Popen([str(HERE / "headless.sh"), str(root), size], stdout=self.log, stderr=self.log, start_new_session=True)
        runtime = root / "runtime"
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            socks = list(runtime.glob("sway-ipc*")) if runtime.exists() else []
            if socks and (runtime / "wayland-1").exists():
                self.ipc_path = socks[0]
                break
            if self.proc.poll() is not None:
                raise RuntimeError("sway exited, see " + self.log.name)
            time.sleep(0.01)
        else:
            raise RuntimeError("sway did not start")
        time.sleep(0.1)
        self.cmd = Ipc(self.ipc_path)
        self.events = Ipc(self.ipc_path)
        self.events.call(2, b'["window"]')
        # With no keyboard, the seat drops that capability and the next wtype's first keys
        # arrive before a client can rebind. A silent keyboard held open avoids that.
        self.keeper = None
        if keep_keyboard:
            env = {"PATH": "/usr/bin", "XDG_RUNTIME_DIR": str(runtime), "WAYLAND_DISPLAY": "wayland-1"}
            self.keeper = subprocess.Popen(["wtype", "-s", "86400000"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.05)
        self.vptr = None
        tool = HERE.parent.parent / "target/vptr/release/vptr"
        if tool.exists():
            env = {"PATH": "/usr/bin", "XDG_RUNTIME_DIR": str(runtime), "WAYLAND_DISPLAY": "wayland-1"}
            self.vptr = subprocess.Popen([str(tool)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
            self.vptr.stdout.readline()

    def pointer(self, command):
        self.vptr.stdin.write(command + "\n")
        self.vptr.stdin.flush()
        self.vptr.stdout.readline()

    def env(self, scratch, extra=None):
        """A client environment with nothing pointing at the real session."""
        scratch = Path(scratch)
        assert scratch.is_absolute() and not str(scratch).startswith(os.path.expanduser("~") + "/."), scratch
        e = {"PATH": "/usr/bin", "LANG": os.environ.get("LANG", "C.UTF-8"), "XDG_RUNTIME_DIR": str(self.root / "runtime"), "WAYLAND_DISPLAY": "wayland-1",
             "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent", "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=/nonexistent", "XDG_SESSION_TYPE": "wayland", "GTK_USE_PORTAL": "0", "QT_FORCE_STDERR_LOGGING": "1"}
        for k, d in {"HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state"}.items():
            p = scratch / d
            p.mkdir(parents=True, exist_ok=True)
            e[k] = str(p)
        e.update(extra or {})
        return e

    def wait_window(self, timeout=60):
        """Blocks until sway reports a new mapped window; returns its monotonic time."""
        while True:
            _, ev = self.events.recv(timeout)
            if ev.get("change") == "new":
                return time.monotonic()

    def tool(self, argv, env):
        return subprocess.run(argv, env=env, capture_output=True, timeout=20)

    def stop(self):
        if getattr(self, "keeper", None):
            self.keeper.kill()
            self.keeper.wait()
            self.keeper = None
        if getattr(self, "vptr", None):
            self.vptr.kill()
            self.vptr.wait()
            self.vptr = None
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        self.proc.wait(5)
        self.log.close()


def pss_kib(pid):
    total = 0
    for p in tree(pid):
        try:
            for line in open(f"/proc/{p}/smaps_rollup"):
                if line.startswith("Pss:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total


def cpu_seconds(pid):
    tick = os.sysconf("SC_CLK_TCK")
    total = 0
    for p in tree(pid):
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            total += int(f[11]) + int(f[12])
        except OSError:
            pass
    return total / tick


def tree(pid):
    out = [pid]
    try:
        for t in os.listdir(f"/proc/{pid}/task"):
            for c in open(f"/proc/{pid}/task/{t}/children").read().split():
                out += tree(int(c))
    except OSError:
        pass
    return out


def median(v):
    v = sorted(v)
    return v[len(v) // 2]
