//! Read and follow the controller's lifecycle history.

use crate::ctx::{Ctx, Fail, R, outln};
use crate::event_history::Event;
use crate::paint::{self, paint};
use crate::{signals, state};
use std::io::{BufRead, BufReader};
use std::process::Stdio;

fn emit_event(ctx: &Ctx, event: &Event) {
    let (kind, target, details) = (
        event.kind.as_str(),
        event.target.as_str(),
        event.details.as_str(),
    );
    let color = if kind.ends_with("-added") {
        "32"
    } else if kind.ends_with("-deleted") {
        "31"
    } else if kind == "job-state" {
        "33"
    } else {
        "36"
    };
    let mut text = format!(
        "{} {}",
        paint(ctx, color, kind),
        paint::event_target(ctx, target)
    );
    if !details.is_empty() {
        if let Some((a, b)) = details.split_once(" -> ") {
            text += &format!(" {} -> {}", paint::value(ctx, a), paint::value(ctx, b));
        } else if let Some((key, value)) = details.split_once('=')
            && key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'_' | b'-'))
        {
            text += &format!(" {key}={}", paint::value(ctx, value));
        } else {
            text += &format!(" {}", paint(ctx, "33", details));
        }
    }
    outln(&format!(
        "{} {}",
        jiff::Timestamp::from_second(event.timestamp)
            .map_or_else(|_| event.timestamp.to_string(), |time| time.to_string()),
        paint::italic(ctx, &text)
    ));
}

pub fn collect_jobs(ctx: &Ctx) -> R {
    let directory = crate::event_history::directory().ok_or(Fail(1))?;
    loop {
        let result = (|| {
            let raw = state::remote_ls(ctx)?;
            let inventory = state::parse(ctx, &raw)?;
            let jobs = super::jobs::observed_jobs(ctx, &inventory);
            crate::event_history::observe_jobs(&directory, &jobs).map_err(|error| {
                ctx.err(&format!("Record job lifecycle events: {error}"));
                Fail(1)
            })
        })();
        if result.is_err() {
            ctx.err("Could not refresh job lifecycle events");
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

pub fn run(ctx: &Ctx, args: &[String]) -> R {
    let mut interval = 2;
    let mut watch = false;
    let mut since_now = false;
    let mut filters = Vec::new();
    let mut args = args.iter();
    while let Some(option) = args.next() {
        match option.as_str() {
            "-w" | "--watch" => watch = true,
            "--since-now" => since_now = true,
            "--interval" => {
                let Some(seconds) = args.next().and_then(|value| super::watch::interval(value))
                else {
                    ctx.err("interval must be a positive whole number of seconds");
                    return Err(Fail(2));
                };
                interval = seconds;
            }
            "--topic" | "--agent" | "--agent-id" => {
                let Some(pattern) = args.next().filter(|pattern| !pattern.is_empty()) else {
                    ctx.err(&format!("{option} requires a regex pattern"));
                    return Err(Fail(2));
                };
                let invert = pattern.starts_with('!');
                let regex = regex::Regex::new(pattern.strip_prefix('!').unwrap_or(pattern))
                    .map_err(|error| {
                        ctx.err(&format!("invalid {option} regex: {error}"));
                        Fail(2)
                    })?;
                filters.push((option.as_str(), regex, invert));
            }
            _ => {
                ctx.err(&format!("Unknown events option: {option}"));
                crate::help::show(ctx, "events");
                return Err(Fail(2));
            }
        }
    }
    if since_now && !watch {
        ctx.err("--since-now requires --watch");
        return Err(Fail(2));
    }
    let matches = |event: &Event| {
        filters.iter().all(|(flag, regex, invert)| {
            let field = match *flag {
                "--topic" => &event.worker.topic,
                "--agent" => &event.worker.agent,
                _ => &event.worker.agent_id,
            };
            regex.is_match(field) != *invert
        })
    };
    let request = |args: &[&str]| {
        crate::proc::capture_ok(ctx, state::control_cmd(ctx, args).stdin(Stdio::null()))
    };
    let (history, mut cursor) = if since_now {
        let id = request(&["lifecycle-events", "--latest-id"])?;
        (Vec::new(), id.trim().parse().map_err(|_| Fail(1))?)
    } else {
        let raw = request(&["lifecycle-events"])?;
        let history: Vec<Event> = serde_json::from_str(&raw).map_err(|error| {
            ctx.err(&format!("Decode lifecycle events: {error}"));
            Fail(1)
        })?;
        let cursor = history.last().map_or(0, |event| event.id);
        (history, cursor)
    };
    if !watch && ctx.json.get() {
        let events: Vec<_> = history.iter().filter(|event| matches(event)).collect();
        let output = serde_json::to_string(&events).map_err(|_| Fail(1))?;
        crate::ctx::json_out(ctx, &output);
        return Ok(());
    }
    let show = |event: &Event| -> R {
        if matches(event) {
            if ctx.json.get() {
                outln(&serde_json::to_string(event).map_err(|_| Fail(1))?);
            } else {
                emit_event(ctx, event);
            }
        }
        Ok(())
    };
    for event in &history {
        show(event)?;
    }
    if !watch {
        return Ok(());
    }

    signals::trap();
    while signals::pending().is_none() {
        let mut child =
            state::control_cmd(ctx, &["lifecycle-events", "--watch", &cursor.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(ctx.child_stderr())
                .spawn()
                .map_err(|error| {
                    ctx.err(&format!("Open lifecycle event stream: {error}"));
                    Fail(1)
                })?;
        let input = child.stdout.take().ok_or(Fail(1))?;
        let result = (|| {
            for line in BufReader::new(input).lines() {
                if signals::pending().is_some() {
                    break;
                }
                let Ok(line) = line else { break };
                if line.is_empty() {
                    continue;
                }
                let event: Event = serde_json::from_str(&line).map_err(|error| {
                    ctx.err(&format!("Decode streamed event: {error}"));
                    Fail(1)
                })?;
                if event.id > cursor {
                    cursor = event.id;
                    show(&event)?;
                }
            }
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        result?;
        if signals::pending().is_none() {
            ctx.err("Lifecycle event stream ended; reconnecting");
            signals::sleep(interval as f64);
        }
    }
    Ok(())
}
