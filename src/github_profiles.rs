/// A standard GitHub-hosted Linux runner and its rworker profile.
pub(crate) struct Profile {
    pub name: &'static str,
    pub runner: &'static str,
    pub label: &'static str,
    pub system: &'static str,
}

pub(crate) const PROFILES: [Profile; 2] = [
    Profile {
        name: "gh-aarch64",
        runner: "linux-arm64",
        label: "ubuntu-24.04-arm",
        system: "aarch64-linux",
    },
    Profile {
        name: "gh-x86_64",
        runner: "linux-x64",
        label: "ubuntu-24.04",
        system: "x86_64-linux",
    },
];

pub(crate) fn from_name(name: &str) -> Option<&'static Profile> {
    let name = match name {
        "github-arm64" => "gh-aarch64",
        "github-x64" | "gh" => "gh-x86_64",
        name => name,
    };
    PROFILES.iter().find(|profile| profile.name == name)
}

pub(crate) fn from_runner(runner: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.runner == runner)
}
