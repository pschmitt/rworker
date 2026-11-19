//! Running external commands (ssh, rsync, nix, ssh-keygen) the way the
//! shell did: inherited stdin, stderr unless muted, `$(...)` capture.

use crate::ctx::{Ctx, Fail, R};
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};

/// The shell's `$?` for a finished process.
pub fn code_of(st: ExitStatus) -> i32 {
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

fn spawn_failed(ctx: &Ctx, cmd: &Command, e: &std::io::Error) -> Fail {
    let prog = cmd.get_program().to_string_lossy();
    ctx.err(&format!("{}: {prog}: {e}", ctx.me));
    Fail(127)
}

/// `$(cmd)`: stdout with trailing newlines removed, and the exit status.
pub fn capture(ctx: &Ctx, cmd: &mut Command) -> R<(i32, String)> {
    cmd.stdout(Stdio::piped()).stderr(ctx.child_stderr());
    let out = cmd.output().map_err(|e| spawn_failed(ctx, cmd, &e))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok((code_of(out.status), trim_nl(&text)))
}

/// `$(cmd)` that must succeed: its output, or Fail(1) like `|| return 1`.
pub fn capture_ok(ctx: &Ctx, cmd: &mut Command) -> R<String> {
    match capture(ctx, cmd)? {
        (0, text) => Ok(text),
        _ => Err(Fail(1)),
    }
}

/// `$(printf ... | cmd)`.
pub fn capture_with_input(ctx: &Ctx, cmd: &mut Command, input: &[u8]) -> R<(i32, String)> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(ctx.child_stderr());
    let mut child = cmd.spawn().map_err(|e| spawn_failed(ctx, cmd, &e))?;
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let out = child
        .wait_with_output()
        .map_err(|e| spawn_failed(ctx, cmd, &e))?;
    Ok((
        code_of(out.status),
        trim_nl(&String::from_utf8_lossy(&out.stdout)),
    ))
}

/// Run with stdout and stderr where they are (stderr muted if asked).
pub fn run(ctx: &Ctx, cmd: &mut Command) -> R<i32> {
    cmd.stderr(ctx.child_stderr());
    let st = cmd.status().map_err(|e| spawn_failed(ctx, cmd, &e))?;
    Ok(code_of(st))
}

/// `cmd > /dev/null`.
pub fn run_quiet_stdout(ctx: &Ctx, cmd: &mut Command) -> R<i32> {
    cmd.stdout(Stdio::null());
    run(ctx, cmd)
}

/// `cmd <<< INPUT` with stdout and stderr passed through.
pub fn run_with_input(ctx: &Ctx, cmd: &mut Command, input: &[u8]) -> R<i32> {
    cmd.stdin(Stdio::piped()).stderr(ctx.child_stderr());
    let mut child = cmd.spawn().map_err(|e| spawn_failed(ctx, cmd, &e))?;
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let st = child.wait().map_err(|e| spawn_failed(ctx, cmd, &e))?;
    Ok(code_of(st))
}

pub fn trim_nl(s: &str) -> String {
    s.trim_end_matches('\n').to_string()
}

/// Bash-compatible quoting of one word for a remote shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./=:,@+%^-".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The words as one remote command line, each followed by a space (as
/// `printf '%q ' "$@"` produced).
pub fn quote_words<S: AsRef<str>>(words: &[S]) -> String {
    if words.is_empty() {
        return "'' ".to_string();
    }
    words
        .iter()
        .map(|w| format!("{} ", shell_quote(w.as_ref())))
        .collect()
}

pub fn sleep_secs(secs: f64) {
    std::thread::sleep(std::time::Duration::from_secs_f64(secs.max(0.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("ls"), "ls");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(quote_words(&["up", "--topic", "x y"]), "up --topic 'x y' ");
    }
}
