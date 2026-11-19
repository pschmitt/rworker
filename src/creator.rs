//! Who creates or claims a worker: the agent process above us, its PID
//! and session, the agent id, the topic and the working directory, as
//! `--meta key=value` arguments for the control host.

use crate::ctx::env;
use std::fs;
use std::os::unix::fs::MetadataExt;

const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "agy",
    "antigravity",
    "opencode",
    "gemini",
    "copilot",
    "aider",
    "cursor-agent",
    "crush",
    "goose",
    "browser-daemon",
];

/// `$PWD` as bash keeps it: the inherited value when it still names the
/// current directory, else the physical path.
pub fn pwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let inherited = env("PWD");
    if inherited.starts_with('/')
        && let (Ok(a), Ok(b)) = (fs::metadata(&inherited), fs::metadata(&cwd))
        && a.dev() == b.dev()
        && a.ino() == b.ino()
    {
        return inherited;
    }
    cwd.to_string_lossy().into_owned()
}

fn parent_of(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The first agent process among our ancestors, with its PID.
fn find_agent() -> Option<(String, u32)> {
    // SAFETY: getppid has no preconditions.
    let mut pid = unsafe { libc::getppid() } as u32;
    while pid > 1 {
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        let comm = comm.trim_end_matches('\n');
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let argv0 = cmdline
            .split(|b| *b == 0 || *b == b'\n')
            .next()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default();
        // Nix wraps binaries: comm is then ".claude-wrapped".
        let comm = comm.strip_prefix('.').unwrap_or(comm);
        let base = argv0.rsplit('/').next().unwrap_or("").to_string();
        for cand in [comm.strip_suffix("-wrapped").unwrap_or(comm), base.as_str()] {
            if AGENTS.contains(&cand) {
                return Some((cand.to_string(), pid));
            }
        }
        pid = parent_of(pid)?;
    }
    None
}

pub fn meta(topic: &str, agent_id: &str) -> Vec<String> {
    let agent_id = if agent_id.is_empty() {
        let a = env("RWORKER_AGENT_ID");
        if a.is_empty() {
            env("CONCORD_AGENT_ID")
        } else {
            a
        }
    } else {
        agent_id.to_string()
    };
    let session = [
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
    ]
    .iter()
    .map(|k| env(k))
    .find(|v| !v.is_empty())
    .unwrap_or_default();
    let (agent, agent_pid) = match find_agent() {
        Some((a, p)) => (a, Some(p)),
        None => {
            let agent = if !env("CLAUDE_CODE_SESSION_ID").is_empty() {
                "claude"
            } else if !(env("CODEX_THREAD_ID") + &env("CODEX_SESSION_ID")).is_empty() {
                "codex"
            } else {
                "shell"
            };
            (agent.to_string(), None)
        }
    };
    let mut out = vec![
        "--meta".to_string(),
        format!("agent={agent}"),
        "--meta".to_string(),
        format!("cwd={}", pwd()),
    ];
    if let Some(p) = agent_pid {
        out.extend(["--meta".to_string(), format!("agent_pid={p}")]);
    }
    if !session.is_empty() {
        out.extend(["--meta".to_string(), format!("agent_session={session}")]);
    }
    if !agent_id.is_empty() {
        out.extend(["--meta".to_string(), format!("agent_id={agent_id}")]);
    }
    if !topic.is_empty() {
        out.extend(["--meta".to_string(), format!("topic={topic}")]);
    }
    out
}
