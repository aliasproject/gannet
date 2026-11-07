//! Opening files with the default app, through `xdg-open`.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Starts `xdg-open` in its own session. `on_fail` runs on a helper thread if it can't
/// start or reports failure, with a reason for the user.
pub fn open_with_default(path: &Path, on_fail: impl FnOnce(PathBuf, String) + Send + 'static) {
    let path = path.to_path_buf();
    run_detached(Command::new("xdg-open").arg(&path), move |reason| on_fail(path, reason));
}

/// Starts another program, on its own, with its output discarded.
pub fn spawn_detached(cmd: &mut Command, on_fail: impl FnOnce(String) + Send + 'static) {
    run_detached(cmd, on_fail);
}

fn run_detached(cmd: &mut Command, on_fail: impl FnOnce(String) + Send + 'static) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = std::thread::Builder::new().name("gannet-wait".into()).spawn(move || match child.wait() {
                Ok(s) if s.success() => {}
                Ok(s) => on_fail(match s.code() {
                    Some(3) => "No app is set up for this type of file".to_string(),
                    Some(c) => format!("The opener reported an error (code {c})"),
                    None => "The opener was stopped".to_string(),
                }),
                Err(e) => on_fail(crate::listing::describe(&e)),
            });
        }
        Err(e) => on_fail(crate::listing::describe(&e)),
    }
}
