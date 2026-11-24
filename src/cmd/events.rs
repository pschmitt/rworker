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
    if matches!(kind, "worker-added" | "worker-deleted") {
        let worker = &event.worker;
        if !worker.profile.is_empty() {
            let forge = crate::github_profiles::Forge::from_key(&worker.provider)
                .or_else(|| crate::github_profiles::from_name(&worker.profile).map(|p| p.forge));
            let glyph = crate::pool_icon::provider_glyph(forge);
            let color = match glyph {
                crate::pool_icon::GITHUB => "38;2;0;0;0",
                crate::pool_icon::GITLAB => "38;2;226;67;41",
                _ => "38;2;218;26;50",
            };
            let icon = if ctx.icons.get() {
                format!("{} ", paint(ctx, color, glyph))
            } else {
                String::new()
            };
            text += &format!(" profile={icon}{}", paint::vm(ctx, &worker.profile));
        }
        if !worker.agent.is_empty() {
            let glyph = crate::table::glyph(&worker.agent);
            let icon = if ctx.icons.get() && !glyph.is_empty() {
                let glyph = if ctx.color {
                    paint::agents(glyph)
                } else {
                    glyph.to_owned()
                };
                format!("{glyph} ")
            } else {
                String::new()
            };
            text += &format!(" agent={icon}{}", worker.agent);
        }
    }
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
            crate::event_history::observe_workers(&directory, &inventory).map_err(|error| {
                ctx.err(&format!("Record worker lifecycle events: {error}"));
                Fail(1)
            })?;
            let (jobs, topics) = super::jobs::observed_jobs(ctx, &inventory);
            crate::event_history::observe_jobs(&directory, &jobs, topics).map_err(|error| {
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

/// A flag set whenever a job starts, changes state or goes away, from a
/// background thread that follows the controller's lifecycle events (and
/// reconnects). Watched job lists refresh on it instead of only polling.
pub fn job_signal() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    let set = std::sync::Arc::clone(&flag);
    std::thread::spawn(move || {
        let ctx = Ctx::from_env();
        let mut cursor = None;
        loop {
            if cursor.is_none() {
                cursor = crate::proc::capture_ok(
                    &ctx,
                    state::control_cmd(&ctx, &["lifecycle-events", "--latest-id"])
                        .stdin(Stdio::null())
                        .stderr(Stdio::null()),
                )
                .ok()
                .and_then(|id| id.trim().parse::<u64>().ok());
            }
            if let Some(start) = cursor
                && let Ok(mut child) =
                    state::control_cmd(&ctx, &["lifecycle-events", "--watch", &start.to_string()])
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
            {
                if let Some(output) = child.stdout.take() {
                    for line in BufReader::new(output).lines().map_while(Result::ok) {
                        let Ok(event) = serde_json::from_str::<Event>(&line) else {
                            continue;
                        };
                        if event.id > cursor.unwrap_or(0) {
                            cursor = Some(event.id);
                            if event.kind.starts_with("job-") {
                                set.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
    flag
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
