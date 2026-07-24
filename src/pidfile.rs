// SPDX-License-Identifier: GPL-3.0-or-later
//! A pidfile, so another process can find this compositor to signal it.
//!
//! The settings apps apply a change by writing `compositor.toml` and asking the running
//! compositor to reload with `SIGHUP` (see [`crate::signals`]). For that they need the
//! pid, and a sibling process cannot read it from the environment -- so the compositor
//! drops it in a well-known file next to its log, under the per-user runtime directory.
//!
//! The file is removed on a clean exit via the returned [`Guard`]. A crash leaves it
//! stale; a reader should treat "no such process" as "not running" rather than trusting
//! the file blindly.

use std::path::PathBuf;

use tracing::{info, warn};

/// Named for the process, beside `wlrix-compositor.log`.
const PID_NAME: &str = "wlrix-compositor.pid";

/// `$XDG_RUNTIME_DIR` (owned by one user, cleaned up on logout), else the temp dir --
/// the same rule the log file follows, so the two sit together.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// Where the pidfile lives.
pub fn path() -> PathBuf {
    runtime_dir().join(PID_NAME)
}

/// Write this process's pid. Returns a guard that removes the file when dropped; failure
/// to write is logged and swallowed, since a missing pidfile is not worth refusing to
/// start over -- it only costs the settings apps their live-reload.
pub fn write() -> Option<Guard> {
    let path = path();
    match create(&path).and_then(|mut file| {
        use std::io::Write;
        write!(file, "{}", std::process::id())
    }) {
        Ok(()) => {
            info!(path = %path.display(), "wrote pidfile");
            Some(Guard { path })
        }
        Err(err) => {
            warn!(?err, path = %path.display(), "could not write pidfile");
            None
        }
    }
}

/// Create (or truncate) the pidfile. `O_NOFOLLOW` for the same reason as the log:
/// the path is predictable, and the temp-dir fallback is world-writable.
fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Removes the pidfile on drop, so a live pidfile means a live compositor.
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            warn!(?err, path = %self.path.display(), "could not remove pidfile");
        }
    }
}
