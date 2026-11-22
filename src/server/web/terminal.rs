//! Interactive worker SSH over a same-origin, authenticated WebSocket.

use super::{authorized, failure, log_stream, name, same_origin};
use axum::Router;
use axum::extract::Path;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path as FilePath, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{Interest, unix::AsyncFd};
use tokio::process::Child;
use tokio::sync::Semaphore;
use tokio::time::timeout;

static SESSIONS: Semaphore = Semaphore::const_new(8);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INPUT: usize = 64 * 1024;

pub(super) fn routes() -> Router<PathBuf> {
    Router::new().route("/v1/ui/follow/ssh/{worker}", get(connect))
}

async fn connect(
    Path(worker): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return failure((StatusCode::FORBIDDEN, "Invalid WebSocket origin."));
    }
    let check = authorized(&headers);
    if let Err(error) = check
        && error.0 != StatusCode::UNAUTHORIZED
    {
        return failure(error);
    }
    if let Err(error) = name(&worker) {
        return failure(error);
    }
    let Ok(permit) = SESSIONS.try_acquire() else {
        return failure((StatusCode::SERVICE_UNAVAILABLE, "Too many SSH sessions."));
    };
    upgrade
        .max_message_size(MAX_INPUT)
        .max_frame_size(MAX_INPUT)
        .on_upgrade(move |mut socket| async move {
            let _permit = permit;
            let Ok(Some(Ok(Message::Text(request)))) = timeout(IO_TIMEOUT, socket.recv()).await
            else {
                return;
            };
            let Ok(request) = serde_json::from_str::<Connect>(&request) else {
                return;
            };
            if check.is_err() && !log_stream::authorize_token(&request.token) {
                let _ = send(&mut socket, Message::Text("SSH access denied.".into())).await;
                return;
            }
            if session(&mut socket, &worker, request.size).await.is_err() {
                let _ = send(&mut socket, Message::Text("SSH session failed.".into())).await;
            }
        })
        .into_response()
}

#[derive(Deserialize)]
struct Connect {
    #[serde(default)]
    token: String,
    size: Size,
}

#[derive(Deserialize)]
struct Size {
    cols: u16,
    rows: u16,
}

impl Size {
    fn apply(&self, fd: &File) -> io::Result<()> {
        if !(1..=512).contains(&self.cols) || !(1..=256).contains(&self.rows) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid terminal size",
            ));
        }
        let size = libc::winsize {
            ws_col: self.cols,
            ws_row: self.rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: fd is a live PTY and size points to a fully initialized winsize.
        syscall(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) })
    }
}

fn syscall(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct SshProcess(Child);

impl Drop for SshProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            // SAFETY: this unreaped child is the leader of its own session and
            // the group ID is still reserved by that child.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
    }
}

fn spawn(worker: &str, size: Size) -> io::Result<(AsyncFd<File>, SshProcess)> {
    // SAFETY: successful posix_openpt returns a new descriptor owned by us.
    let fd = unsafe {
        libc::posix_openpt(libc::O_CLOEXEC | libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK)
    };
    syscall(fd)?;
    let master = unsafe { File::from_raw_fd(fd) };
    // SAFETY: fd is a live PTY master and path is a writable character buffer.
    let mut path = [0; 128];
    unsafe {
        syscall(libc::grantpt(fd))?;
        syscall(libc::unlockpt(fd))?;
        let result = libc::ptsname_r(fd, path.as_mut_ptr(), path.len());
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
    }
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: successful ptsname_r wrote a NUL-terminated path.
    let path = unsafe { CStr::from_ptr(path.as_ptr()) };
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(FilePath::new(std::ffi::OsStr::from_bytes(path.to_bytes())))?;
    size.apply(&master)?;
    // SAFETY: AsyncFd owns the nonblocking File and its descriptor stays open
    // and refers to this PTY for as long as the registration exists.
    let master = unsafe { AsyncFd::register(master) }.map_err(|error| error.into_parts().1)?;

    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["__web-ssh", worker])
        .env("TERM", "xterm-256color")
        .env_remove("SSH_CONNECTION")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: pre_exec only calls async-signal-safe syscalls. By this point the
    // child stdin is the PTY slave, which becomes its controlling terminal.
    unsafe {
        command.pre_exec(|| {
            syscall(libc::setsid())?;
            syscall(libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0))
        });
    }
    let child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    Ok((master, SshProcess(child)))
}

async fn send(socket: &mut WebSocket, frame: Message) -> io::Result<()> {
    timeout(IO_TIMEOUT, socket.send(frame))
        .await?
        .map_err(io::Error::other)
}

async fn session(socket: &mut WebSocket, worker: &str, size: Size) -> io::Result<()> {
    let (pty, _child) = spawn(worker, size)?;
    send(socket, Message::Text("SSH session opened.".into())).await?;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut output = [0; 8192];
    let mut input = axum::body::Bytes::new();
    let mut write_deadline = tokio::time::Instant::now();
    loop {
        tokio::select! {
            result = pty.async_io(Interest::READABLE, |mut file| file.read(&mut output)) => {
                let count = match result {
                    Ok(0) => break,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    result => result?,
                };
                send(socket, Message::Binary(output[..count].to_vec().into())).await?;
            }
            result = pty.async_io(Interest::WRITABLE, |mut file| file.write(&input)), if !input.is_empty() => {
                input = input.slice(result?..);
            }
            message = socket.recv(), if input.is_empty() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    input = bytes;
                    write_deadline = tokio::time::Instant::now() + IO_TIMEOUT;
                }
                Some(Ok(Message::Text(text))) => serde_json::from_str::<Size>(&text)?.apply(pty.get_ref())?,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => break,
            },
            _ = tokio::time::sleep_until(write_deadline), if !input.is_empty() => break,
            _ = ping.tick() => send(socket, Message::Ping(axum::body::Bytes::new())).await?,
        }
    }
    Ok(())
}
