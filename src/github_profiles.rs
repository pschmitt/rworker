/// A standard GitHub-hosted Linux runner and its rworker profile.
pub(crate) struct Profile {
    pub forge: Forge,
    pub name: &'static str,
    pub runner: &'static str,
    pub label: &'static str,
    pub system: &'static str,
}

pub(crate) const PROFILES: [Profile; 2] = [
    Profile {
        forge: Forge::GitHub,
        name: "gh-aarch64",
        runner: "linux-arm64",
        label: "ubuntu-24.04-arm",
        system: "aarch64-linux",
    },
    Profile {
        forge: Forge::GitHub,
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
        "gl" => "gl-x86_64",
        name => name,
    };
    PROFILES
        .iter()
        .chain(std::iter::once(&GITLAB_PROFILE))
        .find(|profile| profile.name == name)
}

pub(crate) const GITLAB_PROFILE: Profile = Profile {
    forge: Forge::GitLab,
    name: "gl-x86_64",
    runner: "gl-linux-x64",
    label: "GitLab Linux x86-64",
    system: "x86_64-linux",
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forge {
    GitHub,
    GitLab,
}

impl Forge {
    pub(crate) const ALL: [Self; 2] = [Self::GitHub, Self::GitLab];

    pub(crate) fn from_key(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|forge| forge.key() == value)
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub Actions",
            Self::GitLab => "GitLab CI",
        }
    }

    pub(crate) fn tags(self) -> &'static [&'static str] {
        match self {
            Self::GitHub => &["github-actions"],
            Self::GitLab => &["gitlab-ci", "gitlab"],
        }
    }

    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::GitHub => "gha-",
            Self::GitLab => "glci-",
        }
    }

    pub(crate) fn settings_key(self) -> &'static str {
        match self {
            Self::GitHub => "githubActions",
            Self::GitLab => "gitlabCi",
        }
    }

    pub(crate) fn profiles(self) -> &'static [Profile] {
        match self {
            Self::GitHub => &PROFILES,
            Self::GitLab => std::slice::from_ref(&GITLAB_PROFILE),
        }
    }
}
