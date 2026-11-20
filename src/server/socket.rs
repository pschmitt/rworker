//! Local control transport. Requests are a JSON argv line followed by EOF.
//! Responses carry stream tags so progress remains on stderr.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::ctx::{Fail, R};

pub(crate) fn request(path: &str, args: &[String]) -> R {
    match exchange(path, args) {
        Ok(0) => Ok(()),
        Ok(code) => Err(Fail(code)),
        Err(error) => {
            eprintln!("rworker control socket: {error}");
            Err(Fail(1))
        }
    }
}

fn exchange(path: &str, args: &[String]) -> io::Result<i32> {
    let mut socket = UnixStream::connect(path)?;
    serde_json::to_writer(&mut socket, args)?;
    socket.write_all(b"\n")?;
    socket.shutdown(Shutdown::Write)?;
    let mut reader = BufReader::new(socket);
    let mut frame = Vec::new();
    loop {
        frame.clear();
        if reader.read_until(b'\n', &mut frame)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "missing exit status",
            ));
        }
        match frame[0] {
            b'o' => {
                let mut out = io::stdout().lock();
                out.write_all(&frame[1..])?;
                out.flush()?;
            }
            b'e' => io::stderr().lock().write_all(&frame[1..])?,
            b'x' => {
                return std::str::from_utf8(&frame[1..])
                    .ok()
                    .and_then(|status| status.trim().parse::<u8>().ok())
                    .map(i32::from)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid exit status")
                    });
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown response frame",
                ));
            }
        }
    }
}

/// Serve one connection, with stdin and stdout attached to the accepted socket.
pub(crate) fn serve(config: &str) -> R {
    match serve_connection(config) {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("rworker control connection: {error}");
            Err(Fail(1))
        }
    }
}

fn serve_connection(config: &str) -> io::Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::sync_channel;

    const MAX_REQUEST: u64 = 256 * 1024;
    let mut request = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_REQUEST + 1)
        .read_until(b'\n', &mut request)?;
    if request.len() as u64 > MAX_REQUEST || !request.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid control request size",
        ));
    }
    let args: Vec<String> = serde_json::from_slice(&request)?;
    if args
        .first()
        .is_some_and(|action| matches!(action.as_str(), "refresh-inventory" | "inventory-api"))
    {
        io::stdout()
            .lock()
            .write_all(b"eInvalid control command\nx2\n")?;
        return Ok(());
    }
    if matches!(args.as_slice(), [action, rest @ ..] if action == "ls" && (rest.is_empty() || rest == ["--fresh"]))
        && let Some(dir) = super::inventory_cache::directory()
    {
        let result = super::inventory_cache::list(&dir, args.len() == 2);
        let mut out = io::stdout().lock();
        match result {
            Ok(workers) => {
                out.write_all(b"o")?;
                serde_json::to_writer(&mut out, &workers)?;
                out.write_all(b"\nx0\n")?;
            }
            Err(error) => write!(out, "e{error}\nx1\n")?,
        }
        return out.flush();
    }
    let mut child = Command::new(std::env::current_exe()?)
        .args(["server", config])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let (send, receive) = sync_channel(16);
    let pipes: [(u8, Option<Box<dyn Read + Send>>); 2] = [
        (b'o', child.stdout.take().map(|pipe| Box::new(pipe) as _)),
        (b'e', child.stderr.take().map(|pipe| Box::new(pipe) as _)),
    ];
    let forwarded = std::thread::scope(|scope| {
        for (tag, pipe) in pipes {
            let Some(pipe) = pipe else { continue };
            let send = send.clone();
            scope.spawn(move || {
                let mut reader = BufReader::new(pipe);
                loop {
                    let mut frame = vec![tag];
                    match reader.read_until(b'\n', &mut frame) {
                        Ok(0) => break,
                        Ok(_) => {
                            if !frame.ends_with(b"\n") {
                                frame.push(b'\n');
                            }
                            if send.send(Ok(frame)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = send.send(Err(error));
                            break;
                        }
                    }
                }
            });
        }
        drop(send);
        let result = (|| -> io::Result<()> {
            let mut out = io::stdout().lock();
            while let Ok(frame) = receive.recv() {
                out.write_all(&frame?)?;
                out.flush()?;
            }
            Ok(())
        })();
        drop(receive);
        if result.is_err() {
            let _ = child.kill();
        }
        result
    });
    let status = child.wait()?;
    forwarded?;
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    let mut out = io::stdout().lock();
    writeln!(out, "x{code}")?;
    out.flush()
}
