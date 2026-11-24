//! JSON calls to the overlay APIs. The curl config travels on stdin so
//! authorization headers and request bodies stay out of process arguments.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

fn quoted(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

pub(super) fn json_request(
    method: &str,
    url: &str,
    headers: &[&str],
    body: Option<&str>,
) -> Result<Value, String> {
    let label = format!("HTTP {method}");
    let config = (|| -> std::io::Result<Vec<u8>> {
        let mut config = Vec::new();
        writeln!(config, "url = {}", quoted(url))?;
        writeln!(config, "request = {}", quoted(method))?;
        for header in headers {
            writeln!(config, "header = {}", quoted(header))?;
        }
        if let Some(body) = body {
            writeln!(config, "data-raw = {}", quoted(body))?;
        }
        Ok(config)
    })()
    .map_err(|error| format!("{label}: {error}"))?;
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--silent",
            "--fail",
            "--connect-timeout",
            "5",
            "--max-time",
            "30",
            "--proto",
            "=https,http",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{label}: {error}"))?;
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(&config)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{label}: {error}"));
    }
    let result = child
        .wait_with_output()
        .map_err(|error| format!("{label}: {error}"))?;
    if !result.status.success() {
        return Err(format!("{label}: curl {}", result.status));
    }
    if result.stdout.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&result.stdout).map_err(|_| format!("{label}: invalid JSON response"))
}
