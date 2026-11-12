//! Scratch folders for tests. All destructive tests must live in one of these.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct Scratch(pub PathBuf);

impl Scratch {
    /// A new empty folder under the system temp directory. Panics unless the path
    /// looks like a scratch path: tests must never run against real files.
    pub fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // scripts/sandbox-run.sh sets this, inside bubblewrap with the real home hidden.
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh, not plain cargo test");
        let base = std::env::temp_dir();
        assert!(base.is_absolute(), "temp dir must be absolute: {base:?}");
        let dir = base.join(format!("gannet-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn file(&self, name: &str, content: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A bus of our own, so tests never touch the real session bus.
pub struct PrivateBus {
    daemon: Child,
    pub address: String,
    _dir: Scratch,
}

impl PrivateBus {
    pub fn start() -> Self {
        let dir = Scratch::new();
        // A config of our own, so nothing from the system's bus setup comes along.
        let config = dir.path().join("bus.conf");
        std::fs::write(&config, format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
<allow send_destination="*" eavesdrop="true"/>
<allow eavesdrop="true"/>
<allow own="*"/>
  </policy>
</busconfig>
"#,
            dir.path().join("bus").display()
        ))
        .unwrap();
        let daemon = Command::new("dbus-daemon")
            .args(["--nofork", "--print-address=1", &format!("--config-file={}", config.display())])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon must be installed to run the D-Bus tests");
        // Owned before anything can fail, so the daemon is always stopped.
        let mut bus = Self { daemon, address: String::new(), _dir: dir };
        let mut line = String::new();
        BufReader::new(bus.daemon.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert!(!line.trim().is_empty(), "dbus-daemon printed no address");
        bus.address = line.trim().to_string();
        bus
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
