//! Scratch folders for tests. All destructive tests must live in one of these.

use std::path::{Path, PathBuf};
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
