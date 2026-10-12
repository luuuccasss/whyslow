//! whyslow keeps running while the command is interrupted so it can still
//! print a report. Ctrl+C from the terminal already reaches the command (same
//! process group); signals sent to whyslow alone (kill, timeout) are forwarded.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// si_code of a signal sent with kill().
#[cfg(any(target_os = "linux", target_os = "android"))]
const SI_USER: libc::c_int = 0;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SI_USER: libc::c_int = 0x10001;

static PENDING: AtomicI32 = AtomicI32::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    // A SIGINT not sent with kill() comes from the terminal and already hit the command.
    let from_tty = sig == libc::SIGINT && !info.is_null() && unsafe { (*info).si_code } != SI_USER;
    if !from_tty {
        PENDING.store(sig, Ordering::SeqCst);
    }
}

pub fn install() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            // No SA_RESTART: a blocking waitpid() returns EINTR so we can forward.
            sa.sa_flags = libc::SA_SIGINFO;
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
