//! `ls`, `ls --watch` and the worker filters shared with logs and events.

use crate::ctx::{Ctx, Fail, R, out};
use crate::jq::{self, get};
use crate::state;
use crate::table;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;

/// Check a filter regex (a leading ! inverts it, and is not part of it).
pub fn validate_regex(ctx: &Ctx, flag: &str, pattern: &str) -> R {
    let p = pattern.strip_prefix('!').unwrap_or(pattern);
    if Regex::new(p).is_err() {
        ctx.err(&format!("invalid {flag} regex: {p}"));
        return Err(Fail(2));
    }
    Ok(())
}

fn matches(v: &str, re: &str) -> bool {
    if re.is_empty() {
        return true;
    }
    match re.strip_prefix('!') {
        Some(r) => !Regex::new(r).map(|r| r.is_match(v)).unwrap_or(false),
        None => Regex::new(re).map(|r| r.is_match(v)).unwrap_or(false),
    }
}

/// Workers whose topic, agent and agent id match the filters; the answer
/// unchanged when there are none.
pub fn filter_workers(ctx: &Ctx, workers: &str, topic: &str, agent: &str) -> R<String> {
    let agent_id = ctx.filter_agent_id.borrow().clone();
    if topic.is_empty() && agent.is_empty() && agent_id.is_empty() {
        return Ok(workers.to_string());
    }
    let list = state::parse(ctx, workers)?;
    let items = state::items(ctx, &list)?;
    let s = |w: &Value, k: &str| jq::or_empty(get(w, k));
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|w| {
            matches(&s(w, "topic"), topic)
                && matches(&s(w, "agent"), agent)
                && matches(&s(w, "agent_id"), &agent_id)
        })
        .cloned()
        .collect();
    Ok(jq::compact(&Value::Array(kept)))
}

pub fn cmd_ls(ctx: &Ctx, args: &[String]) -> R {
    let mut json = false;
    let mut watch = false;
    let mut interval = "5".to_string();
    let (mut topic, mut agent) = (String::new(), String::new());
    let mut i = 0;
    while i < args.len() && !args[i].is_empty() {
        let a = args[i].as_str();
        match a {
            "-o" => {
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some("json") => json = true,
                    Some("wide") => ctx.wide.set(true),
                    _ => {
                        ctx.err("Output format must be json or wide");
                        crate::help::show(ctx, "ls");
                        return Err(Fail(2));
                    }
                }
            }
            "--json" | "-ojson" => json = true,
            "-w" | "--watch" => watch = true,
            "--topic" | "--agent" | "--agent-id" => {
                let v = args.get(i + 1).cloned().unwrap_or_default();
                if v.is_empty() {
                    ctx.err(&format!("{a} requires a regex pattern"));
                    return Err(Fail(2));
                }
                validate_regex(ctx, a, &v)?;
                match a {
                    "--topic" => topic = v,
                    "--agent" => agent = v,
                    _ => *ctx.filter_agent_id.borrow_mut() = v,
                }
                i += 1;
            }
            "-n" | "--interval" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                let ok = !v.is_empty()
                    && v.bytes().all(|b| b.is_ascii_digit())
                    && v.parse::<u64>().map(|n| n >= 1).unwrap_or(true);
                if !ok {
                    ctx.err("Interval must be a whole number of seconds");
                    crate::help::show(ctx, "ls");
                    return Err(Fail(2));
                }
                interval = v;
            }
            _ => {
                ctx.err(&format!("Unknown option: {a}"));
                crate::help::show(ctx, "ls");
                return Err(Fail(2));
            }
        }
        i += 1;
    }
    if watch {
        return watch_ls(ctx, &interval, &topic, &agent);
    }
    let out = state::remote_ls(ctx)?;
    let out = filter_workers(ctx, &out, &topic, &agent)?;
    if json || ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let list = state::parse(ctx, &out)?;
    table::render(ctx, &list)
}

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
struct Screen;

impl Screen {
    fn enter() -> Screen {
        tput("smcup");
        tput("civis");
        Screen
    }

    fn leave(self) -> R {
        let code = tput("cnorm");
        if code != 0 {
            return Err(Fail(code));
        }
        crate::ctx::status(tput("rmcup"))
    }
}

/// `ls --watch`: redraw in place, like `kw` (kubectl-watch), and mark what
/// changed since the previous refresh: + new, ~ changed, - gone (shown
/// once more).
fn watch_ls(ctx: &Ctx, interval: &str, topic: &str, agent: &str) -> R {
    let secs: f64 = interval.parse().unwrap_or(5.0);
    let (green, yellow, red, dim, reset) = if ctx.color {
        ("\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };
    crate::signals::trap();
    let screen = Screen::enter();
    let mut prev_sig: HashMap<String, String> = HashMap::new();
    let mut prev_row: BTreeMap<String, String> = BTreeMap::new();
    let mut first = true;
    let mut json = String::new();
    let mut error = String::new();
    loop {
        if crate::signals::pending().is_some() {
            return screen.leave();
        }
        match refresh(ctx, topic, agent) {
            Ok(out) => {
                json = out;
                error.clear();
            }
            Err(msg) => error = msg,
        }
        let list: Value =
            serde_json::from_str(if json.is_empty() { "[]" } else { &json }).unwrap_or(Value::Null);
        let cur_sig: HashMap<String, String> = table::signatures(&list).into_iter().collect();
        let plain = ctx.muted(|| table::plain(ctx, &list)).unwrap_or_default();
        let plain_lines: Vec<&str> = if plain.is_empty() {
            Vec::new()
        } else {
            plain.split('\n').collect()
        };
        let colored;
        let shown: Vec<&str> = if ctx.color && !plain.is_empty() {
            colored = table::colorize(&plain);
            colored.split('\n').collect()
        } else {
            plain_lines.clone()
        };
        let mut frame = format!(
            "Every {interval}s: rworker ls    {}    {} workers    (q quits, any key refreshes)\n",
            crate::time::now_hms(),
            cur_sig.len()
        );
        if !error.is_empty() {
            frame += &format!("{red}update failed, showing the last state: {error}{reset}\n");
        }
        frame.push('\n');
        let mut cur_row: BTreeMap<String, String> = BTreeMap::new();
        for (i, line) in plain_lines.iter().enumerate() {
            let name = line.split(' ').next().unwrap_or("").to_string();
            let mut marker = "  ".to_string();
            if i > 0 {
                cur_row.insert(name.clone(), line.to_string());
                if !first && !prev_sig.contains_key(&name) {
                    marker = format!("{green}+{reset} ");
                } else if !first && prev_sig.get(&name) != cur_sig.get(&name) {
                    marker = format!("{yellow}~{reset} ");
                }
            }
            frame += &format!("{marker}{}\n", shown.get(i).copied().unwrap_or(""));
        }
        for (name, row) in &prev_row {
            if !cur_sig.contains_key(name) {
                frame += &format!("{red}-{reset} {dim}{row}{reset}\n");
            }
        }
        if plain_lines.is_empty() {
            frame += "No workers found.\n";
        }
        // Home, draw, clear the rest: no flicker. \e[K ends every line, or
        // a line shorter than the one drawn there before keeps its tail.
        out(&format!("\x1b[H{}\x1b[J", frame.replace('\n', "\x1b[K\n")));
        prev_sig = cur_sig;
        prev_row = cur_row;
        first = false;

        let key = if std::io::stdin().is_terminal() {
            let wait = KeyWait::new();
            let k = wait.key(secs);
            drop(wait);
            k
        } else {
            // No keyboard (e.g. piped or in a script): just wait.
            crate::signals::sleep(secs);
            None
        };
        if matches!(key, Some(b'q' | b'Q')) || crate::signals::pending().is_some() {
            return screen.leave();
        }
    }
}

/// One refresh for --watch: the filtered list, or the last line of what
/// went wrong.
fn refresh(ctx: &Ctx, topic: &str, agent: &str) -> Result<String, String> {
    let mut cmd = state::control_cmd(ctx, &["ls"]);
    let output = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    let stdout = crate::proc::trim_nl(&String::from_utf8_lossy(&output.stdout));
    let stderr = crate::proc::trim_nl(&String::from_utf8_lossy(&output.stderr));
    let last = |s: &str| s.rsplit('\n').next().unwrap_or("").to_string();
    if !output.status.success() {
        let combined = if stderr.is_empty() { stdout } else { stderr };
        return Err(last(&combined));
    }
    // remote_ls keeps the cache for the completions.
    if std::fs::create_dir_all(&ctx.state_dir).is_ok() {
        let tmp = ctx.state_dir.join(".ls.json.tmp");
        if std::fs::write(&tmp, format!("{stdout}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, ctx.ls_cache());
        }
    }
    ctx.muted(|| filter_workers(ctx, &stdout, topic, agent))
        .map_err(|_| "jq: error: invalid JSON from the control host".to_string())
}
