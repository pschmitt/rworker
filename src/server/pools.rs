//! Which workers are ours and which pool each belongs to, read from the
//! OpenStack metadata the creator sets (`nixbot-worker`, `rworker-pool`).
//! Name prefixes are only a fallback for workers created before that
//! metadata existed.

use serde_json::{Map, Value};

use super::openstack::setting;

/// Metadata key naming the pool of an autoscaled worker.
pub(super) const POOL_KEY: &str = "rworker-pool";
/// Metadata key every rworker-created server carries (`adhoc` for ad-hoc
/// workers, the pool's label for autoscaled ones).
pub(super) const KIND_KEY: &str = "nixbot-worker";
/// Pool name of nixbot's autoscaled pool, the historic default.
pub(super) const NIXBOT_POOL: &str = "nixbot";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Adhoc,
    Pool(String),
}

impl Kind {
    /// The `kind` shown by `ls` and the consoles: `adhoc` or the pool name.
    pub fn label(&self) -> &str {
        match self {
            Self::Adhoc => "adhoc",
            Self::Pool(name) => name,
        }
    }
}

pub(super) struct Pools<'a> {
    adhoc_prefix: &'a str,
    /// Name prefix -> pool, for workers without pool metadata.
    legacy: Vec<(&'a str, String)>,
}

impl<'a> Pools<'a> {
    /// `adhocPrefix`, `poolPrefix` (nixbot's pool) and the optional
    /// `poolPrefixes` map (prefix -> pool name) of the settings.
    pub fn from_settings(cfg: &'a Value) -> Result<Self, String> {
        let mut legacy = Vec::new();
        if let Some(prefix) = cfg["poolPrefix"].as_str().filter(|p| !p.is_empty()) {
            legacy.push((prefix, NIXBOT_POOL.to_owned()));
        }
        if let Some(map) = cfg["poolPrefixes"].as_object() {
            for (prefix, pool) in map {
                let pool = pool
                    .as_str()
                    .ok_or_else(|| format!("Invalid poolPrefixes entry: {prefix}"))?;
                if !prefix.is_empty() {
                    legacy.push((prefix.as_str(), pool.to_owned()));
                }
            }
        }
        Ok(Self {
            adhoc_prefix: setting(cfg, "adhocPrefix")?,
            legacy,
        })
    }

    /// Classify a server; `None` when it is not an rworker worker.
    pub fn classify(&self, name: &str, metadata: &Map<String, Value>) -> Option<Kind> {
        if let Some(kind) = from_metadata(metadata) {
            return Some(kind);
        }
        if !self.adhoc_prefix.is_empty() && name.starts_with(self.adhoc_prefix) {
            return Some(Kind::Adhoc);
        }
        self.legacy
            .iter()
            .filter(|(prefix, _)| name.starts_with(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, pool)| Kind::Pool(pool.clone()))
    }
}

fn from_metadata(metadata: &Map<String, Value>) -> Option<Kind> {
    let kind = metadata.get(KIND_KEY).and_then(Value::as_str);
    if kind == Some("adhoc") {
        return Some(Kind::Adhoc);
    }
    if let Some(pool) = metadata
        .get(POOL_KEY)
        .and_then(Value::as_str)
        .filter(|pool| !pool.is_empty())
    {
        return Some(Kind::Pool(pool.to_owned()));
    }
    // Before `rworker-pool`: nixbot's pool was labelled `1` or `pool`,
    // other pools `<name>-pool`.
    match kind? {
        "" => None,
        "1" | "pool" => Some(Kind::Pool(NIXBOT_POOL.to_owned())),
        other => Some(Kind::Pool(
            other.strip_suffix("-pool").unwrap_or(other).to_owned(),
        )),
    }
}

/// The pool an autoscaler manages: `poolName`, else derived from
/// `poolLabels.kind` (`circus-pool` -> `circus`), else nixbot's.
pub(super) fn own_pool(cfg: &Value) -> String {
    if let Some(name) = cfg["poolName"].as_str().filter(|n| !n.is_empty()) {
        return name.to_owned();
    }
    match cfg["poolLabels"]["kind"].as_str() {
        None | Some("" | "1" | "pool") => NIXBOT_POOL.to_owned(),
        Some(kind) => kind.strip_suffix("-pool").unwrap_or(kind).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect()
    }

    fn settings() -> Value {
        json!({
            "adhocPrefix": "adhoc-",
            "poolPrefix": "nixbot-worker-",
            "poolPrefixes": {"circus-worker-": "circus"},
        })
    }

    #[test]
    fn metadata_wins_over_the_name() {
        let cfg = settings();
        let pools = Pools::from_settings(&cfg).unwrap();
        // A pool worker never becomes ad-hoc because of its name.
        assert_eq!(
            pools.classify(
                "adhoc-x",
                &meta(&[("nixbot-worker", "pool"), ("rworker-pool", "circus")])
            ),
            Some(Kind::Pool("circus".into()))
        );
        assert_eq!(
            pools.classify("whatever", &meta(&[("nixbot-worker", "adhoc")])),
            Some(Kind::Adhoc)
        );
    }

    #[test]
    fn legacy_labels_map_to_pools() {
        let cfg = settings();
        let pools = Pools::from_settings(&cfg).unwrap();
        for label in ["1", "pool"] {
            assert_eq!(
                pools.classify("nixbot-worker-1", &meta(&[("nixbot-worker", label)])),
                Some(Kind::Pool("nixbot".into()))
            );
        }
        assert_eq!(
            pools.classify(
                "circus-worker-1",
                &meta(&[("nixbot-worker", "circus-pool")])
            ),
            Some(Kind::Pool("circus".into()))
        );
    }

    #[test]
    fn name_prefixes_are_only_a_fallback() {
        let cfg = settings();
        let pools = Pools::from_settings(&cfg).unwrap();
        let none = Map::new();
        assert_eq!(pools.classify("adhoc-1", &none), Some(Kind::Adhoc));
        assert_eq!(
            pools.classify("nixbot-worker-1", &none),
            Some(Kind::Pool("nixbot".into()))
        );
        assert_eq!(
            pools.classify("circus-worker-1", &none),
            Some(Kind::Pool("circus".into()))
        );
        assert_eq!(pools.classify("rofl-15", &none), None);
    }

    #[test]
    fn own_pool_from_settings() {
        assert_eq!(own_pool(&json!({})), "nixbot");
        assert_eq!(own_pool(&json!({"poolLabels": {"kind": "pool"}})), "nixbot");
        assert_eq!(
            own_pool(&json!({"poolLabels": {"kind": "circus-pool"}})),
            "circus"
        );
        assert_eq!(
            own_pool(&json!({"poolName": "gpu", "poolLabels": {"kind": "circus-pool"}})),
            "gpu"
        );
    }
}
