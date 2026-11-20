//! Settings read once from the environment, and the few switches that
//! commands flip while they run (`--json`, `-o wide`, `--quiet`).

use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::Stdio;

pub struct Ctx {
    /// Program name for help and hints (basename of argv[0]).
    pub me: String,
    pub control_host: String,
    pub state_dir: PathBuf,
    pub home: String,
    /// `$USER`, possibly empty.
    pub user: String,
    pub hostname: String,
    /// Nerd Font glyphs for the creating agent.
    pub icons: bool,
    /// Colors for human-readable output: stdout is a terminal and NO_COLOR
    /// is unset. Decided once, like the bash client.
    pub color: bool,
    /// NO_COLOR is set (non-empty): no colors in help either.
    pub no_color: bool,
    /// OpenSSH connection sharing for the control host.
    pub control_master: bool,
    pub json: Cell<bool>,
    pub wide: Cell<bool>,
    pub quiet: Cell<bool>,
    pub filter_agent_id: RefCell<String>,
    mute: Cell<u32>,
}

pub fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// `${NAME:-default}`.
pub fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

pub fn hostname() -> String {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: the buffer is valid for its length and NUL-terminated below.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len() - 1) };
    if rc != 0 {
        return String::new();
    }
    // SAFETY: gethostname wrote a NUL-terminated string into buf (the last
    // byte stays 0).
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

impl Ctx {
    pub fn from_env() -> Ctx {
        let me = std::env::args_os()
            .next()
            .map(PathBuf::from)
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "rworker".to_string());
        let home = env("HOME");
        let state_base = env_or("XDG_STATE_HOME", &format!("{home}/.local/state"));
        let no_color = !env("NO_COLOR").is_empty();
        // Child processes of `logs -f` inherit the parent's decision, made
        // on the parent's stdout.
        let color = match env("__RWORKER_COLOR").as_str() {
            "1" => true,
            "0" => false,
            _ => std::io::stdout().is_terminal() && !no_color,
        };
        Ctx {
            me,
            control_host: env_or("RWORKER_HOST", "rofl-10"),
            state_dir: PathBuf::from(state_base).join("rworker"),
            home,
            user: env("USER"),
            hostname: hostname(),
            icons: env_or("RWORKER_ICONS", "1") != "0",
            color,
            no_color,
            control_master: env_or("RWORKER_SSH_CONTROL_MASTER", "1") != "0",
            json: Cell::new(false),
            wide: Cell::new(false),
            quiet: Cell::new(env("__RWORKER_QUIET") == "1"),
            filter_agent_id: RefCell::new(String::new()),
            mute: Cell::new(0),
        }
    }

    /// `$USER@$(hostname)`: who owns the workers this host creates.
    pub fn owner(&self) -> String {
        format!("{}@{}", self.user, self.hostname)
    }

    pub fn ls_cache(&self) -> PathBuf {
        self.state_dir.join(".ls.json")
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.state_dir.join("known_hosts")
    }

    pub fn state_file(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.json"))
    }

    pub fn key_file(&self) -> String {
        env_or("RWORKER_KEY", &format!("{}/.ssh/id_ed25519.pub", self.home))
    }

    /// Run `f` with stderr silenced (`2>/dev/null` around a shell function):
    /// our own messages and those of the commands it starts.
    pub fn muted<T>(&self, f: impl FnOnce() -> T) -> T {
        self.mute.set(self.mute.get() + 1);
        let out = f();
        self.mute.set(self.mute.get() - 1);
        out
    }

    pub fn is_muted(&self) -> bool {
        self.mute.get() > 0
    }

    /// stderr for a child process: ours, or nothing while muted.
    pub fn child_stderr(&self) -> Stdio {
        if self.is_muted() {
            Stdio::null()
        } else {
            Stdio::inherit()
        }
    }

    /// One message line on stderr (unless muted).
    pub fn err(&self, msg: &str) {
        if !self.is_muted() {
            let mut e = std::io::stderr().lock();
            let _ = writeln!(e, "{msg}");
        }
    }

    /// Text on stderr as is (unless muted).
    pub fn err_raw(&self, msg: &str) {
        if !self.is_muted() {
            let mut e = std::io::stderr().lock();
            let _ = e.write_all(msg.as_bytes());
        }
    }
}

/// A failed command: its exit status. Messages were printed already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fail(pub i32);

pub type R<T = ()> = Result<T, Fail>;

/// Exit status of a finished command as a result: 0 is Ok.
pub fn status(code: i32) -> R {
    if code == 0 { Ok(()) } else { Err(Fail(code)) }
}

/// Print to stdout. A closed pipe ends the program like it ends a shell
/// script (SIGPIPE is restored to its default at startup).
pub fn out(text: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(text.as_bytes());
    let _ = o.flush();
}

pub fn outln(text: &str) {
    out(&format!("{text}\n"));
}

/// Preserve each command's existing machine-readable output when redirected.
pub fn json_out(ctx: &Ctx, text: &str) {
    if !std::io::stdout().is_terminal() {
        outln(text);
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        outln(text);
        return;
    };
    let pretty = crate::jq::pretty(&value);
    if !ctx.color {
        outln(&pretty);
        return;
    }
    static TOKEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#""(?:[^"\\]|\\.)*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null"#)
            .expect("valid JSON token pattern")
    });
    let colored = TOKEN.replace_all(&pretty, |token: &regex::Captures<'_>| {
        let value = token.get(0).expect("JSON token match");
        let color = if value.as_str().starts_with('"') {
            if pretty[value.end()..].trim_start().starts_with(':') {
                36
            } else {
                32
            }
        } else {
            35
        };
        format!("\x1b[{color}m{value}\x1b[0m", value = value.as_str())
    });
    outln(&colored);
}
