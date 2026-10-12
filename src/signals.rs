//! whyslow keeps running while the command is interrupted so it can still
//! print a report. SIGINT from the terminal already reaches the command (same
//! process group); SIGTERM/SIGHUP sent to whyslow alone are forwarded to it.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static PENDING: AtomicI32 = AtomicI32::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGINT {
        INTERRUPTED.store(true, Ordering::SeqCst);
    } else {
        INTERRUPTED.store(true, Ordering::SeqCst);
        PENDING.store(sig, Ordering::SeqCst);
    }
}

pub fn install() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            // No SA_RESTART: a blocking waitpid() returns EINTR so we can forward.
            sa.sa_flags = 0;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// A signal to forward to the command, if one arrived since the last call.
pub fn take_pending() -> Option<i32> {
    match PENDING.swap(0, Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
