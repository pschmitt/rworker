//! A private, bounded index of workers observed by the log collector.
use crate::ctx::{Ctx, Fail, R};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

fn directory(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("worker-history")
}

fn timestamp() -> String {
    jiff::Timestamp::now().to_string()
}

fn record(dir: &std::path::Path, name: &str, source: &Value, finished: bool) -> R {
    if !super::log_cache::component(name) {
        return Err(Fail(2));
    }
    let path = dir.join(name);
    let old: Value = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let field = |key: &str| {
        let value = &source[key];
        if value.is_string() || value.is_number() {
            value.clone()
        } else {
            old[key].clone()
        }
    };
    let record = json!({
        "name": name, "topic": field("topic"), "agent": field("agent"),
        "created_at": field("created_at"), "last_seen": timestamp(),
        "owner": field("owner"), "profile": field("profile"), "agent_id": field("agent_id"),
        "status": field("status"),
        "exit_code": if finished { source["exit_code"].as_i64().map(Value::from).unwrap_or(Value::Null) } else { old["exit_code"].clone() },
    });
    let bytes = serde_json::to_vec(&record).map_err(|_| Fail(1))?;
    super::log_cache::private_dir(dir).map_err(|_| Fail(1))?;
    super::log_cache::put(dir, &path, &bytes, SystemTime::now()).map_err(|_| Fail(1))
}

fn files(dir: &std::path::Path) -> R<Vec<(SystemTime, PathBuf)>> {
    let files = match fs::read_dir(dir) {
        Ok(files) => files,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(Fail(1)),
    };
    let mut files: Vec<_> = files
        .flatten()
        .filter_map(|entry| {
            if !entry
                .file_name()
                .to_str()
                .is_some_and(super::log_cache::component)
            {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, entry.path()))
        })
        .collect();
    files.sort_unstable_by(|a, b| b.cmp(a));
    Ok(files)
}

fn prune(dir: &std::path::Path) -> R {
    for (index, (modified, path)) in files(dir)?.into_iter().enumerate() {
        if index >= 1024 || modified.elapsed().unwrap_or_default() > Duration::from_secs(30 * 86400)
        {
            fs::remove_file(path).map_err(|_| Fail(1))?;
        }
    }
    Ok(())
}

pub(super) fn observe(ctx: &Ctx, inventory: &Value) -> R {
    let owner = format!("{}@", ctx.user);
    for worker in inventory.as_array().ok_or(Fail(1))? {
        if worker["kind"] == "adhoc"
            && worker["owner"]
                .as_str()
                .is_some_and(|value| value.starts_with(&owner))
            && let Some(name) = worker["name"]
                .as_str()
                .filter(|name| super::log_cache::component(name))
        {
            record(&directory(ctx), name, worker, false)?;
        }
    }
    prune(&directory(ctx))
}

pub(super) fn record_created(ctx: &Ctx, worker: &str, info: &Value) -> R {
    record(&directory(ctx), worker, info, false)?;
    prune(&directory(ctx))
}

pub(super) fn record_exec(ctx: &Ctx, worker: &str, info: &Value) -> R {
    record(&directory(ctx), worker, info, true)?;
    prune(&directory(ctx))
}

pub(crate) fn record_builder(dir: &std::path::Path, name: &str, info: &Value) -> R {
    record(dir, name, info, false)?;
    prune(dir)
}

pub(crate) fn rows(dir: &std::path::Path) -> R<Vec<Value>> {
    Ok(files(dir)?
        .into_iter()
        .filter_map(|(_, path)| serde_json::from_slice(&fs::read(path).ok()?).ok())
        .collect())
}

fn controller_rows(ctx: &Ctx) -> R<Vec<Value>> {
    let mut history = rows(&directory(ctx))?;
    if std::env::var_os("RWORKER_BUILDER_ARCHIVE").is_some() {
        let remote = crate::state::remote(ctx, &["__builder-history"])?;
        let builders: Vec<Value> = serde_json::from_str(&remote).map_err(|_| Fail(1))?;
        let seen: std::collections::HashSet<String> = history
            .iter()
            .filter_map(|row| row["name"].as_str().map(str::to_owned))
            .collect();
        history.extend(builders.into_iter().filter(|row| {
            row["name"]
                .as_str()
                .is_some_and(|name| !seen.contains(name))
        }));
    }
    Ok(history)
}

pub fn show(ctx: &Ctx) -> R {
    crate::ctx::outln(&serde_json::to_string(&controller_rows(ctx)?).map_err(|_| Fail(1))?);
    Ok(())
}

pub(super) fn with_retired(ctx: &Ctx, active: &str) -> R<String> {
    let mut active: Vec<Value> = serde_json::from_str(active).map_err(|_| Fail(1))?;
    let history: Vec<Value> = if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
        controller_rows(ctx)?
    } else {
        let mut cmd = crate::state::control_ssh_cmd(ctx, &["__cached-worker-history"]);
        let remote = crate::proc::capture_ok(ctx, cmd.stdin(Stdio::null()))?;
        serde_json::from_str(&remote).map_err(|_| Fail(1))?
    };
    let mut names: std::collections::HashSet<String> = active
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_owned))
        .collect();
    for mut row in history {
        if row["name"]
            .as_str()
            .is_some_and(|name| names.insert(name.to_owned()))
        {
            row["retired"] = json!(true);
            row["kind"] = json!("adhoc");
            row["status"] = json!(
                row["exit_code"]
                    .as_i64()
                    .map_or_else(|| "RETIRED".into(), |code| format!("EXIT {code}"),)
            );
            active.push(row);
        }
    }
    serde_json::to_string(&active).map_err(|_| Fail(1))
}

pub fn client(ctx: &Ctx) -> R {
    if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
        return show(ctx);
    }
    let result = crate::state::control_ssh_cmd(ctx, &["__cached-worker-history"])
        .stdin(Stdio::null())
        .status()
        .map_err(|_| Fail(1))?;
    crate::ctx::status(result.code().unwrap_or(1))
}
