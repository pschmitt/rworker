use crate::ctx::{Ctx, Fail, R};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

const ENDPATH: &str = "/v1/workers/events/ws";

fn curl() -> Command {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--disable",
        "--silent",
        "--fail",
        "--no-buffer",
        "--connect-timeout",
        "3",
    ]);
    cmd
}

fn local() -> Command {
    let mut cmd = curl();
    let socket = std::env::var_os("RWORKER_INVENTORY_HTTP_SOCKET")
        .unwrap_or_else(|| "/run/rworker-inventory-api/http.sock".into());
    cmd.arg("--unix-socket")
        .arg(socket)
        .arg(format!("ws://localhost{ENDPATH}"));
    cmd
}

pub(crate) fn serve(ctx: &Ctx) -> R {
    let error = local().stdin(Stdio::null()).exec();
    ctx.err(&format!("Could not start inventory WebSocket: {error}"));
    Err(Fail(1))
}

pub(crate) fn endpoint() -> Option<String> {
    std::env::var("RWORKER_API_URL")
        .ok()
        .and_then(|url| url.strip_prefix("https://").map(str::to_owned))
}

pub(crate) struct Feed {
    child: Child,
    frames: Receiver<Vec<u8>>,
}

impl Feed {
    pub(crate) fn open(ctx: &Ctx, endpoint: Option<&str>) -> Option<Self> {
        let mut cmd = if let Some(endpoint) = endpoint {
            let mut cmd = curl();
            if let Some(config) = std::env::var_os("RWORKER_API_CURL_CONFIG") {
                cmd.arg("--config").arg(config);
            }
            cmd.args(["--proto", "=wss", "--url"])
                .arg(format!("wss://{}{ENDPATH}", endpoint.trim_end_matches('/')));
            cmd
        } else if std::env::var_os("RWORKER_CONTROL_SOCKET").is_some() {
            local()
        } else {
            crate::state::control_ssh_cmd(ctx, &["__inventory-websocket"])
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: prctl is async-signal-safe. Tie the feed to its watcher thread.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().ok()?;
        let output = child.stdout.take().expect("stdout was piped");
        let (tx, frames) = mpsc::sync_channel(4);
        std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let read = (&mut output)
                    .take(16 * 1024 * 1024)
                    .read_until(b'\n', &mut line);
                if !matches!(read, Ok(n) if n > 0 && line.ends_with(b"\n"))
                    || tx.send(line).is_err()
                {
                    return;
                }
            }
        });
        Some(Self { child, frames })
    }

    pub(crate) fn receive(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        self.frames.recv_timeout(timeout)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
