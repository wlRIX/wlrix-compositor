// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the compositor's logs go.
//!
//! Two destinations. stderr, as before -- which is useful when running nested, and free
//! when running under the session, which passes it through. And a file, because on a
//! TTY stderr is written to a console that the compositor itself is covering: the text
//! is there but nothing can be seen of it, so a session that goes wrong leaves no
//! evidence. The file is what makes a bad boot diagnosable afterwards.
//!
//! Truncated at startup. These are for working out what this run did, and a log that
//! grows across boots buries that under history.

use std::{fs::File, path::PathBuf, sync::Arc};

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Named for the process rather than the session, so the compositor's log and the
/// session's sit side by side and neither has to guess about the other.
const LOG_NAME: &str = "wlrix-compositor.log";

/// Set up logging, returning the file being written to if there is one.
pub fn init() -> Option<PathBuf> {
    // Same default as before: `RUST_LOG` if set, otherwise info.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (path, file) = match open_log() {
        Some((path, file)) => (Some(path), Some(file)),
        None => (None, None),
    };

    // No color in the file: these get read with `grep`, and escape codes make that
    // needlessly awkward.
    let file_layer = file.map(|file| fmt::layer().with_ansi(false).with_writer(Arc::new(file)));

    // The file layer goes on first deliberately. Two `fmt` layers share the formatted
    // span fields cached on each span, so whichever formats them first decides whether
    // they carry color -- and a log file full of escape codes is a nuisance to read.
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    path
}

/// Where the log lives: the per-user runtime directory if there is one, else the temp
/// directory.
///
/// `$XDG_RUNTIME_DIR` (`/run/user/<uid>`) is owned by one user and cleaned up on logout,
/// so the greeter's log and the session's do not collide in `/tmp` -- there, the
/// greeter (running as `greeter`) leaves a file the session's own user cannot truncate.
fn log_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// Create (or truncate) the log file.
fn open_log() -> Option<(PathBuf, File)> {
    let path = log_dir().join(LOG_NAME);
    match crate::logging::create(&path) {
        Ok(file) => Some((path, file)),
        Err(err) => {
            // Logging is not worth failing to start over; stderr still works.
            eprintln!("wlrix-compositor: could not open {}: {err}", path.display());
            None
        }
    }
}

/// Open for writing, truncating what is there.
///
/// `O_NOFOLLOW` because the path is predictable and lives in a world-writable
/// directory: without it, anyone could leave a symlink there and have the compositor
/// truncate a file of their choosing.
fn create(path: &std::path::Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
