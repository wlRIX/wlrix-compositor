// SPDX-License-Identifier: GPL-3.0-or-later
//! Scheduling the compositor's main thread ahead of the rest of the machine.
//!
//! Input, the cursor and every frame go through the one thread that runs the event loop. On
//! equal terms with a build that fills every CPU, that thread misses vblanks: the pointer
//! jerks and video stutters until the build is done. It can also start out far *worse* than
//! equal, because priority is inherited -- CachyOS's ananicy-cpp gives greetd nice 16 and the
//! idle I/O class, and the compositor used to run with both.
//!
//! So, as KWin does, the main thread asks for `SCHED_RR` at the lowest real-time priority:
//! ahead of every ordinary task, behind the audio threads that ask for more. Where
//! `RLIMIT_RTPRIO` does not allow that, it settles for the best nice value `RLIMIT_NICE`
//! allows, down to [`FALLBACK_NICE`]. Either way an idle I/O class is dropped.
//!
//! `SCHED_RESET_ON_FORK` keeps all of this to this one thread. Threads and processes made
//! afterwards -- Mesa's workers, XWayland, anything a keybind starts -- go back to the normal
//! policy, and a negative nice is not handed down. That is also why [`raise`] has to run
//! before the backend brings up the GPU and its threads.

use tracing::{info, warn};

/// `SCHED_RR`'s lowest priority. Anything real-time outranks every normal task, so the
/// compositor needs no more than this, and the audio server, which asks for more, keeps
/// precedence over it.
const REALTIME_PRIORITY: libc::c_int = 1;

/// The nice value to aim for without real-time scheduling: what ananicy-cpp gives sway, niri
/// and KWin.
const FALLBACK_NICE: libc::c_int = -12;

/// `sched_setscheduler`'s flag for "children start on the normal policy".
const SCHED_RESET_ON_FORK: libc::c_int = 0x4000_0000;

/// `ioprio_set`/`ioprio_get`'s "which": a single thread or process.
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
/// The I/O class sits in the top bits of an I/O priority, above the level.
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
/// The class that is served only when the disk is otherwise idle.
const IOPRIO_CLASS_IDLE: libc::c_int = 3;
/// No class at all: the kernel derives one from the CPU scheduling -- real-time under
/// `SCHED_RR`, best-effort at a level following the nice value otherwise.
const IOPRIO_NONE: libc::c_int = 0;

/// Give the calling thread the best scheduling it is allowed. Call from the main thread,
/// before any other is spawned.
pub fn raise() {
    raise_nice();
    reset_io_class();

    let param = libc::sched_param {
        sched_priority: REALTIME_PRIORITY,
    };
    // SAFETY: `param` outlives the call, which only reads it.
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_RR | SCHED_RESET_ON_FORK, &param) } == 0 {
        info!(
            priority = REALTIME_PRIORITY,
            "main thread scheduled SCHED_RR"
        );
        return;
    }
    let why = std::io::Error::last_os_error();

    // Still mark the normal policy reset-on-fork, so the negative nice `raise_nice` may have
    // set stays with this thread. Setting the flag needs no privilege.
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: as above.
    unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER | SCHED_RESET_ON_FORK, &param) };
    info!(%why, nice = current_nice(), "no SCHED_RR; main thread stays on the normal policy");
}

/// The calling thread's nice value, or `None` if it could not be read.
fn current_nice() -> Option<libc::c_int> {
    // `getpriority` returns the nice value itself, so -1 is a valid answer and only `errno`
    // tells it from a failure.
    // SAFETY: `__errno_location` points at this thread's `errno`; `getpriority` has no
    // pointer arguments.
    let nice = unsafe {
        *libc::__errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS, 0)
    };
    (std::io::Error::last_os_error().raw_os_error() == Some(0)).then_some(nice)
}

/// Lower the nice value as far as `RLIMIT_NICE` allows, stopping at [`FALLBACK_NICE`].
///
/// Done even when `SCHED_RR` is about to be granted: reset-on-fork returns a *negative* nice
/// to 0 in children but hands a positive one down unchanged, so a compositor started at nice
/// 16 would start XWayland at 16 as well.
fn raise_nice() {
    let Some(nice) = current_nice() else { return };
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid `rlimit` for the call to fill in.
    if unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut limit) } != 0 {
        return;
    }
    // `RLIMIT_NICE` is 20 minus the lowest nice value allowed.
    let lowest = 20 - limit.rlim_cur.min(40) as libc::c_int;
    let target = lowest.max(FALLBACK_NICE);
    if nice <= target {
        if nice > 0 {
            warn!(
                nice,
                "started at a low priority and RLIMIT_NICE allows no better"
            );
        }
        return;
    }
    // SAFETY: no pointer arguments.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, target) } == 0 {
        info!(
            from = nice,
            to = target,
            "raised the main thread's nice value"
        );
    } else {
        warn!(nice, err = %std::io::Error::last_os_error(), "could not raise the nice value");
    }
}

/// Leave the idle I/O class, if the compositor was started in it. An idle-class compositor
/// waits on any disk read -- its own code paging back in included -- for as long as anything
/// else is writing.
fn reset_io_class() {
    // SAFETY: `ioprio_get` takes two integers and returns one.
    let ioprio = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) };
    if ioprio < 0 || (ioprio as libc::c_int) >> IOPRIO_CLASS_SHIFT != IOPRIO_CLASS_IDLE {
        return;
    }
    // Leaving the idle class for the default one needs no privilege.
    // SAFETY: `ioprio_set` takes three integers.
    if unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_NONE) } == 0 {
        info!("started in the idle I/O class; reset to the default");
    } else {
        warn!(err = %std::io::Error::last_os_error(), "could not leave the idle I/O class");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calling thread's policy, without the reset-on-fork flag.
    fn policy() -> libc::c_int {
        // SAFETY: no pointer arguments.
        unsafe { libc::sched_getscheduler(0) & !SCHED_RESET_ON_FORK }
    }

    /// Whatever the limits grant, nothing made after `raise` inherits it: not real-time
    /// scheduling and not a negative nice. On its own thread, so the test harness's thread is
    /// left as it was.
    #[test]
    fn raise_stays_with_the_calling_thread() {
        std::thread::spawn(|| {
            raise();
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is a valid `rlimit` for the call to fill in.
            unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) };
            if limit.rlim_cur >= REALTIME_PRIORITY as libc::rlim_t {
                assert_eq!(policy(), libc::SCHED_RR);
            }
            let (child_policy, child_nice) = std::thread::spawn(|| (policy(), current_nice()))
                .join()
                .unwrap();
            assert_eq!(child_policy, libc::SCHED_OTHER);
            assert!(child_nice.unwrap() >= 0);
        })
        .join()
        .unwrap();
    }
}
