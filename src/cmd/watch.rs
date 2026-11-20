//! Shared terminal drawing and input handling for watched lists.
use crate::ctx::{Fail, R, out};
use std::io::IsTerminal;
use unicode_width::UnicodeWidthChar;

/// The terminal state while `--watch` waits for a key: no echo, no line
/// buffering, signals still on (like `read -s -n 1`).
struct KeyWait {
    saved: Option<libc::termios>,
}

impl KeyWait {
    fn new() -> KeyWait {
        // SAFETY: tcgetattr/tcsetattr on stdin with a valid termios buffer.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return KeyWait { saved: None };
            }
            let saved = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &t);
            KeyWait { saved: Some(saved) }
        }
    }

    /// A key pressed within `secs`, if any.
    fn key(&self, secs: f64) -> Option<u8> {
        let mut fds = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (secs * 1000.0).min(i32::MAX as f64) as i32;
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut fds, 1, ms) };
        if n <= 0 {
            return None;
        }
        let mut b = [0u8; 1];
        // SAFETY: reading one byte into a valid buffer.
        let r = unsafe { libc::read(0, b.as_mut_ptr().cast(), 1) };
        (r == 1).then_some(b[0])
    }
}

impl Drop for KeyWait {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: restoring the attributes read in new().
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// `tput CAP 2>/dev/null` (terminfo knows the sequences), its status.
fn tput(cap: &str) -> i32 {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::Command::new("tput")
        .arg(cap)
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(127, crate::proc::code_of)
}

/// The alternate screen without a cursor while watching. Leaving it is
/// the bash client's EXIT trap under `set -e`: a failing tput (no terminal
/// type, no tput) became the exit status.
pub(super) struct Screen {
    terminal: bool,
    rows: Vec<String>,
    dimensions: (u16, u16),
}

impl Screen {
    pub(super) fn enter() -> Screen {
        tput("smcup");
        tput("civis");
        Screen {
            terminal: std::io::stdout().is_terminal(),
            rows: Vec::new(),
            dimensions: (0, 0),
        }
    }

    pub(super) fn draw(&mut self, frame: &str) {
        let dimensions = self
            .terminal
            .then(ratatui::crossterm::terminal::size)
            .and_then(Result::ok)
            .filter(|&(width, height)| width > 0 && height > 0);
        let Some((width, height)) = dimensions else {
            out(&format!("\x1b[H{}\x1b[J", frame.replace('\n', "\x1b[K\n")));
            return;
        };
        let rows: Vec<_> = frame
            .lines()
            .take(height as usize)
            .map(|line| clip(line, width as usize))
            .collect();
        let resized = self.dimensions != (width, height);
        let mut patch = String::new();
        for index in 0..rows.len().max(self.rows.len()).min(height as usize) {
            let new = rows.get(index).map_or("", String::as_str);
            if resized || new != self.rows.get(index).map_or("", String::as_str) {
                use std::fmt::Write;
                let _ = write!(patch, "\x1b[{};1H{new}\x1b[K", index + 1);
            }
        }
        if !patch.is_empty() {
            out(&format!("\x1b[?2026h{patch}\x1b[?2026l"));
        }
        self.rows = rows;
        self.dimensions = (width, height);
    }

    pub(super) fn leave(self) -> R {
        let code = tput("cnorm");
        if code != 0 {
            return Err(Fail(code));
        }
        crate::ctx::status(tput("rmcup"))
    }
}

/// Fit a row to the terminal without counting SGR style sequences as cells.
fn clip(line: &str, width: usize) -> String {
    let mut row = String::new();
    let mut used = 0;
    let mut rest = line;
    while let Some(ch) = rest.chars().next() {
        if let Some(sgr) = rest.strip_prefix("\x1b[")
            && let Some(end) = sgr.find('m')
            && sgr[..end].bytes().all(|b| b.is_ascii_digit() || b == b';')
        {
            row.push_str(&rest[..end + 3]);
            rest = &rest[end + 3..];
            continue;
        }
        rest = &rest[ch.len_utf8()..];
        let Some(cells) = ch.width() else {
            continue;
        };
        if used + cells > width {
            if row.contains('\x1b') {
                row.push_str("\x1b[0m");
            }
            break;
        }
        used += cells;
        row.push(ch);
    }
    row
}

pub(super) fn wait(secs: f64) -> bool {
    let key = if std::io::stdin().is_terminal() {
        KeyWait::new().key(secs)
    } else {
        crate::signals::sleep(secs);
        None
    };
    !matches!(key, Some(b'q' | b'Q')) && crate::signals::pending().is_none()
}

pub(super) fn interval(value: &str) -> Option<u64> {
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_ansi_rows_at_terminal_cell_width() {
        assert_eq!(clip("\x1b[36mworker\x1b[0m", 3), "\x1b[36mwor\x1b[0m");
        assert_eq!(clip("a表", 2), "a");
        assert_eq!(clip("a表", 3), "a表");
    }
}
