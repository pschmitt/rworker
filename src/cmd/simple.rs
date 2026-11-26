//! Actions that are one call to the control host: down, extend, topic,
//! claim, niks3-token and profiles, plus builder.

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint;
use crate::state;
use serde_json::{Value, json};

fn parsed(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

pub fn cmd_down(ctx: &Ctx, args: &[String]) -> R {
    let Some(name) = args.first().filter(|a| !a.is_empty()) else {
        crate::help::show(ctx, "down");
        return Err(Fail(2));
    };
    let mut remote = vec![
        "down".to_string(),
        name.clone(),
        "--owner".into(),
        ctx.owner(),
    ];
    if args.get(1).map(String::as_str) == Some("--force") {
        remote.push("--force".into());
    }
    if let Some((forge, id)) = state::hosted_id(name) {
        remote = vec![format!("{}-down", forge.key()), id.into(), ctx.owner()];
    }
    if state::remote_quiet(ctx, &remote)? != 0 {
        return Err(Fail(1));
    }
    state::forget(ctx, name);
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &jq::compact(&json!({"name": name, "deleted": true})));
    } else {
        paint::ok(ctx, &format!("deleted {}", paint::vm(ctx, name)));
    }
    Ok(())
}

pub fn cmd_extend(ctx: &Ctx, args: &[String]) -> R {
    let mut remote = vec!["extend".to_string()];
    remote.extend(args.iter().cloned());
    remote.extend(["--owner".to_string(), ctx.owner()]);
    let out = state::remote(ctx, &remote)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let v = parsed(&out);
    let name = jq::raw(get(&v, "name"));
    paint::ok(
        ctx,
        &format!(
            "{} now expires {}, at most until {}",
            paint::vm(ctx, &name),
            super::up::fmt_expiry(super::up::epoch(get(&v, "expires_at"))),
            crate::time::weekday_hm(super::up::epoch(get(&v, "max_lifetime_end")))
        ),
    );
    super::up::warn_niks3_token(ctx, &name, &v);
    Ok(())
}

pub fn cmd_topic(ctx: &Ctx, args: &[String]) -> R {
    let mut name: Option<String> = None;
    let mut text: Vec<&str> = Vec::new();
    let mut force = false;
    for a in args {
        match a.as_str() {
            "--force" => force = true,
            other => {
                if name.as_deref().unwrap_or("").is_empty() {
                    name = Some(other.to_string());
                } else {
                    text.push(other);
                }
            }
        }
    }
    let name = name.unwrap_or_default();
    if name.is_empty() {
        crate::help::show(ctx, "topic");
        return Err(Fail(2));
    }
    let mut remote = vec!["topic".to_string(), name.clone(), text.join(" ")];
    if force {
        remote.push("--force".into());
    }
    remote.extend(["--owner".to_string(), ctx.owner()]);
    let out = state::remote(ctx, &remote)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let topic = jq::or(get(&parsed(&out), "topic"), "-");
    paint::ok(ctx, &format!("{} topic: {topic}", paint::vm(ctx, &name)));
    Ok(())
}

pub fn cmd_claim(ctx: &Ctx, args: &[String]) -> R {
    let mut name = String::new();
    let (mut topic, mut agent_id) = (String::new(), String::new());
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--force" => force = true,
            flag @ ("--topic" | "--agent-id") => {
                // `shift` past the end stopped the bash client (set -e).
                let Some(v) = args.get(i + 1) else {
                    return Err(Fail(1));
                };
                if flag == "--topic" {
                    topic = v.clone();
                } else {
                    agent_id = v.clone();
                }
                i += 1;
            }
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown claim option: {s}"));
                crate::help::show(ctx, "claim");
                return Err(Fail(2));
            }
            s => {
                if !name.is_empty() {
                    crate::help::show(ctx, "claim");
                    return Err(Fail(2));
                }
                name = s.to_string();
            }
        }
        i += 1;
    }
    if name.is_empty() {
        crate::help::show(ctx, "claim");
        return Err(Fail(2));
    }
    if topic.is_empty() {
        ctx.err(&format!("{} no topic provided", paint::warn_mark(ctx)));
    }
    let any_id = [
        agent_id.clone(),
        crate::ctx::env("RWORKER_AGENT_ID"),
        crate::ctx::env("CONCORD_AGENT_ID"),
    ]
    .iter()
    .any(|s| !s.is_empty());
    if !any_id {
        ctx.err(&format!(
            "{} no agent id provided (--agent-id, RWORKER_AGENT_ID)",
            paint::warn_mark(ctx)
        ));
    }
    let mut remote = vec!["claim".to_string(), name.clone()];
    remote.extend(crate::creator::meta(&topic, &agent_id));
    if force {
        remote.push("--force".into());
    }
    remote.extend(["--owner".to_string(), ctx.owner()]);
    let out = state::remote(ctx, &remote)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let v = parsed(&out);
    paint::ok(
        ctx,
        &format!(
            "{} claimed: {} {}, topic: {}",
            paint::vm(ctx, &name),
            jq::or(get(&v, "agent"), "-"),
            jq::or(get(&v, "agent_id"), "-"),
            jq::or(get(&v, "topic"), "-")
        ),
    );
    Ok(())
}

pub fn cmd_niks3_token(ctx: &Ctx, args: &[String]) -> R {
    if args.first().is_none_or(String::is_empty) {
        crate::help::show(ctx, "niks3-token");
        return Err(Fail(2));
    }
    if let Some(name) = args.iter().find(|arg| !arg.starts_with('-'))
        && crate::state::hosted_id(name).is_some()
    {
        let force = args.iter().any(|arg| arg == "--force");
        let result = super::nix_cache::issue_lease_niks3_token(ctx, name, force);
        let v = json!({"name": name, "niks3_token": result});
        if ctx.json.get() {
            crate::ctx::json_out(ctx, &jq::compact(&v));
            return if result["ok"] == true {
                Ok(())
            } else {
                Err(Fail(1))
            };
        }
        return report_niks3_token(ctx, &v);
    }
    let mut remote = vec!["niks3-token".to_string()];
    remote.extend(args.iter().cloned());
    remote.extend(["--owner".to_string(), ctx.owner()]);
    let out = state::remote(ctx, &remote)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    report_niks3_token(ctx, &parsed(&out))
}

fn report_niks3_token(ctx: &Ctx, v: &Value) -> R {
    let name = jq::raw(get(v, "name"));
    if jq::path(v, &["niks3_token", "ok"]) == &Value::Bool(true) {
        paint::ok(
            ctx,
            &format!(
                "{} has a niks3 token until {}",
                paint::vm(ctx, &name),
                super::up::fmt_expiry(super::up::epoch(jq::path(
                    v,
                    &["niks3_token", "expires_at"]
                )))
            ),
        );
        Ok(())
    } else {
        super::up::warn_niks3_token(ctx, &name, v);
        Err(Fail(1))
    }
}

pub fn cmd_profiles(ctx: &Ctx, args: &[String]) -> R {
    let (mut wide, mut json, mut profile) = (false, ctx.json.get(), None);
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-o" => match args.next() {
                Some("wide") => wide = true,
                Some("json") => json = true,
                _ => {
                    ctx.err("Output format must be json or wide");
                    return Err(Fail(2));
                }
            },
            "--wide" | "-owide" => wide = true,
            "-ojson" => json = true,
            name if !name.starts_with('-') && profile.is_none() => profile = Some(name),
            _ => {
                ctx.err(&format!("Unknown profile argument: {arg}"));
                crate::help::show(ctx, "profiles");
                return Err(Fail(2));
            }
        }
    }
    wide |= profile.is_some();
    let mut query = vec!["profiles"];
    if wide {
        query.push("--details");
        query.extend(profile);
    }
    let out = state::remote(ctx, &query)?;
    let v = state::parse(ctx, &out)?;
    let Value::Array(mut list) = v else {
        ctx.err("jq: error (at <stdin>:0): Cannot sort, input must be an array");
        return Err(Fail(5));
    };
    list.sort_by(|a, b| {
        let key = |w: &Value| json!([!jq::truthy(get(w, "default")), get(w, "name")]);
        jq::cmp(&key(a), &key(b))
    });
    if list.is_empty()
        && let Some(name) = profile
    {
        ctx.err(&format!("Unknown profile: {name}"));
        return Err(Fail(1));
    }
    if json {
        crate::ctx::json_out(ctx, &jq::pretty(&Value::Array(list)));
        return Ok(());
    }
    let mut rows = vec!["PROFILE\tARCH\tFLAVOR\tVCPU\tRAM (GB)\tDISK (GB)\tTAGS".to_string()];
    if wide {
        rows[0].push_str("\tPROVIDER\tIMAGE / RUNNER\tIMAGE ID\tIMAGE CREATED");
    }
    for p in &list {
        let star = if jq::truthy(get(p, "default")) {
            "*"
        } else {
            ""
        };
        let tags: Vec<String> = get(p, "image_tags")
            .as_array()
            .map(|a| a.iter().map(jq::raw).collect())
            .unwrap_or_default();
        let [ram, disk] = crate::table::profile_sizes(p);
        let name = jq::raw(get(p, "name"));
        let label = if ctx.icons.get() {
            format!("{} {name}{star}", crate::pool_icon::profile_glyph(p))
        } else {
            format!("{name}{star}")
        };
        let mut row = [
            jq::tsv(&json!(label)),
            jq::tsv(&json!(jq::or(&p["arch"], "-"))),
            jq::tsv(get(p, "flavor")),
            jq::tsv(&json!(
                p.get("vcpus")
                    .filter(|value| !value.is_null())
                    .map_or_else(|| "-".to_owned(), jq::raw)
            )),
            ram,
            disk,
            jq::tsv(&json!(tags.join(", "))),
        ]
        .join("\t");
        if wide {
            let forge = p["provider"]
                .as_str()
                .and_then(crate::github_profiles::Forge::from_key);
            let provider = forge.map_or("OpenStack", crate::github_profiles::Forge::label);
            let image = if forge.is_some() {
                jq::or(&p["flavor"], "-")
            } else {
                jq::or(&p["image_name"], &jq::or(&p["image_status"], "-"))
            };
            for value in [
                provider.to_owned(),
                image,
                jq::or(&p["image_id"], "-"),
                jq::or(&p["image_created_at"], "-"),
            ] {
                row.push('\t');
                row.push_str(&jq::tsv(&json!(value)));
            }
        }
        rows.push(row);
    }
    if ctx.color {
        let cells: Vec<_> = rows.iter().map(|row| row.split('\t').collect()).collect();
        outln(&crate::table::styled_columns(
            ctx,
            &cells,
            |col, text| match col {
                0 => {
                    let (name, marker) = text
                        .strip_suffix('*')
                        .map_or((text, ""), |name| (name, "*"));
                    let (icon, name) = match name.split_once(' ') {
                        Some((glyph @ crate::pool_icon::GITHUB, name)) if ctx.icons.get() => {
                            (paint::paint(ctx, "38;2;0;0;0", glyph), name)
                        }
                        Some((glyph @ crate::pool_icon::GITLAB, name)) if ctx.icons.get() => {
                            (paint::paint(ctx, "38;2;226;67;41", glyph), name)
                        }
                        Some((glyph @ crate::pool_icon::OPENSTACK, name)) if ctx.icons.get() => {
                            (paint::paint(ctx, "38;2;218;26;50", glyph), name)
                        }
                        _ => (String::new(), name),
                    };
                    let gap = if icon.is_empty() { "" } else { " " };
                    format!(
                        "{icon}{gap}{}{}",
                        paint::vm(ctx, name),
                        paint::paint(ctx, "32", marker)
                    )
                }
                2 => paint::flavor(ctx, text),
                3 => paint::paint(ctx, "2", text),
                _ => text.to_owned(),
            },
        ));
    } else {
        outln(&jq::column(&rows.join("\n")));
    }
    Ok(())
}

/// `builder` options for the on-demand gateway.
#[derive(Default)]
struct GatewayArgs {
    selector: Option<String>,
    gateway: Option<String>,
    identity: Option<String>,
    host_key: Option<String>,
    system: Option<String>,
    jobs: Option<String>,
    features: Option<String>,
}

fn parse_builder_args(args: &[String]) -> Result<(Option<String>, GatewayArgs), String> {
    let mut name = None;
    let mut options = GatewayArgs::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let slot = match flag {
            "--profile" | "--arch" | "--pool" => {
                if options.selector.is_some() {
                    return Err("Pass only one of --profile, --arch and --pool".into());
                }
                &mut options.selector
            }
            "--gateway" => &mut options.gateway,
            "-i" | "--identity" => &mut options.identity,
            "--host-key" => &mut options.host_key,
            "--system" => &mut options.system,
            "--jobs" => &mut options.jobs,
            "--features" => &mut options.features,
            _ if arg.starts_with('-') => return Err(format!("Unknown option: {arg}")),
            _ if name.is_none() && !arg.is_empty() => {
                name = Some(arg.clone());
                continue;
            }
            _ => return Err(format!("Unexpected argument: {arg}")),
        };
        let value = inline
            .or_else(|| args.next().cloned())
            .filter(|value| !value.is_empty() && !value.contains(char::is_whitespace))
            .ok_or_else(|| format!("{flag} needs a value"))?;
        *slot = Some(match flag {
            "--profile" | "--arch" | "--pool" => {
                let selector = format!("{}:{value}", &flag[2..]);
                if !valid_selector(&selector) {
                    return Err(format!("Invalid {flag} value: {value}"));
                }
                selector
            }
            _ => value,
        });
    }
    Ok((name, options))
}

fn valid_selector(selector: &str) -> bool {
    selector.split_once(':').is_some_and(|(_, name)| {
        (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    })
}

/// The system a selector builds, when the selector itself says so.
fn selector_system(selector: Option<&str>) -> &'static str {
    let arch = match selector.and_then(|selector| selector.split_once(':')) {
        Some(("arch", arch)) => arch.to_ascii_lowercase(),
        Some(("profile", profile)) => {
            return crate::github_profiles::from_name(profile)
                .map_or("x86_64-linux", |profile| profile.system);
        }
        _ => return "x86_64-linux",
    };
    match arch.as_str() {
        "aarch64" | "arm64" | "aarch64-linux" => "aarch64-linux",
        _ => "x86_64-linux",
    }
}

/// The gateway's host name and its pinned host key, from the SSH
/// configuration and known_hosts of this user: the Nix daemon has neither.
fn gateway_host(ctx: &Ctx, host: &str) -> R<(String, Option<String>)> {
    let output = std::process::Command::new("ssh")
        .args(["-G", "--", host])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| {
            ctx.err("Could not read the SSH configuration");
            Fail(1)
        })?;
    let config = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| {
        config.lines().find_map(|line| {
            line.split_once(' ')
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| value.to_owned())
        })
    };
    let hostname = value("hostname").unwrap_or_else(|| host.to_owned());
    let alias = value("hostkeyalias").unwrap_or_else(|| hostname.clone());
    let port = value("port").filter(|port| port != "22");
    let lookup = port
        .as_ref()
        .map_or_else(|| alias.clone(), |port| format!("[{alias}]:{port}"));
    let files: Vec<String> = ["userknownhostsfile", "globalknownhostsfile"]
        .into_iter()
        .filter_map(value)
        .flat_map(|files| {
            files
                .split_whitespace()
                .map(|file| match file.strip_prefix("~/") {
                    Some(rest) => format!("{}/{rest}", ctx.home),
                    None => file.to_owned(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut keys = Vec::new();
    for file in files {
        let Ok(found) = std::process::Command::new("ssh-keygen")
            .args(["-F", &lookup, "-f", &file])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        keys.extend(
            String::from_utf8_lossy(&found.stdout)
                .lines()
                .filter(|line| !line.starts_with('#'))
                .filter_map(|line| {
                    let mut fields = line.split_whitespace().skip(1);
                    Some(format!("{} {}", fields.next()?, fields.next()?))
                })
                .collect::<Vec<_>>(),
        );
    }
    keys.sort_by_key(|key| !key.starts_with("ssh-ed25519 "));
    let hostname = port.map_or(hostname.clone(), |port| format!("{hostname}:{port}"));
    Ok((hostname, keys.into_iter().next()))
}

fn gateway_builder(ctx: &Ctx, options: GatewayArgs) -> R {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let host = options
        .gateway
        .or_else(|| env("RWORKER_BUILDER_HOST"))
        .unwrap_or_else(|| ctx.control_host.clone());
    let (hostname, known_key) = gateway_host(ctx, &host)?;
    let key = match options.host_key.or_else(|| env("RWORKER_BUILDER_HOST_KEY")) {
        Some(key) => key,
        None => base64(
            format!(
                "{}\n",
                known_key.ok_or_else(|| {
                    ctx.err(&format!(
                        "No known host key for {host}; pass --host-key or set RWORKER_BUILDER_HOST_KEY"
                    ));
                    Fail(1)
                })?
            )
            .as_bytes(),
        ),
    };
    let identity = options
        .identity
        .or_else(|| env("RWORKER_BUILDER_IDENTITY"))
        .unwrap_or_else(|| "-".into());
    let hosted = options
        .selector
        .as_deref()
        .and_then(|selector| selector.strip_prefix("profile:"))
        .and_then(crate::github_profiles::from_name)
        .is_some()
        || selector_system(options.selector.as_deref()) == "aarch64-linux";
    let system = options
        .system
        .unwrap_or_else(|| selector_system(options.selector.as_deref()).into());
    let jobs = options
        .jobs
        .unwrap_or_else(|| if hosted { "1" } else { "16" }.into());
    let features = options.features.unwrap_or_else(|| {
        if hosted {
            "-"
        } else {
            "nixos-test,benchmark,big-parallel,kvm"
        }
        .into()
    });
    let query = options
        .selector
        .as_deref()
        .map(|selector| format!("?remote-program={selector}"))
        .unwrap_or_default();
    let line = format!(
        "ssh-ng://rworker-builder@{hostname}{query} {system} {identity} {jobs} 1 {features} - {key}"
    );
    if ctx.json.get() {
        crate::ctx::json_out(
            ctx,
            &jq::compact(&json!({
                "gateway": hostname, "selector": options.selector, "builder": line,
            })),
        );
    } else {
        outln(&line);
    }
    Ok(())
}

pub fn cmd_builder(ctx: &Ctx, args: &[String]) -> R {
    let (name, options) = parse_builder_args(args).map_err(|error| {
        ctx.err(&error);
        Fail(2)
    })?;
    let Some(name) = name else {
        return gateway_builder(ctx, options);
    };
    if options.selector.is_some() || options.gateway.is_some() || options.host_key.is_some() {
        ctx.err("--profile, --arch, --pool, --gateway and --host-key need no worker name: the gateway leases the worker");
        return Err(Fail(2));
    }
    let name = &name;
    let hosted = state::hosted_id(name);
    if hosted.is_some() && !ctx.json.get() {
        ctx.err("Hosted builder connections require builder --json and its SSH configuration");
        return Err(Fail(2));
    }
    let ip = state::worker_ip(ctx, name)?;
    let key = state::worker_field(ctx, name, "host_key").map_err(|_| Fail(1))?;
    // `jq -r` printed the key with a newline, and base64 encoded it too.
    let key = base64(format!("{key}\n").as_bytes());
    // The Nix daemon opens builder connections as root, without the user's
    // SSH agent or config, so the line names the private half of the key
    // that `up` handed to the worker ("-" makes root try its own key).
    let identity = ctx.key_file();
    let mut identity = identity
        .strip_suffix(".pub")
        .unwrap_or(&identity)
        .to_string();
    if std::fs::File::open(&identity).is_err() {
        identity = "-".to_string();
    }
    let (line, ssh_config) = if let Some((forge, _)) = hosted {
        let profile = state::worker_field(ctx, name, "profile")?;
        let Some(profile) =
            crate::github_profiles::from_name(&profile).filter(|p| p.forge == forge)
        else {
            ctx.err("Unknown hosted builder profile");
            return Err(Fail(1));
        };
        let port = state::worker_field(ctx, name, "github_port")?
            .parse::<u64>()
            .ok()
            .filter(|port| (1024..=65535).contains(port))
            .ok_or_else(|| {
                ctx.err("Invalid hosted builder relay port");
                Fail(1)
            })?;
        let proxy = state::hosted_proxy_command(ctx, port);
        if forge != crate::github_profiles::Forge::GitHub {
            ctx.err("This hosted profile does not provide an isolated Nix builder store");
            return Err(Fail(2));
        }
        let features = if state::worker_field(ctx, name, "kvm").as_deref() == Ok("true") {
            "kvm"
        } else {
            "-"
        };
        (
            format!(
                "ssh-ng://{user}@{name}?remote-program={home}/.local/bin/rworker-nix-daemon {} {identity} 1 1 {features} - {key}",
                profile.system,
                user = state::login_user(ctx, name),
                home = crate::worker_user::home(&state::login_user(ctx, name)),
            ),
            Some(format!("Host {name}\n  ProxyCommand {proxy}\n")),
        )
    } else {
        (
            format!(
                "ssh-ng://{user}@{ip} x86_64-linux {identity} 16 1 nixos-test,benchmark,big-parallel,kvm - {key}",
                user = state::login_user(ctx, name),
            ),
            None,
        )
    };
    if ctx.json.get() {
        let mut result = json!({"name": name, "builder": line});
        if let Some(config) = ssh_config {
            result["ssh_config"] = json!(config);
        }
        crate::ctx::json_out(ctx, &jq::compact(&result));
    } else {
        outln(&line);
    }
    Ok(())
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn builder_takes_an_optional_name_and_one_selector() {
        let (name, options) = parse_builder_args(&args(&["--arch", "aarch64"])).unwrap();
        assert!(name.is_none());
        assert_eq!(options.selector.as_deref(), Some("arch:aarch64"));
        assert_eq!(
            selector_system(options.selector.as_deref()),
            "aarch64-linux"
        );
        let (name, options) =
            parse_builder_args(&args(&["--profile=large-builder", "--jobs", "4"])).unwrap();
        assert!(name.is_none());
        assert_eq!(options.selector.as_deref(), Some("profile:large-builder"));
        assert_eq!(options.jobs.as_deref(), Some("4"));
        assert_eq!(selector_system(Some("profile:gh-aarch64")), "aarch64-linux");
        assert_eq!(selector_system(Some("pool:circus")), "x86_64-linux");
        let (name, _) = parse_builder_args(&args(&["adhoc-1234"])).unwrap();
        assert_eq!(name.as_deref(), Some("adhoc-1234"));
        assert!(parse_builder_args(&args(&["--arch", "x86_64", "--pool", "circus"])).is_err());
        assert!(parse_builder_args(&args(&["--profile", "a b"])).is_err());
        assert!(parse_builder_args(&args(&["--pool"])).is_err());
        assert!(parse_builder_args(&args(&["--bogus"])).is_err());
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        assert_eq!(
            super::base64(b"ssh-ed25519 AAAA\n"),
            "c3NoLWVkMjU1MTkgQUFBQQo="
        );
        assert_eq!(super::base64(b"ab"), "YWI=");
        assert_eq!(super::base64(b""), "");
    }
}
