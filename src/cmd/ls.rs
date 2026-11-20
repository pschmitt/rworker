//! `ls`, `ls --watch` and the worker filters shared with logs and events.

use super::watch;
use crate::ctx::{Ctx, Fail, R};
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
    let mut fresh = false;
    let mut history = false;
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
            "--wide" | "-owide" => ctx.wide.set(true),
            "-w" | "--watch" => watch = true,
            "--fresh" => fresh = true,
            "-A" | "--all" | "--history" => history = true,
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
        if fresh {
            ctx.err("--fresh cannot be used with --watch");
            return Err(Fail(2));
        }
        return watch_ls(ctx, &interval, &topic, &agent, history);
    }
    let out = if fresh {
        state::remote(ctx, &["ls", "--fresh"])?
    } else {
        state::remote_ls(ctx)?
    };
    let out = if history {
        super::history::with_retired(ctx, &out)?
    } else {
        out
    };
    let out = filter_workers(ctx, &out, &topic, &agent)?;
    if json || ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let list = state::parse(ctx, &out)?;
    table::render(ctx, &list)
}

/// `ls --watch`: redraw in place, like `kw` (kubectl-watch), and mark what
/// changed since the previous refresh: + new, ~ changed, - gone (shown
/// once more).
fn watch_ls(ctx: &Ctx, interval: &str, topic: &str, agent: &str, history: bool) -> R {
    let secs: u64 = interval.parse::<u64>().unwrap_or(5).min(u32::MAX.into());
    let (green, yellow, red, dim, reset) = if ctx.color {
        ("\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };
    crate::signals::trap();
    let mut screen = watch::Screen::enter();
    let mut prev_sig: HashMap<String, String> = HashMap::new();
    let mut prev_row: BTreeMap<String, String> = BTreeMap::new();
    let mut first = true;
    let mut json = String::new();
    let mut error = String::new();
    use std::time::{Duration, Instant};
    let endpoint = crate::inventory_feed::endpoint();
    let mut feed = crate::inventory_feed::Feed::open(ctx, endpoint.as_deref());
    let interval_duration = Duration::from_secs(secs);
    let initial = Instant::now() + Duration::from_millis(400);
    let mut next_poll = initial;
    let mut next_draw = initial;
    let mut next_connect = initial;
    let mut last_message = Instant::now();
    let mut live = false;
    let mut manual = false;
    loop {
        if crate::signals::pending().is_some() {
            return screen.leave();
        }
        let now = Instant::now();
        let mut changed = false;
        if let Some(stream) = &feed {
            match stream.receive(Duration::ZERO) {
                Ok(line) => {
                    last_message = now;
                    if line != b"\n"
                        && let Ok(raw) = std::str::from_utf8(&line)
                        && let Ok(out) = format_inventory(ctx, raw.trim(), topic, agent, history)
                    {
                        live = true;
                        changed = json != out || !error.is_empty();
                        json = out;
                        error.clear();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if last_message.elapsed() < Duration::from_secs(8) => {}
                Err(_) => {
                    feed = None;
                    live = false;
                    next_poll = now;
                    next_connect = now + interval_duration;
                }
            }
        } else if now >= next_connect {
            feed = crate::inventory_feed::Feed::open(ctx, endpoint.as_deref());
            last_message = now;
            next_connect = now + interval_duration;
        }
        if manual || (!live && now >= next_poll) {
            match refresh(ctx, topic, agent, history) {
                Ok(out) => {
                    json = out;
                    error.clear();
                }
                Err(message) => error = message,
            }
            changed = true;
            next_poll = Instant::now() + interval_duration;
        }
        if changed || now >= next_draw {
            next_draw = Instant::now() + interval_duration;
            let list: Value = serde_json::from_str(if json.is_empty() { "[]" } else { &json })
                .unwrap_or(Value::Null);
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
            let command = if std::io::stdout().is_terminal() {
                "machines"
            } else {
                "ls"
            };
            let mut frame = format!(
                "{}: rworker {command}    {}    {} workers    (q quits, any key refreshes)\n",
                if live {
                    "Live".into()
                } else {
                    format!("Every {interval}s")
                },
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
            screen.draw(&frame);
            prev_sig = cur_sig;
            prev_row = cur_row;
            first = false;
        }
        let Some(refresh) = watch::wait_refresh(0.15) else {
            return screen.leave();
        };
        manual = refresh;
    }
}

/// One refresh for --watch: the filtered list, or the last line of what
/// went wrong.
fn refresh(ctx: &Ctx, topic: &str, agent: &str, history: bool) -> Result<String, String> {
    let mut cmd = state::control_cmd(ctx, &["ls"]);
    let output = cmd
        .stdin(if std::io::stdin().is_terminal() {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::inherit()
        })
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
    format_inventory(ctx, &stdout, topic, agent, history)
}

fn format_inventory(
    ctx: &Ctx,
    raw: &str,
    topic: &str,
    agent: &str,
    history: bool,
) -> Result<String, String> {
    state::cache_inventory(ctx, &ctx.ls_cache(), raw);
    ctx.muted(|| {
        let inventory = if history {
            super::history::with_retired(ctx, raw)?
        } else {
            raw.to_owned()
        };
        filter_workers(ctx, &inventory, topic, agent)
    })
    .map_err(|_| "jq: error: invalid JSON from the control host".to_string())
}
