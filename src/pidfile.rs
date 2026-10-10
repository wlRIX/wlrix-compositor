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
//!
//! # One session, possibly two compositors
//!
//! The path is fixed, but the compositor is not always alone: a nested one started for testing
//! inside a live wlRIX session shares its runtime directory. Left to itself, the nested one
//! would overwrite the live session's pidfile on startup and delete it on exit, and from then on
//! every `compositor.toml` change would silently stop applying to the compositor the user is
//! actually sitting in. So the file belongs to whoever took it first:
//!
//! - **On startup**, a file that names another *live compositor* is left alone, and this one
//!   runs without a pidfile. A file naming a dead process, or a live process that is not a
//!   compositor -- a pid reused after a crash -- is stale and is taken over.
//! - **On exit**, the file is removed only if it still names this process.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// Named for the process, beside `wlrix-compositor.log`.
const PID_NAME: &str = "wlrix-compositor.pid";

/// What the kernel calls this program in `/proc/<pid>/comm`: its name cut to the 15 bytes
/// `TASK_COMM_LEN` leaves room for.
const COMM: &str = "wlrix-composito";

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

/// Write this process's pid, unless another running compositor already holds the file.
///
/// Returns a guard that removes the file when dropped, or `None` when nothing was written.
/// Failure to write is logged and swallowed, since a missing pidfile is not worth refusing to
/// start over -- it only costs the settings apps their live-reload.
pub fn write() -> Option<Guard> {
    claim(&path(), std::process::id(), is_compositor)
}

/// [`write`], with the path, this process's pid and the "is that a running compositor?" test
/// passed in, so the decisions can be tested without a second compositor.
fn claim(path: &Path, me: u32, is_compositor: impl Fn(u32) -> bool) -> Option<Guard> {
    if let Some(holder) = read(path)
        && holder != me
        && is_compositor(holder)
    {
        // Another compositor got here first -- the live session, when this one is nested.
        // Taking the file would point the settings apps at the wrong compositor, and removing
        // it on the way out would point them at none.
        info!(
            path = %path.display(),
            holder,
            "another compositor holds the pidfile; running without one"
        );
        return None;
    }

    match create(path).and_then(|mut file| {
        use std::io::Write;
        write!(file, "{me}")
    }) {
        Ok(()) => {
            info!(path = %path.display(), "wrote pidfile");
            Some(Guard {
                path: path.to_owned(),
                pid: me,
            })
        }
        Err(err) => {
            warn!(?err, path = %path.display(), "could not write pidfile");
            None
        }
    }
}

/// The pid a pidfile names, if it exists and names one.
fn read(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether `pid` is a running wlRIX compositor.
///
/// By name rather than by "is anything alive at that pid": a pidfile left by a crash names a
/// pid the kernel may since have handed to something else entirely, and deferring to that would
/// leave the session without a pidfile for no reason.
fn is_compositor(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim_end() == COMM)
}

/// Create (or truncate) the pidfile. `O_NOFOLLOW` for the same reason as the log:
/// the path is predictable, and the temp-dir fallback is world-writable.
fn create(path: &Path) -> std::io::Result<std::fs::File> {
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
    pid: u32,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Only if it is still ours. Another compositor that found this one dead -- or a crash
        // and restart while this process was wedged -- may have written its own pid since, and
        // removing that would orphan a compositor that is still running.
        if read(&self.path) != Some(self.pid) {
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            warn!(?err, path = %self.path.display(), "could not remove pidfile");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pidfile path in a fresh directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wlrix-compositor-pidfile-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(PID_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ME: u32 = 2000;
    const LIVE: u32 = 1000;

    #[test]
    fn an_absent_file_is_written_and_removed_on_exit() {
        let scratch = Scratch::new("absent");
        let guard = claim(&scratch.file(), ME, |_| false).expect("should claim the file");
        assert_eq!(read(&scratch.file()), Some(ME));
        drop(guard);
        assert!(!scratch.file().exists());
    }

    #[test]
    fn a_live_compositors_file_is_left_alone() {
        // The nested-compositor case: the live session's compositor got there first.
        let scratch = Scratch::new("live");
        std::fs::write(scratch.file(), LIVE.to_string()).unwrap();
        assert!(claim(&scratch.file(), ME, |pid| pid == LIVE).is_none());
        assert_eq!(read(&scratch.file()), Some(LIVE));
    }

    #[test]
    fn a_stale_file_is_taken_over() {
        // Left by a crash: the pid is dead, or now belongs to something that is not a
        // compositor. Either way it is nobody's.
        let scratch = Scratch::new("stale");
        std::fs::write(scratch.file(), LIVE.to_string()).unwrap();
        let _guard = claim(&scratch.file(), ME, |_| false).expect("should take over");
        assert_eq!(read(&scratch.file()), Some(ME));
    }

    #[test]
    fn a_garbled_file_is_taken_over() {
        let scratch = Scratch::new("garbled");
        std::fs::write(scratch.file(), "not a pid").unwrap();
        let _guard = claim(&scratch.file(), ME, |_| true).expect("should take over");
        assert_eq!(read(&scratch.file()), Some(ME));
    }

    #[test]
    fn a_file_naming_this_process_is_rewritten() {
        let scratch = Scratch::new("ours");
        std::fs::write(scratch.file(), ME.to_string()).unwrap();
        assert!(claim(&scratch.file(), ME, |_| true).is_some());
    }

    #[test]
    fn a_file_somebody_else_has_since_taken_is_not_removed() {
        let scratch = Scratch::new("taken");
        let guard = claim(&scratch.file(), ME, |_| false).unwrap();
        std::fs::write(scratch.file(), LIVE.to_string()).unwrap();
        drop(guard);
        assert_eq!(read(&scratch.file()), Some(LIVE));
    }

    #[test]
    fn the_comm_name_is_the_program_name_as_the_kernel_cuts_it() {
        // `TASK_COMM_LEN` is 16 including the terminator. Kept in step by hand with the binary's
        // name; a rename that missed this would make every compositor look stale to the next.
        assert_eq!(COMM, &"wlrix-compositor"[..15]);
        assert!(!is_compositor(u32::MAX));
    }
}
