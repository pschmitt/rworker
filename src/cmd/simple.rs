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
    let mut remote = vec!["niks3-token".to_string()];
    remote.extend(args.iter().cloned());
    remote.extend(["--owner".to_string(), ctx.owner()]);
    let out = state::remote(ctx, &remote)?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let v = parsed(&out);
    let name = jq::raw(get(&v, "name"));
    if jq::path(&v, &["niks3_token", "ok"]) == &Value::Bool(true) {
        paint::ok(
            ctx,
            &format!(
                "{} has a niks3 token until {}",
                paint::vm(ctx, &name),
                super::up::fmt_expiry(super::up::epoch(jq::path(
                    &v,
                    &["niks3_token", "expires_at"]
                )))
            ),
        );
        Ok(())
    } else {
        super::up::warn_niks3_token(ctx, &name, &v);
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
    let mut rows = vec!["PROFILE\tFLAVOR\tVCPU\tRAM (GB)\tDISK (GB)\tIMAGE TAGS".to_string()];
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
                1 => paint::flavor(ctx, text),
                2 => paint::paint(ctx, "2", text),
                _ => text.to_owned(),
            },
        ));
    } else {
        outln(&jq::column(&rows.join("\n")));
    }
    Ok(())
}

pub fn cmd_builder(ctx: &Ctx, args: &[String]) -> R {
    let Some(name) = args.first().filter(|a| !a.is_empty()) else {
        crate::help::show(ctx, "builder");
        return Err(Fail(2));
    };
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
    let line = format!(
        "ssh-ng://agent@{ip} x86_64-linux {identity} 16 1 nixos-test,benchmark,big-parallel,kvm - {key}"
    );
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &jq::compact(&json!({"name": name, "builder": line})));
    } else {
        outln(&line);
    }
    Ok(())
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
