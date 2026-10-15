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
//!
//! Panics go to the file as well; see [`install_panic_hook`].

use std::{
    backtrace::Backtrace, fs::File, io::Write, panic::PanicHookInfo, path::PathBuf, sync::Arc,
};

use tracing_subscriber::{
    EnvFilter,
    fmt::{
        self,
        format::Writer,
        time::{FormatTime, SystemTime},
    },
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

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

    let file = file.map(Arc::new);
    if let Some(file) = &file {
        install_panic_hook(file.clone());
    }

    // No color in the file: these get read with `grep`, and escape codes make that
    // needlessly awkward.
    let file_layer = file.map(|file| fmt::layer().with_ansi(false).with_writer(file));

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

/// Write every panic into the log file, then hand it on to the hook that was there before.
///
/// The default hook prints only to stderr, and stderr is the console the compositor covers
/// (see the module docs) -- so a panic, the one thing most worth reading after a crash, was
/// the one thing that never reached the file. Getting it back meant digging the message out
/// of a core dump.
///
/// Written straight to the file rather than through `tracing`: `RUST_LOG` could filter an
/// event out, and the stderr layer would print it a second time next to the default hook's
/// own report. The backtrace is captured unconditionally, without waiting on
/// `RUST_BACKTRACE`, which is never set on a real session. That matters most for a panic
/// inside a wayland-backend destructor: it cannot unwind out of the C callback, so the
/// process aborts, and by the time a core dump is taken the frames that panicked are gone
/// from the stack. Only this hook still sees them.
///
/// The previous hook runs afterwards, so stderr gets the usual report as well.
fn install_panic_hook(file: Arc<File>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing to do if this fails: the hook must not panic, and stderr follows.
        let _ = (&*file).write_all(panic_report(info).as_bytes());
        previous(info);
    }));
}

/// The text [`install_panic_hook`] writes for one panic, in the shape of the other lines in
/// the file so it sorts and greps alongside them.
fn panic_report(info: &PanicHookInfo<'_>) -> String {
    let mut report = String::new();
    // `SystemTime` is the timer the `fmt` layers use, so the timestamp matches theirs.
    let _ = SystemTime.format_time(&mut Writer::new(&mut report));
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let location = info
        .location()
        .map(|location| location.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string payload>");
    report.push_str(&format!(
        " ERROR wlrix_compositor::panic: thread '{thread}' panicked at {location}: {message}\n"
    ));
    report.push_str(&format!(
        "stack backtrace:\n{}\n",
        Backtrace::force_capture()
    ));
    report
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic reaches the file with its message, where it happened, and a backtrace.
    ///
    /// The hook is process-wide, so it is taken back down afterwards; a test elsewhere that
    /// panics in the meantime only adds its own report to this scratch file.
    #[test]
    fn a_panic_is_written_to_the_log_file() {
        let path =
            std::env::temp_dir().join(format!("wlrix-compositor-panic-{}.log", std::process::id()));
        let file = Arc::new(create(&path).unwrap());
        install_panic_hook(file);

        let line = line!() + 3;
        let result = std::thread::Builder::new()
            .name("doomed".to_string())
            .spawn(|| panic!("deliberate test panic: {}", 42))
            .unwrap()
            .join();
        drop(std::panic::take_hook());

        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
        assert!(log.contains(&format!(
            " ERROR wlrix_compositor::panic: thread 'doomed' panicked at {}:{line}:",
            file!()
        )));
        assert!(log.contains("deliberate test panic: 42\n"));
        assert!(log.contains("stack backtrace:\n"));
    }
}
