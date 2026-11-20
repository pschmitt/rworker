use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

pub(super) const CREATOR_KEYS: [&str; 6] = [
    "agent",
    "agent_id",
    "agent_pid",
    "agent_session",
    "topic",
    "cwd",
];

pub(super) fn clean(value: &str) -> String {
    // Python str.isprintable excludes Unicode Other and Separator,
    // except for an ordinary ASCII space.
    static NONPRINTABLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"[[\p{C}\p{Z}]&&[^ ]]").expect("valid Unicode category expression")
    });
    NONPRINTABLE
        .replace_all(value, "")
        .trim()
        .chars()
        .take(255)
        .collect()
}

pub(super) fn creator_metadata(pairs: &[String]) -> Result<Map<String, Value>, String> {
    let mut metadata = Map::new();
    for pair in pairs {
        let Some((key, value)) = pair
            .split_once('=')
            .filter(|(key, _)| CREATOR_KEYS.contains(key))
        else {
            return Err(format!(
                "--meta: expected one of {} as KEY=VALUE, got {pair:?}",
                CREATOR_KEYS.join(", ")
            ));
        };
        let value = clean(value);
        if !value.is_empty() {
            metadata.insert(key.into(), Value::String(value));
        }
    }
    Ok(metadata)
}

pub(super) fn stale_creator_keys(
    existing: &Map<String, Value>,
    replacement: &Map<String, Value>,
) -> Vec<&'static str> {
    CREATOR_KEYS
        .into_iter()
        .filter(|&key| existing.contains_key(key) && !replacement.contains_key(key))
        .collect()
}

/// Apply sets before removals. Empty collections need no provider call.
pub(super) struct Change {
    pub set: Map<String, Value>,
    pub remove: Vec<&'static str>,
    pub response: Value,
}

pub(super) fn topic(name: &str, text: &str) -> Change {
    let text = clean(text);
    let mut set = Map::new();
    let mut remove = Vec::new();
    let value = if text.is_empty() {
        remove.push("topic");
        Value::Null
    } else {
        Value::String(text)
    };
    if !value.is_null() {
        set.insert("topic".into(), value.clone());
    }
    Change {
        set,
        remove,
        response: json!({"name": name, "topic": value}),
    }
}

pub(super) fn claim(
    name: &str,
    existing: &Map<String, Value>,
    pairs: &[String],
) -> Result<Change, String> {
    let set = creator_metadata(pairs)?;
    let remove = stale_creator_keys(existing, &set);
    let mut response = Map::new();
    response.insert("name".into(), json!(name));
    for key in CREATOR_KEYS {
        response.insert(key.into(), set.get(key).cloned().unwrap_or(Value::Null));
    }
    Ok(Change {
        set,
        remove,
        response: Value::Object(response),
    })
}
