//! `setup nix`: get a worker ready to use this host's Nix. Over SSH (pinned
//! host key) it pushes
//!   - the cache part: the s3:// substituters, the trusted keys and the AWS
//!     profiles they name, so the worker substitutes from the private cache;
//!   - the auth part: this host's `access-tokens`, so the worker fetches
//!     private flake inputs itself instead of receiving them from here.
//!
//! Nothing else of this host's nix.conf leaves it (it can hold netrc paths).

use crate::ctx::{Ctx, Fail, R, outln};
use crate::jq::{self, get};
use crate::paint;
use crate::proc;
use crate::state;
use regex::Regex;
use serde_json::{Value, json};
use std::process::Command;

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(jq::raw).collect())
        .unwrap_or_default()
}

fn unique(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// What `setup nix` pushes. Both parts by default.
#[derive(Clone, Debug)]
pub struct Opts {
    /// The S3 cache: substituters, trusted keys, AWS profiles.
    pub cache: bool,
    /// This host's access-tokens (private flake inputs).
    pub auth: bool,
    /// Only the tokens of these hosts (empty: all of them).
    pub auth_hosts: Vec<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            cache: true,
            auth: true,
            auth_hosts: Vec::new(),
        }
    }
}

fn read_config(ctx: &Ctx) -> R<Value> {
    let out = Command::new("nix")
        .args(["config", "show", "--json"])
        .stderr(std::process::Stdio::null())
        .output();
    let cfg = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            ctx.err("Could not read the Nix configuration (nix config show)");
            return Err(Fail(1));
        }
    };
    state::parse(ctx, &cfg)
}

/// The access-tokens of this host's Nix as (host, token), sorted by host and
/// limited to `hosts` when that is not empty. Entries nix.conf cannot carry
/// (whitespace or `#` would split or comment them out) are dropped. The
/// values stay in memory: they are pushed over SSH and never printed.
fn access_tokens(cfg: &Value, hosts: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = cfg["access-tokens"]["value"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(host, token)| Some((host.clone(), token.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    out.retain(|(host, token)| {
        let ok = |t: &str| !t.is_empty() && !t.chars().any(|c| c.is_whitespace() || c == '#');
        ok(host) && ok(token) && (hosts.is_empty() || hosts.contains(host))
    });
    out.sort();
    out
}

/// The s3:// substituters, the AWS profiles they name, the trusted public
/// keys and the hosts that have an access-token in this host's Nix (names
/// only). Non-S3 caches are not reachable from OpenStack anyway.
pub fn settings(ctx: &Ctx) -> R<Value> {
    let cfg = read_config(ctx)?;
    Ok(derive(&cfg))
}

fn derive(cfg: &Value) -> Value {
    let vals = |k: &str| strings(jq::alt(jq::path(cfg, &[k, "value"]), &json!([])));
    let subs = unique(
        [vals("substituters"), vals("extra-substituters")]
            .concat()
            .into_iter()
            .filter(|s| s.starts_with("s3://"))
            .collect(),
    );
    let re = Regex::new(r"[?&]profile=(?<p>[^&]+)").expect("valid regex");
    let profiles = unique(
        subs.iter()
            .map(|s| {
                re.captures(s)
                    .map(|c| c["p"].to_string())
                    .unwrap_or_else(|| "default".to_string())
            })
            .collect(),
    );
    let keys = unique(
        [
            vals("trusted-public-keys"),
            vals("extra-trusted-public-keys"),
        ]
        .concat(),
    );
    let auth_hosts: Vec<String> = access_tokens(cfg, &[])
        .into_iter()
        .map(|(h, _)| h)
        .collect();
    json!({"substituters": subs, "profiles": profiles, "keys": keys, "auth_hosts": auth_hosts})
}

/// One profile section ("[name]" and its lines) of an AWS credentials file.
fn aws_profile_section(file: &str, profile: &str) -> String {
    let Ok(text) = std::fs::read_to_string(file) else {
        return String::new();
    };
    let want = format!("[{profile}]");
    let mut found = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line == want {
            found = true;
            out.push(line);
            continue;
        }
        if line.starts_with('[') {
            found = false;
        }
        if found {
            out.push(line);
        }
    }
    proc::trim_nl(&out.join("\n"))
}

/// The sections of `profiles` in an AWS credentials file, one after the other,
/// or the name of the first profile that is missing.
fn aws_sections(file: &str, profiles: &[String]) -> Result<String, String> {
    let mut out = String::new();
    for profile in profiles {
        let section = aws_profile_section(file, profile);
        if section.is_empty() {
            return Err(profile.clone());
        }
        out.push_str(&section);
        out.push('\n');
    }
    Ok(out)
}

fn ssh_capture(opts: &[String], login: &str, script: &str) -> String {
    Command::new("ssh")
        .args(opts)
        .arg(login)
        .arg(script)
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| proc::trim_nl(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// The nix.conf pushed to a worker. An OpenStack worker keeps the system
/// nix.conf, so `extra-` adds to it. A CI runner already has an
/// `access-tokens` line (its own GitHub token) and Nix keeps that entry over
/// an `extra-access-tokens` one, so a private input would 404: a lease gets
/// plain `access-tokens`, which replaces it.
fn render_conf(
    header: &str,
    cache: Option<(&[String], Vec<String>)>,
    tokens: &[(String, String)],
    lease: bool,
) -> String {
    let mut conf = header.to_owned();
    if let Some((subs, keys)) = cache {
        conf.push_str(&format!(
            "\nextra-substituters = {}\nextra-trusted-public-keys = {}",
            subs.join(" "),
            keys.join(" ")
        ));
    }
    if !tokens.is_empty() {
        let pairs: Vec<String> = tokens.iter().map(|(h, t)| format!("{h}={t}")).collect();
        let key = if lease {
            "access-tokens"
        } else {
            "extra-access-tokens"
        };
        conf.push_str(&format!("\n{key} = {}", pairs.join(" ")));
    }
    conf
}

/// The account of a CI lease and where it keeps what `setup nix` pushes
/// (under its home, 0700 directory, 0600 files).
const LEASE_USER: &str = "rworker-build";

/// Shell for a lease: write stdin plus `tail` to `$HOME/.config/rworker-nix/FILE`.
fn lease_write(file: &str, tail: &str) -> String {
    format!(
        "umask 077; d=\"$HOME/.config/rworker-nix\"; mkdir -p \"$d\" && chmod 0700 \"$d\" && {{ cat; {tail} }} > \"$d/{file}.new\" && mv -f \"$d/{file}.new\" \"$d/{file}\""
    )
}

/// What a GitHub lease adds to the pushed nix.conf for the cache: sessions
/// use the lease's own store (in the build user's home, see
/// scripts/github-job-agent.py) so the S3 substituters are trusted and read
/// the AWS credentials of the session. The system daemon would ignore the
/// substituters of an untrusted user.
const GITHUB_STORE_LINES: &str = "printf 'build-users-group =\\nsandbox = true\\nstore = %s/.local/share/nix/rworker\\n' \"$HOME\";";

/// `setup nix` on a CI lease: the files go to the build user's home and the
/// agent's session environment (NIX_USER_CONF_FILES,
/// AWS_SHARED_CREDENTIALS_FILE) picks them up; no daemon to restart. The
/// cache goes into nix.conf only after the runner's Nix has read the bucket
/// with the credentials: Ubuntu's Nix, which a GitLab job installs, cannot
/// open s3:// stores at all, and listing one as a substituter would only
/// make every build complain.
#[allow(clippy::too_many_arguments)]
fn push_lease(
    ctx: &Ctx,
    name: &str,
    forge: crate::github_profiles::Forge,
    header: &str,
    cache: Option<(&[String], Vec<String>)>,
    tokens: &[(String, String)],
    creds: &str,
    shown: &dyn Fn(bool, &str, bool) -> Value,
) -> R<String> {
    let opts_ssh = state::ssh_opts(ctx, name);
    if opts_ssh.is_empty() {
        ctx.err(&format!("No pinned host key or relay known for {name}"));
        return Err(Fail(1));
    }
    let login = format!("{LEASE_USER}@{name}");
    for _ in 1..=3 {
        if super::up::probe(&opts_ssh, &login) {
            break;
        }
        proc::sleep_secs(2.0);
    }
    let mut verified = true;
    let mut cache = cache;
    if let Some((subs, _)) = &cache {
        let code = proc::run_with_input(
            ctx,
            state::worker_ssh(&opts_ssh, &login).arg(lease_write("aws-credentials", "")),
            creds.as_bytes(),
        )?;
        crate::ctx::status(code).map_err(|_| Fail(1))?;
        // `nix store info` is `nix store ping` before Nix 2.19.
        let url = proc::shell_quote(&subs[0]);
        let check = Command::new("ssh")
            .args(&opts_ssh)
            .arg(&login)
            .arg(format!(
                "export AWS_SHARED_CREDENTIALS_FILE=\"$HOME/.config/rworker-nix/aws-credentials\"; timeout 60 nix store info --store {url} || timeout 60 nix store ping --store {url}"
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        verified = match check {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let lines: Vec<&str> = err.lines().collect();
                let tail = lines[lines.len().saturating_sub(3)..].join("\n");
                ctx.err(&format!("The worker cannot reach the S3 cache: {tail}"));
                false
            }
            Err(_) => false,
        };
        if !verified {
            // Keep the unusable cache out of the runner's Nix.
            cache = None;
            let _ = Command::new("ssh")
                .args(&opts_ssh)
                .arg(&login)
                .arg("rm -f \"$HOME/.config/rworker-nix/aws-credentials\"")
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let store = if cache.is_some() && forge == crate::github_profiles::Forge::GitHub {
        GITHUB_STORE_LINES
    } else {
        ""
    };
    if cache.is_some() || !tokens.is_empty() {
        let conf = render_conf(header, cache, tokens, true);
        let code = proc::run_with_input(
            ctx,
            state::worker_ssh(&opts_ssh, &login).arg(lease_write("nix.conf", store)),
            format!("{conf}\n").as_bytes(),
        )?;
        crate::ctx::status(code).map_err(|_| Fail(1))?;
    }
    let mut result = shown(verified, "lease", verified);
    if !verified {
        result["cache"] = json!(false);
        result["substituters"] = json!([]);
        result["profiles"] = json!([]);
    }
    Ok(jq::compact(&result))
}

/// Install a niks3 push token on a CI lease (`token` on stdin only).
pub fn install_niks3_token(ctx: &Ctx, name: &str, token: &str, server_url: &str) -> R {
    let opts_ssh = state::ssh_opts(ctx, name);
    if opts_ssh.is_empty() {
        return Err(Fail(1));
    }
    let login = format!("{LEASE_USER}@{name}");
    let url = proc::shell_quote(server_url);
    let remote = format!(
        "umask 077; d=\"$HOME/.config/rworker-niks3\"; mkdir -p \"$d\" && chmod 0700 \"$d\" && printf '%s\\n' {url} > \"$d/server-url\" && cat > \"$d/token.new\" && mv -f \"$d/token.new\" \"$d/token\""
    );
    let code = proc::run_with_input(
        ctx,
        state::worker_ssh(&opts_ssh, &login).arg(remote),
        token.as_bytes(),
    )?;
    crate::ctx::status(code).map_err(|_| Fail(1))
}

/// Ask the controller for a niks3 token for a CI lease and install it. The
/// controller cannot log in to a lease (only the operator's key is
/// authorized there), so the token comes back to this host and goes to the
/// runner over SSH. The result is `{"ok", "expires_at", "server_url"}` or
/// `{"ok": false, "error"}`; the token never leaves this function.
pub fn issue_lease_niks3_token(ctx: &Ctx, name: &str, force: bool) -> Value {
    let mut args = vec!["niks3-token".to_string(), name.to_string()];
    if force {
        args.push("--force".into());
    }
    args.extend(["--owner".to_string(), ctx.owner()]);
    let Ok(raw) = ctx.muted(|| state::remote(ctx, &args)) else {
        return json!({"ok": false, "error": "the controller refused"});
    };
    let mut reply: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let issued = reply["niks3_token"].take();
    drop(raw);
    if issued["ok"] != true {
        return json!({"ok": false, "error": jq::or(&issued["error"], "unknown error")});
    }
    let (Some(token), Some(url)) = (issued["token"].as_str(), issued["server_url"].as_str()) else {
        return json!({"ok": false, "error": "invalid controller reply"});
    };
    match ctx.muted(|| install_niks3_token(ctx, name, token, url)) {
        Ok(()) => json!({"ok": true, "expires_at": issued["expires_at"], "server_url": url}),
        Err(_) => json!({"ok": false, "error": "could not reach the worker over SSH"}),
    }
}

/// Push the settings to a worker. The JSON result, progress on stderr.
pub fn push(ctx: &Ctx, name: &str, dry: bool, opts: &Opts) -> R<String> {
    let cfg = read_config(ctx)?;
    let settings = derive(&cfg);
    let subs = strings(get(&settings, "substituters"));
    let tokens = if opts.auth {
        access_tokens(&cfg, &opts.auth_hosts)
    } else {
        Vec::new()
    };
    let mut do_cache = opts.cache && !subs.is_empty();
    let do_auth = !tokens.is_empty();
    if !do_cache && !do_auth {
        match (opts.cache, opts.auth) {
            (true, false) => ctx.err("This host's Nix has no s3:// substituter; nothing to push"),
            (false, true) => {
                ctx.err("This host's Nix has no matching access-tokens; nothing to push")
            }
            _ => ctx.err(
                "This host's Nix has no s3:// substituter and no access-tokens; nothing to push",
            ),
        }
        return Err(Fail(1));
    }
    let auth_hosts: Vec<String> = tokens.iter().map(|(h, _)| h.clone()).collect();
    // RWORKER_AWS_CREDENTIALS_FILE: for rworker only, without pointing
    // every other AWS tool at that file.
    let creds_file = crate::ctx::env_or(
        "RWORKER_AWS_CREDENTIALS_FILE",
        &crate::ctx::env_or(
            "AWS_SHARED_CREDENTIALS_FILE",
            &format!("{}/.aws/credentials", ctx.home),
        ),
    );
    let mut creds = String::new();
    if do_cache {
        match aws_sections(&creds_file, &strings(get(&settings, "profiles"))) {
            Ok(text) => creds = text,
            Err(profile) => {
                let msg = format!(
                    "No [{profile}] profile in {creds_file} (named by an s3:// substituter)"
                );
                if !do_auth {
                    ctx.err(&msg);
                    return Err(Fail(1));
                }
                // The tokens are worth pushing without the cache.
                ctx.err(&format!("{msg}; pushing the access-tokens only"));
                do_cache = false;
            }
        }
    }
    let header = format!(
        "# Pushed by rworker setup nix from {} at {}.",
        ctx.hostname,
        crate::time::utc_iso(),
    );
    let conf = render_conf(
        &header,
        do_cache.then(|| (subs.as_slice(), strings(get(&settings, "keys")))),
        &tokens,
        false,
    );
    let profiles = get(&settings, "profiles").clone();
    let subs_v = get(&settings, "substituters").clone();
    let shown = |ok: bool, daemon: &str, reachable: bool| {
        json!({
            "ok": ok, "name": name, "daemon": daemon, "reachable": reachable,
            "cache": do_cache, "auth": do_auth,
            "substituters": if do_cache { subs_v.clone() } else { json!([]) },
            "profiles": if do_cache { profiles.clone() } else { json!([]) },
            "auth_hosts": auth_hosts,
        })
    };
    if dry {
        let mut v = shown(true, "dry-run", false);
        v["dry_run"] = json!(true);
        v["keys"] = if do_cache {
            get(&settings, "keys").clone()
        } else {
            json!([])
        };
        return Ok(jq::compact(&v));
    }

    if let Some((forge, _)) = state::hosted_id(name) {
        return push_lease(
            ctx,
            name,
            forge,
            &header,
            do_cache.then(|| (subs.as_slice(), strings(get(&settings, "keys")))),
            &tokens,
            &creds,
            &shown,
        );
    }
    let ip = state::worker_ip(ctx, name)?;
    let opts_ssh = state::ssh_opts(ctx, name);
    let login = format!("agent@{ip}");
    // NetBird may still be setting up the route to a fresh worker.
    for _ in 1..=10 {
        if super::up::probe(&opts_ssh, &login) {
            break;
        }
        proc::sleep_secs(3.0);
    }
    // An image without this feature has no /run/rworker-nix (agent cannot
    // create it); say so instead of failing on a shell error.
    let has_dir = Command::new("ssh")
        .args(&opts_ssh)
        .arg(&login)
        .arg("test -d /run/rworker-nix")
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_dir {
        return Ok(jq::compact(&shown(false, "image-too-old", false)));
    }
    let unit = "rworker-nix-config.service";
    let before = ssh_capture(
        &opts_ssh,
        &login,
        &format!("systemctl show -p ExecMainStartTimestampMonotonic --value {unit}"),
    );
    // Credentials first, nix.conf last: replacing nix.conf is what restarts
    // the daemon (path unit on the worker). Secrets travel on stdin only.
    if do_cache {
        let code = proc::run_with_input(
            ctx,
            state::worker_ssh(&opts_ssh, &login).arg(
                "umask 077; cat > /run/rworker-nix/aws-credentials.new && mv /run/rworker-nix/aws-credentials.new /run/rworker-nix/aws-credentials",
            ),
            creds.as_bytes(),
        )?;
        crate::ctx::status(code).map_err(|_| Fail(1))?;
    }
    let code = proc::run_with_input(
        ctx,
        state::worker_ssh(&opts_ssh, &login).arg(
            "umask 077; cat > /run/rworker-nix/nix.conf.new && mv /run/rworker-nix/nix.conf.new /run/rworker-nix/nix.conf",
        ),
        format!("{conf}\n").as_bytes(),
    )?;
    crate::ctx::status(code).map_err(|_| Fail(1))?;

    // Does the worker reach the bucket with what it was given?
    let verified = if do_cache {
        let url = &subs[0];
        let check = Command::new("ssh")
            .args(&opts_ssh)
            .arg(&login)
            .arg(format!(
                "AWS_SHARED_CREDENTIALS_FILE=/run/rworker-nix/aws-credentials timeout 60 nix store info --store {}",
                proc::shell_quote(url)
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match check {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let lines: Vec<&str> = err.lines().collect();
                let tail = lines[lines.len().saturating_sub(3)..].join("\n");
                ctx.err(&format!("The worker cannot reach the S3 cache: {tail}"));
                false
            }
            Err(_) => false,
        }
    } else {
        true
    };

    // Did its nix-daemon pick the settings up (restarted by the path unit)?
    let mut daemon = "timeout";
    if ssh_capture(
        &opts_ssh,
        &login,
        &format!("systemctl show -p LoadState --value {unit}"),
    ) != "loaded"
    {
        daemon = "image-too-old";
    } else {
        for _ in 1..=20 {
            let after = ssh_capture(
                &opts_ssh,
                &login,
                &format!(
                    "systemctl show -p ExecMainStartTimestampMonotonic --value {unit}; systemctl is-active nix-daemon.socket"
                ),
            )
            .split('\n')
            .collect::<Vec<_>>()
            .join(" ");
            let first = after.split(' ').next().unwrap_or("");
            if first != before && after.ends_with(" active") {
                daemon = "restarted";
                break;
            }
            proc::sleep_secs(1.0);
        }
    }
    Ok(jq::compact(&shown(
        verified && daemon == "restarted",
        daemon,
        verified,
    )))
}

/// `setup nix NAME [--no-cache] [--no-auth] [--auth-host HOST]... [--dry-run]`.
/// `help_topic` is the help to show on a usage error; `defaults` the parts
/// that are on before the flags are read (`setup nix-cache` turns auth off).
pub fn cmd_setup_nix(ctx: &Ctx, args: &[String], help_topic: &str, defaults: Opts) -> R {
    let mut opts = defaults;
    let mut name = String::new();
    let mut dry = false;
    let mut i = 0;
    while i < args.len() && !args[i].is_empty() {
        let a = args[i].as_str();
        match a {
            "--dry-run" | "-n" => dry = true,
            "--no-cache" => opts.cache = false,
            "--no-auth" => opts.auth = false,
            "--auth-host" => {
                let Some(host) = args.get(i + 1).filter(|h| !h.is_empty()) else {
                    ctx.err("--auth-host needs a host");
                    return Err(Fail(2));
                };
                opts.auth_hosts.push(host.clone());
                i += 1;
            }
            s if s.starts_with('-') => {
                ctx.err(&format!("Unknown option: {s}"));
                crate::help::show(ctx, help_topic);
                return Err(Fail(2));
            }
            s => name = s.to_string(),
        }
        i += 1;
    }
    if name.is_empty() {
        crate::help::show(ctx, help_topic);
        return Err(Fail(2));
    }
    if !opts.cache && !opts.auth {
        ctx.err("Nothing to do: both --no-cache and --no-auth are set");
        return Err(Fail(2));
    }
    let out = push(ctx, &name, dry, &opts).map_err(|_| Fail(1))?;
    if ctx.json.get() {
        crate::ctx::json_out(ctx, &out);
        return Ok(());
    }
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let pushes_cache = get(&v, "cache") == &Value::Bool(true);
    let pushes_auth = get(&v, "auth") == &Value::Bool(true);
    if dry {
        outln(&format!("Would push to {}:", paint::vm(ctx, &name)));
        for s in strings(get(&v, "substituters")) {
            outln(&format!("  substituter {s}"));
        }
        for p in strings(get(&v, "profiles")) {
            outln(&format!("  profile     {p}"));
        }
        if pushes_cache {
            let n = get(&v, "keys").as_array().map_or(0, Vec::len);
            outln(&format!("  trusted keys {n}"));
        }
        for h in strings(get(&v, "auth_hosts")) {
            outln(&format!("  access-token for {h}"));
        }
        return Ok(());
    }
    let what = match (pushes_cache, pushes_auth) {
        (true, true) => "substitutes from the S3 cache and fetches private inputs",
        (true, false) => "substitutes from the S3 cache",
        _ => "fetches private inputs",
    };
    match get(&v, "daemon").as_str() {
        Some("restarted") => paint::ok(ctx, &format!("{} {what}", paint::vm(ctx, &name))),
        Some("lease") if get(&v, "ok") == &Value::Bool(true) => {
            paint::ok(ctx, &format!("{} {what}", paint::vm(ctx, &name)))
        }
        Some("lease") => {
            let pushed = if pushes_auth {
                "only the access-tokens were set up"
            } else {
                "nothing was set up"
            };
            ctx.err(&format!(
                "{} the S3 cache does not work on {}; {pushed}",
                paint::warn_mark(ctx),
                paint::vm(ctx, &name)
            ));
            return Err(Fail(1));
        }
        Some("image-too-old") => {
            ctx.err(&format!(
                "{} {} runs an image from before `setup nix` existed: nothing was pushed (create a new worker)",
                paint::warn_mark(ctx),
                paint::vm(ctx, &name)
            ));
            return Err(Fail(1));
        }
        _ => ctx.err(&format!(
            "{} pushed to {}, but its nix-daemon did not restart in time",
            paint::warn_mark(ctx),
            paint::vm(ctx, &name)
        )),
    }
    Ok(())
}

/// `setup nix-cache NAME [--dry-run]`: the cache part only.
pub fn cmd_nix_cache(ctx: &Ctx, args: &[String]) -> R {
    cmd_setup_nix(
        ctx,
        args,
        "setup",
        Opts {
            auth: false,
            ..Opts::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Value {
        json!({
            "access-tokens": {"value": {
                "github.com": "ghp_secret",
                "git.wiit.one": "PAT:glpat_secret",
                "bad.example": "has space",
                "hash.example": "tok#en",
            }},
            "substituters": {"value": ["https://cache.nixos.org", "s3://b?profile=niks3"]},
            "trusted-public-keys": {"value": ["k1"]},
        })
    }

    #[test]
    fn tokens_are_sorted_and_unsafe_entries_dropped() {
        let t = access_tokens(&cfg(), &[]);
        let hosts: Vec<&str> = t.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hosts, ["git.wiit.one", "github.com"]);
    }

    #[test]
    fn tokens_can_be_limited_to_hosts() {
        let t = access_tokens(&cfg(), &["github.com".to_string(), "nope".to_string()]);
        assert_eq!(t, [("github.com".to_string(), "ghp_secret".to_string())]);
    }

    #[test]
    fn no_tokens_without_the_setting() {
        assert!(access_tokens(&json!({}), &[]).is_empty());
    }

    #[test]
    fn derived_settings_name_hosts_but_never_carry_a_token() {
        let d = derive(&cfg());
        assert_eq!(d["auth_hosts"], json!(["git.wiit.one", "github.com"]));
        assert_eq!(d["substituters"], json!(["s3://b?profile=niks3"]));
        assert_eq!(d["profiles"], json!(["niks3"]));
        let text = d.to_string();
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn aws_sections_return_the_named_profiles_or_the_missing_one() {
        let dir = std::env::temp_dir().join(format!("rworker-aws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("credentials");
        std::fs::write(
            &file,
            "[other]\nk = 1\n[niks3]\naws_access_key_id = a\naws_secret_access_key = b\n",
        )
        .unwrap();
        let f = file.to_str().unwrap();
        let ok = aws_sections(f, &["niks3".to_string()]).unwrap();
        assert!(ok.starts_with("[niks3]\n") && !ok.contains("other"), "{ok}");
        assert_eq!(
            aws_sections(f, &["niks3".to_string(), "gone".to_string()]),
            Err("gone".to_string())
        );
        assert_eq!(
            aws_sections("/nonexistent/credentials", &["niks3".to_string()]),
            Err("niks3".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lease_replaces_the_runners_access_tokens_a_worker_extends_its_own() {
        let tokens = [("github.com".to_string(), "ghp_secret".to_string())];
        let subs = ["s3://b?profile=niks3".to_string()];
        let cache = || Some((subs.as_slice(), vec!["k1".to_string(), "k2".to_string()]));
        let lease = render_conf("# h", cache(), &tokens, true);
        assert_eq!(
            lease,
            "# h\nextra-substituters = s3://b?profile=niks3\nextra-trusted-public-keys = k1 k2\naccess-tokens = github.com=ghp_secret"
        );
        let worker = render_conf("# h", None, &tokens, false);
        assert_eq!(worker, "# h\nextra-access-tokens = github.com=ghp_secret");
        assert_eq!(render_conf("# h", None, &[], true), "# h");
    }

    #[test]
    fn lease_files_are_written_privately_from_stdin() {
        let script = lease_write("nix.conf", GITHUB_STORE_LINES);
        assert!(script.starts_with("umask 077;"), "{script}");
        assert!(script.contains("chmod 0700 \"$d\""), "{script}");
        assert!(script.contains("{ cat; printf "), "{script}");
        assert!(
            script.contains("store = %s/.local/share/nix/rworker"),
            "{script}"
        );
        assert!(script.ends_with("mv -f \"$d/nix.conf.new\" \"$d/nix.conf\""));
        assert!(lease_write("aws-credentials", "").contains("{ cat;  }"));
    }

    #[test]
    fn both_parts_are_on_by_default() {
        let o = Opts::default();
        assert!(o.cache && o.auth && o.auth_hosts.is_empty());
    }
}
