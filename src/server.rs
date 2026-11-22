mod autoscale;
pub(crate) mod builder_gateway;
mod github;
mod github_agent_api;
mod github_leases;
mod github_oidc;
mod http;
mod inventory;
mod inventory_api;
mod inventory_cache;
mod metadata;
mod mutations;
mod netbird;
mod niks3;
mod openstack;
mod pools;
mod provision;
mod reap_command;
mod reaper;
mod selection;
pub(crate) mod socket;
mod tailscale;
mod up;
#[cfg(feature = "web")]
mod web;
mod worker_ssh;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Write};

use serde_json::{Value, json};

use crate::ctx::{Fail, R};

pub fn run(ctx: &crate::ctx::Ctx, args: &[String]) -> R {
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        crate::ctx::out(&crate::help::action_help(ctx, "server"));
        return Ok(());
    }

    let [config, action, rest @ ..] = args else {
        eprintln!("Usage: rworker server CONFIG ACTION [ARGS...]");
        if args.len() == 1 {
            eprintln!("error: the following arguments are required: command");
        }
        return Err(Fail(2));
    };
    if let Err(message) = validate_args(action, rest) {
        eprintln!("error: {message}");
        return Err(Fail(2));
    }
    if let Some(archive) = std::env::var_os("RWORKER_BUILDER_ARCHIVE_DIRECTORY") {
        let archive = std::path::Path::new(&archive);
        if action == "__builder-history" && rest.is_empty() {
            crate::ctx::outln(
                &serde_json::to_string(&crate::cmd::history::rows(
                    &archive.join("worker-history"),
                )?)
                .map_err(|_| Fail(1))?,
            );
            return Ok(());
        }
        if matches!(action.as_str(), "__builder-logs" | "__builder-preview") {
            return crate::cmd::log_cache::read_at(
                &archive.join("job-logs"),
                rest,
                action == "__builder-preview",
            );
        }
    }
    if action == "lifecycle-events" && (rest.is_empty() || rest == ["--latest-id"]) {
        let Some(dir) = crate::event_history::directory() else {
            eprintln!("Lifecycle event history is not configured on this controller");
            return Err(Fail(1));
        };
        let output = if rest.is_empty() {
            crate::event_history::list(&dir)
                .and_then(|events| serde_json::to_string(&events).map_err(io::Error::other))
        } else {
            crate::event_history::latest_id(&dir).map(|id| id.to_string())
        }
        .map_err(|error| {
            eprintln!("Read lifecycle events: {error}");
            Fail(1)
        })?;
        crate::ctx::outln(&output);
        return Ok(());
    }
    if action == "github-agent-api"
        && let [socket] = rest
    {
        return github_agent_api::serve(config, socket).map_err(|error| {
            eprintln!("GitHub agent API: {error}");
            Fail(1)
        });
    }
    let cache = inventory_cache::directory();
    if action == "refresh-inventory" && rest.is_empty() {
        return match cache {
            Some(dir) => inventory_cache::refresh_forever(config, &dir).map_err(|error| {
                eprintln!("Inventory cache: {error}");
                Fail(1)
            }),
            None => {
                eprintln!("RWORKER_INVENTORY_DIRECTORY is required");
                Err(Fail(2))
            }
        };
    }
    if action == "inventory-api"
        && let (Some(dir), [socket]) = (cache.as_ref(), rest)
    {
        return inventory_api::serve(dir.to_owned(), socket).map_err(|error| {
            eprintln!("Inventory API: {error}");
            Fail(1)
        });
    }
    let up = if action == "up" {
        match up::Args::parse(rest) {
            Ok(args) => Some(args),
            Err(error) => {
                eprintln!("error: {error}");
                return Err(Fail(2));
            }
        }
    } else {
        None
    };
    let edit = match action.as_str() {
        "up" | "github-up" | "github-status" | "github-down" | "github-reap" => None,
        "reap" if rest.is_empty() || rest == ["--dry-run"] => None,
        "autoscale" if rest.is_empty() || rest == ["--once"] => None,
        "profiles" if rest.is_empty() => None,
        "ls" if rest.is_empty() || rest == ["--fresh"] => None,
        "topic" | "claim" | "extend" | "niks3-token" | "down" => match parse_edit(action, rest) {
            Ok(edit) => Some(edit),
            Err(error) => {
                eprintln!("error: {error}");
                return Err(Fail(2));
            }
        },
        _ => {
            eprintln!("Invalid server command or arguments: {action}");
            return Err(Fail(2));
        }
    };
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if action == "ls"
            && let Some(dir) = &cache
        {
            let response = inventory_cache::list(dir, !rest.is_empty())?;
            let mut stdout = io::stdout().lock();
            serde_json::to_writer(&mut stdout, &response)?;
            writeln!(stdout)?;
            return Ok(());
        }
        let settings = serde_json::from_reader(BufReader::new(File::open(config)?))?;
        if action == "autoscale" {
            autoscale::run(&settings, !rest.is_empty())?;
            return Ok(());
        }
        if action == "reap" {
            reap_command::reap(&settings, !rest.is_empty())?;
            return Ok(());
        }
        let response = if matches!(
            action.as_str(),
            "github-up" | "github-status" | "github-down" | "github-reap"
        ) {
            github_leases::run(&settings, action, rest)?
        } else if let Some(args) = up {
            up::up(&settings, args)?
        } else if let Some(edit) = edit {
            let mut compute = openstack::OpenStack::from_settings(&settings)?;
            let pools = pools::Pools::from_settings(&settings)?;
            match action.as_str() {
                "down" => {
                    mutations::down(&settings, &mut compute, &pools, &edit)?;
                    writeln!(io::stdout().lock(), "deleted {}", edit.name)?;
                    return Ok(());
                }
                "extend" | "niks3-token" => {
                    mutations::expiry(&settings, &mut compute, &pools, &edit)?
                }
                _ => {
                    let action = if let Some(text) = edit.text {
                        mutations::Edit::Topic(text)
                    } else {
                        mutations::Edit::Claim(&edit.metadata)
                    };
                    mutations::edit(
                        &mut compute,
                        &pools,
                        edit.name,
                        edit.owner,
                        edit.force,
                        action,
                    )?
                }
            }
        } else {
            if action == "ls" {
                inventory::list(&settings)?
            } else {
                let mut rows = match cache
                    .as_ref()
                    .and_then(|dir| inventory_cache::read_profiles(dir))
                {
                    Some(Value::Array(rows)) => rows,
                    _ => profiles(&settings)?,
                };
                github_leases::add_disk_observations(&settings, &mut rows);
                Value::Array(rows)
            }
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        Ok(())
    })();
    // Some operations can change provider state before returning an error.
    if (action == "up"
        || matches!(
            action.as_str(),
            "down" | "extend" | "topic" | "claim" | "niks3-token"
        ))
        && let Some(dir) = &cache
        && let Err(error) = inventory_cache::invalidate(dir)
    {
        eprintln!("Invalidate worker inventory: {error}");
    }
    result.map_err(|error| {
        eprintln!("{error}");
        Fail(1)
    })
}

struct EditArgs<'a> {
    name: &'a str,
    owner: Option<&'a str>,
    force: bool,
    text: Option<&'a str>,
    metadata: Vec<String>,
    ttl: Option<u64>,
    no_token: bool,
}

fn parse_edit<'a>(action: &str, args: &'a [String]) -> Result<EditArgs<'a>, String> {
    let mut positional = Vec::new();
    let mut owner = None;
    let mut force = false;
    let mut ttl = None;
    let mut no_token = false;
    let mut metadata = Vec::new();
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            positional.extend(args);
            break;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        match flag {
            "--force" if inline.is_none() => force = true,
            "--no-niks3-token" if action == "extend" && inline.is_none() => no_token = true,
            "--ttl" if action == "extend" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or("argument --ttl: expected one argument")?;
                ttl = Some(
                    duration_seconds(value).ok_or_else(|| format!("invalid duration: {value}"))?,
                );
            }
            "--owner" | "--meta" if flag == "--owner" || action == "claim" => {
                let value = inline
                    .or_else(|| args.next().filter(|arg| !arg.starts_with("--")))
                    .ok_or_else(|| format!("argument {flag}: expected one argument"))?;
                if flag == "--owner" {
                    owner = Some(value);
                } else {
                    metadata.push(value.to_string());
                }
            }
            _ if arg.starts_with('-') && arg != "-" => {
                return Err(format!("unrecognized arguments: {arg}"));
            }
            _ => positional.push(arg),
        }
    }
    let (name, text) = match (action, positional.as_slice()) {
        ("topic", [name, text]) => (*name, Some(*text)),
        ("claim" | "extend" | "niks3-token" | "down", [name]) => (*name, None),
        ("topic", [_]) => return Err("the following arguments are required: text".into()),
        (_, []) => return Err("the following arguments are required: name".into()),
        _ => return Err("too many positional arguments".into()),
    };
    Ok(EditArgs {
        name,
        owner,
        force,
        text,
        metadata,
        ttl,
        no_token,
    })
}

// Validate before opening the settings file or initializing any providers.
fn validate_args(action: &str, args: &[String]) -> Result<(), String> {
    if !matches!(action, "up" | "extend") {
        return Ok(());
    }
    let mut ttl = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg.as_str(), None),
        };
        if !matches!(
            flag,
            "--ttl" | "--disk" | "--flavor" | "--profile" | "--owner" | "--meta" | "--ssh-key"
        ) {
            continue;
        }
        let value = inline
            .or_else(|| {
                args.next()
                    .map(String::as_str)
                    .filter(|v| !v.starts_with("--"))
            })
            .ok_or_else(|| format!("argument {flag}: expected one argument"))?;
        match flag {
            "--ttl" => {
                duration_seconds(value).ok_or_else(|| format!("invalid duration: {value}"))?;
                ttl = true;
            }
            "--disk" if value.parse::<i64>().is_err() => {
                return Err(format!("invalid int value: '{value}'"));
            }
            _ => {}
        }
    }
    if action == "extend" && !ttl {
        return Err("the following arguments are required: --ttl".into());
    }
    Ok(())
}

fn duration_seconds(value: &str) -> Option<u64> {
    fn digits(value: &str) -> Option<u64> {
        if value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse().ok()
        } else {
            None
        }
    }

    if let Some(seconds) = digits(value) {
        return Some(seconds);
    }
    if let Some((hours, rest)) = value.split_once('h') {
        let hours = digits(hours)?.checked_mul(3600)?;
        return if rest.is_empty() {
            Some(hours)
        } else {
            hours.checked_add(digits(rest.strip_suffix('m')?)?.checked_mul(60)?)
        };
    }
    digits(value.strip_suffix('m')?)?.checked_mul(60)
}

fn required<'a>(settings: &'a Value, key: &str) -> io::Result<&'a Value> {
    settings.get(key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Missing setting: {key}"),
        )
    })
}

fn profiles(settings: &Value) -> io::Result<Vec<Value>> {
    let mut profiles: Vec<_> = match settings.get("profiles") {
        Some(profiles) => profiles
            .as_object()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Expected a profiles object")
            })?
            .iter()
            .collect(),
        None => Vec::new(),
    };
    profiles.sort_unstable_by_key(|(name, _)| *name);
    let default = settings.get("defaultProfile").and_then(Value::as_str);
    let mut rows = profiles
        .into_iter()
        .map(|(name, profile)| {
            if !profile.is_object() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected an object for profile {name}"),
                ));
            }
            let flavor = profile
                .get("flavor")
                .unwrap_or(required(settings, "adhocFlavor")?);
            let disk = profile
                .get("disk")
                .unwrap_or(required(settings, "adhocVolumeSize")?);
            Ok(json!({
                "name": name,
                "default": default == Some(name.as_str())
                    || (settings.get("defaultProfile").is_none() && name == "standard"),
                "flavor": flavor,
                "disk": disk,
                "image_tags": profile.get("imageTags").cloned().unwrap_or_else(|| json!([])),
            }))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if let Ok(compute) = openstack::OpenStack::from_settings(settings) {
        let mut flavors = HashMap::new();
        for profile in &mut rows {
            let Some(flavor) = profile["flavor"].as_str() else {
                continue;
            };
            // Profiles sharing a flavor also share its lookup, including failures.
            let size = flavors.entry(flavor.to_owned()).or_insert_with(|| {
                let result = compute.flavor(flavor).ok()?;
                Some((result["vcpus"].as_u64()?, result["ram"].as_u64()?))
            });
            if let Some((vcpus, ram_mib)) = *size {
                profile["vcpus"] = json!(vcpus);
                profile["ram_mib"] = json!(ram_mib);
            }
        }
    }
    let gha = &settings["githubActions"];
    if let Some(repository) = gha["repository"].as_str() {
        let specs = gha["tokenFile"]
            .as_str()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|token| {
                github::GitHub::new(repository, token.trim())
                    .ok()?
                    .public_repository()
            })
            // GitHub's standard public and private Ubuntu runner specifications.
            .map(|public| if public { (4, 16) } else { (2, 8) });
        rows.extend(crate::github_profiles::PROFILES.iter().map(|profile| {
            json!({
                "name": profile.name,
                "provider": "github",
                "system": profile.system,
                "flavor": profile.label,
                "vcpus": specs.map(|(cpus, _)| cpus),
                "ram_gb": specs.map(|(_, ram)| ram),
                "disk_gb": 14,
                "image_tags": ["github-actions"],
                "default": false,
            })
        }));
    }
    Ok(rows)
}

fn epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
