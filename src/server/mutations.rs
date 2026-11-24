use serde_json::{Map, Value};

use super::{metadata, pools::Pools, selection::Target};

pub(super) type WorkerRecord<Id> = (Id, Map<String, Value>);

pub(super) trait Compute {
    type WorkerId;

    /// Fetch the server with exactly this name; callers classify it with
    /// `pools::Pools` before acting on it.
    fn worker(&mut self, name: &str) -> Result<Option<WorkerRecord<Self::WorkerId>>, String>;

    fn set_metadata(
        &mut self,
        worker: &Self::WorkerId,
        metadata: Map<String, Value>,
    ) -> Result<(), String>;

    fn delete_metadata(&mut self, worker: &Self::WorkerId, keys: &[&str]) -> Result<(), String>;
}

pub(super) enum Edit<'a> {
    Topic(&'a str),
    Claim(&'a [String]),
}

pub(super) fn edit(
    cloud: &mut impl Compute,
    pools: &Pools<'_>,
    name: &str,
    owner: Option<&str>,
    force: bool,
    action: Edit<'_>,
) -> Result<Value, String> {
    let (id, existing) = cloud
        .worker(name)?
        .ok_or_else(|| format!("no worker {name}"))?;
    let target = Target::new(name, pools.classify(name, &existing), false)?;
    target.check(existing.get("owner").and_then(Value::as_str), owner, force)?;
    let change = match action {
        Edit::Topic(text) => metadata::topic(name, text),
        Edit::Claim(pairs) => metadata::claim(name, &existing, pairs)?,
    };
    if !change.set.is_empty() {
        cloud.set_metadata(&id, change.set)?;
    }
    if !change.remove.is_empty() {
        cloud.delete_metadata(&id, &change.remove)?;
    }
    Ok(change.response)
}

/// Compute the metadata update before attempting token renewal.
pub(super) fn extension(
    name: &str,
    metadata: &Map<String, Value>,
    now: f64,
    ttl_seconds: u64,
    default_max_ttl: f64,
    no_token: bool,
) -> Result<Extension, String> {
    let number = |key, default| {
        metadata.get(key).map_or(Ok(default), |value| {
            value
                .as_f64()
                .or_else(|| value.as_str()?.parse().ok())
                .filter(|value| value.is_finite())
                .ok_or_else(|| format!("Invalid worker metadata: {key}"))
        })
    };
    let end = number("created_at", now)? + number("max_lifetime", default_max_ttl)?;
    let expires_at = (now + ttl_seconds as f64).min(end).trunc() as i64;
    let renew_token = !no_token
        && metadata
            .get("niks3_token_exp")
            .is_some_and(crate::jq::truthy);
    Ok(Extension {
        expires_at,
        renew_token,
        response: serde_json::json!({
            "name": name,
            "expires_at": expires_at,
            "max_lifetime_end": end.trunc() as i64,
        }),
    })
}

pub(super) struct Extension {
    pub expires_at: i64,
    pub renew_token: bool,
    pub response: Value,
}

pub(super) fn expiry(
    settings: &Value,
    cloud: &mut impl Compute<WorkerId = String>,
    pools: &Pools<'_>,
    args: &super::EditArgs<'_>,
) -> Result<Value, String> {
    let name = args.name;
    let (id, metadata) = cloud
        .worker(name)?
        .ok_or_else(|| format!("no worker {name}"))?;
    let target = Target::new(name, pools.classify(name, &metadata), false)?;
    target.check(
        metadata.get("owner").and_then(Value::as_str),
        args.owner,
        args.force,
    )?;
    let now = super::epoch();
    let max_ttl = settings["adhocMaxTtl"]
        .as_f64()
        .ok_or("Missing setting: adhocMaxTtl")?;
    if let Some(ttl) = args.ttl {
        let mut result = extension(name, &metadata, now, ttl, max_ttl, args.no_token)?;
        cloud.set_metadata(
            &id,
            [(
                "expires_at".into(),
                Value::String(result.expires_at.to_string()),
            )]
            .into_iter()
            .collect(),
        )?;
        if result.renew_token {
            result.response["niks3_token"] =
                super::niks3::install(settings, cloud, &id, name, &metadata, result.expires_at)?;
        }
        return Ok(result.response);
    }

    if !settings["niks3"]
        .as_object()
        .is_some_and(|cfg| !cfg.is_empty())
    {
        return Err("niks3 tokens are not configured".into());
    }
    let number = |key| {
        metadata
            .get(key)
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    };
    let expires = number("expires_at").unwrap_or_default();
    let last = number("last_active").unwrap_or_default();
    let expires = if expires != 0.0 && last >= expires {
        let lifetime =
            number("created_at").unwrap_or(now) + number("max_lifetime").unwrap_or(max_ttl);
        let grace = settings["adhocGracePeriod"]
            .as_f64()
            .ok_or("Missing setting: adhocGracePeriod")?;
        expires.max((last + grace).min(lifetime))
    } else {
        expires
    };
    if expires <= now {
        return Err(format!("{name} has expired"));
    }
    let token = super::niks3::install(settings, cloud, &id, name, &metadata, expires as i64)?;
    Ok(serde_json::json!({"name": name, "niks3_token": token}))
}

pub(super) fn down(
    settings: &Value,
    compute: &mut super::openstack::OpenStack,
    pools: &Pools<'_>,
    args: &super::EditArgs<'_>,
) -> Result<(), String> {
    let name = args.name;
    let netbird = super::netbird::NetBird::from_settings(settings);
    let (id, metadata) = compute
        .worker(name)?
        .ok_or_else(|| format!("no worker {name}"))?;
    let target = Target::new(name, pools.classify(name, &metadata), true)?;
    target.check(
        metadata.get("owner").and_then(Value::as_str),
        args.owner,
        args.force,
    )?;

    let by = args.owner.unwrap_or("operator");
    let reason = if args.force { "down --force" } else { "down" };
    delete(
        settings,
        compute,
        &netbird,
        &id,
        name,
        &metadata,
        (by, reason),
    )
}

/// Delete a worker. `why` (who, reason) becomes the details of its
/// `worker-deleted` event.
pub(super) fn delete(
    settings: &Value,
    compute: &super::openstack::OpenStack,
    netbird: &super::netbird::NetBird,
    id: &str,
    name: &str,
    metadata: &Map<String, Value>,
    why: (&str, &str),
) -> Result<(), String> {
    if let Some(key) = metadata
        .get("setup_key_id")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    {
        netbird.delete_key(key);
    }
    if settings["tailscale"].is_object() {
        let tidy = super::tailscale::Tailscale::from_settings(&settings["tailscale"]).and_then(
            |mut client| {
                if let Some(key) = metadata
                    .get("ts_key_id")
                    .and_then(Value::as_str)
                    .filter(|key| !key.is_empty())
                {
                    client.delete_key(key);
                }
                client.delete_device(name)
            },
        );
        if let Err(error) = tidy {
            eprintln!("removing {name} from Tailscale: {error}");
        }
    }
    compute.command(&["server", "delete", "--", id])?;
    crate::event_history::note_deletion(name, why.0, why.1);
    // The server is gone; an ephemeral peer that NetBird cannot remove now
    // expires on its own, so do not fail the deletion over it.
    if let Err(error) = netbird.delete_peer(name) {
        eprintln!("removing {name} from NetBird: {error}");
    }
    Ok(())
}
