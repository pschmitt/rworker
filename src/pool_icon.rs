//! The Nerd Font glyph shown for a worker: the `icon` its Glance image
//! carries (`rworker_icon`, set when the image is published), else one
//! derived from its kind, else a generic server. Human-readable output only:
//! JSON keeps the pool name and label, never the glyph.

use serde_json::Value;

/// nf-fa-github: GitHub Actions workers.
const GITHUB: &str = "\u{f09b}";

/// nf-md-tools: ad-hoc workers on an image without an icon.
const ADHOC: &str = "\u{f1064}";
/// nf-md-robot: nixbot's pool on an image without an icon.
const NIXBOT: &str = "\u{f06a9}";
/// nf-md-server: any other worker without an image icon.
const GENERIC: &str = "\u{f048b}";

pub fn glyph(worker: &Value) -> &str {
    if worker["name"]
        .as_str()
        .is_some_and(|name| name.starts_with("gha-"))
    {
        return GITHUB;
    }
    if let Some(icon) = worker["icon"].as_str().filter(|icon| !icon.is_empty()) {
        return icon;
    }
    match worker["kind"].as_str() {
        Some("adhoc") => ADHOC,
        Some("nixbot" | "pool") => NIXBOT,
        _ => GENERIC,
    }
}

/// `ls --json` and scripts: the inventory without the console-only glyph.
pub fn strip(list: &mut Value) {
    if let Some(items) = list.as_array_mut() {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                object.remove("icon");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn image_icon_wins_then_kind_then_generic() {
        assert_eq!(
            glyph(&json!({"icon": "\u{f0508}", "kind": "adhoc"})),
            "\u{f0508}"
        );
        assert_eq!(glyph(&json!({"icon": "", "kind": "adhoc"})), ADHOC);
        assert_eq!(glyph(&json!({"kind": "nixbot"})), NIXBOT);
        assert_eq!(glyph(&json!({"kind": "circus"})), GENERIC);
    }

    #[test]
    fn github_workers_use_the_github_glyph() {
        assert_eq!(
            glyph(&json!({"name": "gha-example", "kind": "adhoc"})),
            GITHUB
        );
    }

    #[test]
    fn json_never_carries_the_glyph() {
        let mut list = json!([{"name": "a", "icon": "x", "pool": "circus", "label": "Circus"}]);
        strip(&mut list);
        assert_eq!(
            list,
            json!([{"name": "a", "pool": "circus", "label": "Circus"}])
        );
    }
}
