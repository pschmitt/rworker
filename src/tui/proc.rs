//! The TUI's background work: this program run again as a child process
//! (one-shot actions, followed streams, the worker list refresh) and the
//! keyboard, all reporting to the UI thread over one channel.

use ratatui::crossterm::event::{self, Event};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub enum Msg {
    Input(Event),
    Refreshing,
    Workers {
        result: Result<Vec<Value>, String>,
        took: Duration,
    },
    Line {
        id: u64,
        text: String,
        err: bool,
    },
    Exit {
        id: u64,
    },
    Done {
        id: u64,
        ok: bool,
        out: String,
        err: String,
    },
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| "rworker".into())
}

/// `rworker ARGS...` in its own process group (so a stream is stopped
/// with everything it started), without a terminal on stdin.
pub fn command<S: AsRef<str>>(args: &[S], color: bool) -> Command {
    let mut cmd = Command::new(exe());
    cmd.args(args.iter().map(AsRef::as_ref))
        .env("__RWORKER_COLOR", if color { "1" } else { "0" })
        .env_remove("__RWORKER_QUIET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: prctl is async-signal-safe. The child gets SIGTERM when the
    // thread that started it ends: the UI thread for streams, the waiting
    // thread for actions; so nothing outlives a killed TUI.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd
}

/// `rworker ARGS...` attached to the terminal (ssh), its exit status.
pub fn interactive(args: &[String]) -> std::io::Result<Option<i32>> {
    let status = Command::new(exe())
        .args(args)
        .env_remove("__RWORKER_COLOR")
        .status()?;
    Ok(status.code())
}

/// Send each line of R, as text: lossy UTF-8, and of a line rewritten
/// with carriage returns (progress output) only its last state.
fn pump(r: impl Read, id: u64, err: bool, tx: &Sender<Msg>) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let raw = String::from_utf8_lossy(&buf);
        let raw = raw.trim_end_matches(['\n', '\r']);
        let text = raw
            .rsplit('\r')
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        if tx.send(Msg::Line { id, text, err }).is_err() {
            return;
        }
    }
}

/// A running child whose output lines go to the UI. Dropping it stops
/// the child and everything it started.
pub struct Stream {
    child: Option<Child>,
}

impl Stream {
    pub fn spawn(id: u64, args: &[String], tx: &Sender<Msg>) -> std::io::Result<Stream> {
        let mut child = command(args, true).spawn()?;
        let out = child.stdout.take();
        let err = child.stderr.take();
        let etx = tx.clone();
        let errs = thread::spawn(move || {
            if let Some(e) = err {
                pump(e, id, true, &etx);
            }
        });
        let otx = tx.clone();
        thread::spawn(move || {
            if let Some(o) = out {
                pump(o, id, false, &otx);
            }
            let _ = errs.join();
            let _ = otx.send(Msg::Exit { id });
        });
        Ok(Stream { child: Some(child) })
    }

    /// The exit status, once its output has ended.
    pub fn code(&mut self) -> Option<i32> {
        let child = self.child.as_mut()?;
        child.wait().ok().map(|s| s.code().unwrap_or(-1))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Ok(None) = child.try_wait() {
            // SAFETY: signalling the process group of our own child.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
        }
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Run `rworker ARGS...` to its end in the background and report it.
pub fn action(id: u64, args: Vec<String>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let msg = match command(&args, false).output() {
            Ok(o) => Msg::Done {
                id,
                ok: o.status.success(),
                out: String::from_utf8_lossy(&o.stdout).into_owned(),
                err: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            Err(e) => Msg::Done {
                id,
                ok: false,
                out: String::new(),
                err: e.to_string(),
            },
        };
        let _ = tx.send(msg);
    });
}

/// The last non-empty line of a command's messages, without colors.
pub fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(super::ansi::strip)
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
}

/// `rworker ls --json` every INTERVAL, or right away when triggered.
pub fn refresher(tx: Sender<Msg>, interval: Duration, trigger: Receiver<()>) {
    thread::spawn(move || {
        loop {
            if tx.send(Msg::Refreshing).is_err() {
                return;
            }
            let t0 = Instant::now();
            let result = match command(&["ls", "--json"], false).output() {
                Ok(o) if o.status.success() => serde_json::from_slice::<Value>(&o.stdout)
                    .map_err(|e| format!("invalid JSON: {e}"))
                    .and_then(|v| match v {
                        Value::Array(a) => Ok(a),
                        _ => Err("unexpected answer from the control host".into()),
                    }),
                Ok(o) => {
                    let msg = last_line(&String::from_utf8_lossy(&o.stderr));
                    Err(if msg.is_empty() {
                        format!("ls failed ({})", o.status)
                    } else {
                        msg
                    })
                }
                Err(e) => Err(e.to_string()),
            };
            let took = t0.elapsed();
            if tx.send(Msg::Workers { result, took }).is_err() {
                return;
            }
            match trigger.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => while trigger.try_recv().is_ok() {},
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Keyboard and resize events. While PAUSED (an ssh session owns the
/// terminal) it reads nothing and says so in PARKED.
pub fn input(tx: Sender<Msg>, paused: Arc<AtomicBool>, parked: Arc<AtomicBool>) {
    thread::spawn(move || {
        loop {
            if paused.load(Ordering::SeqCst) {
                parked.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(30));
                continue;
            }
            parked.store(false, Ordering::SeqCst);
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    // Paused while polling: leave the key to the child.
                    if paused.load(Ordering::SeqCst) {
                        continue;
                    }
                    if let Ok(ev) = event::read()
                        && tx.send(Msg::Input(ev)).is_err()
                    {
                        return;
                    }
                }
                Ok(false) => {}
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    });
}
