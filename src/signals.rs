//! The shell's `trap ... INT TERM`: a command that must clean up asks for
//! the signals here and checks between steps; the running child (ssh,
//! rsync) gets the terminal's SIGINT itself, as it did under bash.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

struct Flags {
    int: Arc<AtomicBool>,
    term: Arc<AtomicBool>,
}

static FLAGS: OnceLock<Flags> = OnceLock::new();

/// Catch SIGINT and SIGTERM from now on (instead of dying).
pub fn trap() {
    FLAGS.get_or_init(|| {
        let int = Arc::new(AtomicBool::new(false));
        let term = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&int));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term));
        Flags { int, term }
    });
}

/// Which trapped signal arrived, if any.
pub fn pending() -> Option<i32> {
    let f = FLAGS.get()?;
    if f.int.load(Ordering::SeqCst) {
        Some(libc::SIGINT)
    } else if f.term.load(Ordering::SeqCst) {
        Some(libc::SIGTERM)
    } else {
        None
    }
}

/// Sleep, waking early for a trapped signal.
pub fn sleep(secs: f64) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.max(0.0));
    while std::time::Instant::now() < end {
        if pending().is_some() {
            return;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
    }
}

/// SIGPIPE ends the program on a closed stdout, as it ends a shell script.
pub fn default_sigpipe() {
    // SAFETY: restoring a default disposition has no preconditions.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
