//! Fields and CLI arguments for the worker creation form.

use super::app::split_words;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Topic,
    Profile,
    Lifetime,
    Flavor,
    Disk,
    AfterCreate,
    ExtraFlags,
}

use Field::{AfterCreate, Disk, ExtraFlags, Flavor, Lifetime, Profile, Topic};

const FIELDS: [Field; 7] = [
    Topic,
    Profile,
    Lifetime,
    Flavor,
    Disk,
    AfterCreate,
    ExtraFlags,
];

impl Field {
    pub(super) fn label(self) -> &'static str {
        match self {
            Topic => "Topic",
            Profile => "Profile",
            Lifetime => "Lifetime",
            Flavor => "Flavor",
            Disk => "Disk (GiB)",
            AfterCreate => "After create",
            ExtraFlags => "Extra flags",
        }
    }
}

#[derive(Default)]
pub(super) struct Edit {
    pub(super) text: String,
    pub(super) cursor: usize,
}

pub(super) fn edit(text: &mut String, cursor: &mut usize, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let byte = |s: &str, c: usize| s.char_indices().nth(c).map_or(s.len(), |(i, _)| i);
    let at = byte(text, *cursor);
    match key.code {
        KeyCode::Char('u') if ctrl => {
            text.replace_range(..at, "");
            *cursor = 0;
        }
        KeyCode::Char('w') if ctrl => {
            let head = text[..at].trim_end();
            let cut = head.rfind(' ').map_or(0, |i| i + 1);
            text.replace_range(cut..at, "");
            *cursor = text[..cut].chars().count();
        }
        KeyCode::Char(c) if !ctrl => {
            text.insert(at, c);
            *cursor += 1;
        }
        KeyCode::Backspace if *cursor > 0 => {
            text.remove(byte(text, *cursor - 1));
            *cursor -= 1;
        }
        KeyCode::Delete if at < text.len() => {
            text.remove(at);
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(text.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = text.chars().count(),
        KeyCode::Char('a') if ctrl => *cursor = 0,
        KeyCode::Char('e') if ctrl => *cursor = text.chars().count(),
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum After {
    Create,
    Ssh,
    SshAndDelete,
}

const MODES: [(After, &str); 3] = [
    (After::Create, "Create only"),
    (After::Ssh, "Open SSH"),
    (After::SshAndDelete, "Open SSH and delete on exit"),
];

pub(super) struct CreateWorker {
    pub(super) request: u64,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    pub(super) focus: Field,
    profiles: Vec<Value>,
    profile: usize,
    mode: usize,
    topic: Edit,
    lifetime: Edit,
    flavor: Edit,
    disk: Edit,
    extra: Edit,
}

impl CreateWorker {
    pub(super) fn new(request: u64) -> Self {
        Self {
            request,
            loading: true,
            error: None,
            focus: Topic,
            profiles: Vec::new(),
            profile: 0,
            mode: 0,
            topic: Edit::default(),
            lifetime: Edit::default(),
            flavor: Edit::default(),
            disk: Edit::default(),
            extra: Edit::default(),
        }
    }

    pub(super) fn load(&mut self, result: Result<Vec<Value>, String>) {
        self.loading = false;
        match result {
            Ok(rows) if !rows.is_empty() => self.profiles = rows,
            _ => self.error = Some("Profiles unavailable. You can still use the default.".into()),
        }
    }

    pub(super) fn fields(&self) -> impl Iterator<Item = Field> {
        FIELDS
            .into_iter()
            .filter(|field| !self.github() || !matches!(field, Flavor | Disk))
    }

    fn profile(&self) -> Option<&Value> {
        self.profiles.get(self.profile)
    }

    fn github(&self) -> bool {
        self.profile()
            .is_some_and(|profile| profile["provider"] == "github")
    }

    pub(super) fn profile_facts(&self) -> String {
        let Some(profile) = self.profile() else {
            return if self.loading {
                "Loading profiles…".into()
            } else {
                String::new()
            };
        };
        let mut facts = vec![if self.github() {
            "GitHub Actions".into()
        } else {
            "OpenStack".into()
        }];
        if let Some(cpus) = profile["vcpus"].as_u64() {
            facts.push(format!("{cpus} vCPU"));
        }
        if let Some(gb) = profile["ram_gb"].as_u64() {
            facts.push(format!("{gb} GB RAM"));
        } else if let Some(mib) = profile["ram_mib"].as_u64() {
            facts.push(if mib % 1024 == 0 {
                format!("{} GiB RAM", mib / 1024)
            } else {
                format!("{mib} MiB RAM")
            });
        }
        if let Some(bytes) = profile["disk_total_bytes"].as_u64() {
            facts.push(format!(
                "{:.1} GiB disk",
                bytes as f64 / (1_u64 << 30) as f64
            ));
        } else if let Some(size) = profile["disk"].as_u64() {
            facts.push(format!("{size} GiB disk"));
        }
        facts.join(" · ")
    }

    pub(super) fn choice(&self, field: Field, icons: bool) -> String {
        if field == AfterCreate {
            return format!("‹ {} ›", MODES[self.mode].1);
        }
        let Some(profile) = self.profile() else {
            return "Controller default".into();
        };
        let name = profile["name"].as_str().unwrap_or("Controller default");
        let default = if profile["default"] == true {
            " (default)"
        } else {
            ""
        };
        let icon = if icons {
            format!("{} ", crate::pool_icon::profile_glyph(profile))
        } else {
            String::new()
        };
        format!(
            "‹ {icon}{name}{default} ›   {}/{}",
            self.profile + 1,
            self.profiles.len()
        )
    }

    pub(super) fn input(&self, field: Field) -> Option<(&Edit, &str)> {
        match field {
            Topic => Some((&self.topic, "Optional")),
            Lifetime => Some((
                &self.lifetime,
                if self.github() {
                    "30m default"
                } else {
                    "Profile default, e.g. 2h"
                },
            )),
            Flavor => Some((
                &self.flavor,
                self.profile()
                    .and_then(|profile| profile["flavor"].as_str())
                    .unwrap_or("Profile default"),
            )),
            Disk => Some((&self.disk, "Profile default")),
            ExtraFlags => Some((&self.extra, "Optional, e.g. --no-nix-auth")),
            _ => None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) {
        let cycle = |index: usize, len: usize, forward| {
            if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            }
        };
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                let fields: Vec<_> = self.fields().collect();
                let index = fields
                    .iter()
                    .position(|field| *field == self.focus)
                    .unwrap_or(0);
                self.focus = fields[cycle(
                    index,
                    fields.len(),
                    matches!(key.code, KeyCode::Tab | KeyCode::Down),
                )];
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focus == Profile || self.focus == AfterCreate =>
            {
                let forward = key.code != KeyCode::Left;
                if self.focus == AfterCreate {
                    self.mode = cycle(self.mode, MODES.len(), forward);
                } else if !self.profiles.is_empty() {
                    self.profile = cycle(self.profile, self.profiles.len(), forward);
                }
            }
            _ => {
                let input = match self.focus {
                    Topic => &mut self.topic,
                    Lifetime => &mut self.lifetime,
                    Flavor => &mut self.flavor,
                    Disk => &mut self.disk,
                    ExtraFlags => &mut self.extra,
                    _ => return,
                };
                edit(&mut input.text, &mut input.cursor, key);
            }
        }
    }

    pub(super) fn submission(&self) -> Result<(Vec<String>, After), String> {
        let extra = split_words(self.extra.text.trim())?;
        let mut after = MODES[self.mode].0;
        if after == After::Create {
            let mut flags = extra.iter();
            while let Some(flag) = flags.next() {
                match flag.as_str() {
                    "--" => break,
                    "--ssh" => {
                        after = After::Ssh;
                        break;
                    }
                    _ if crate::help::flags("up")
                        .any(|(name, takes_value)| name == flag && takes_value) =>
                    {
                        flags.next();
                    }
                    _ => {}
                }
            }
        }
        let mut args = vec!["up".into()];
        args.extend(extra);
        let disk = if self.github() {
            ""
        } else {
            self.disk.text.trim()
        };
        if !disk.is_empty() && !disk.parse::<u64>().is_ok_and(|size| size > 0) {
            return Err("Disk must be a positive whole number of GiB.".into());
        }
        let flavor = if self.github() {
            ""
        } else {
            self.flavor.text.trim()
        };
        for (flag, value) in [
            ("--topic", self.topic.text.trim()),
            (
                "--profile",
                self.profile()
                    .and_then(|profile| profile["name"].as_str())
                    .unwrap_or(""),
            ),
            ("--ttl", self.lifetime.text.trim()),
            ("--flavor", flavor),
            ("--disk", disk),
        ] {
            if !value.is_empty() {
                args.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        args.push("--quiet-topic-warning".into());
        Ok((args, after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_ssh_cleanup_keeps_topic_and_omits_openstack_overrides() {
        let mut form = CreateWorker::new(1);
        form.load(Ok(vec![
            serde_json::json!({"name": "standard", "default": true}),
            serde_json::json!({"name": "gh-aarch64", "provider": "github"}),
        ]));
        form.topic.text = "build the ARM image".into();
        form.flavor.text = "large".into();
        form.disk.text = "250".into();
        form.focus = Profile;
        form.key(KeyEvent::from(KeyCode::Right));
        assert!(!form.fields().any(|field| field == Flavor || field == Disk));
        form.focus = AfterCreate;
        form.key(KeyEvent::from(KeyCode::Left));
        let (args, after) = form.submission().unwrap();
        assert_eq!(after, After::SshAndDelete);
        assert_eq!(
            args,
            [
                "up",
                "--topic",
                "build the ARM image",
                "--profile",
                "gh-aarch64",
                "--quiet-topic-warning"
            ]
        );
    }
}
