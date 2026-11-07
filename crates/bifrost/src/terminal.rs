//! Our terminal: switching stdin to raw mode (so that, e.g., Ctrl-C reaches the guest)
//! and back.

use std::sync::Mutex;

/// The libc crate does not define it for managarm (mlibc uses the Linux ABI value).
const TCSANOW: libc::c_int = 0;

/// The terminal settings to restore on exit.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Switches stdin to raw mode if it is a terminal; returns whether it did.
/// `output_processing` keeps output post-processing (e.g., newline translation) for
/// output that is not from a terminal.
pub fn enter_raw_mode(output_processing: bool) -> bool {
    // SAFETY: isatty() and tc[gs]etattr() only access the termios struct.
    unsafe {
        if libc::isatty(0) == 0 {
            return false;
        }
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut termios) != 0 {
            return false;
        }
        let saved = termios;
        libc::cfmakeraw(&mut termios);
        if output_processing {
            termios.c_oflag |= libc::OPOST;
        }
        if libc::tcsetattr(0, TCSANOW, &termios) != 0 {
            return false;
        }
        *SAVED.lock().unwrap() = Some(saved);
        true
    }
}

/// Restores the terminal settings (if we changed them).
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap().take() {
        // SAFETY: See enter_raw_mode().
        unsafe { libc::tcsetattr(0, TCSANOW, &saved) };
    }
}
