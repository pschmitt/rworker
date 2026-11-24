//! JWT signing for per-worker niks3 credentials.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::openstack::setting;

fn base64url(bytes: &[u8]) -> String {
    crate::cmd::simple::base64(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

pub(super) fn token_id() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct SigningInput(PathBuf);

impl Drop for SigningInput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn mint(cfg: &Value, name: &str, now: i64, expires: i64) -> Result<String, String> {
    let key = setting(cfg, "signingKeyFile")?;
    let id = token_id().map_err(|err| format!("Generate niks3 token ID: {err}"))?;
    let header = json!({"alg": "EdDSA", "typ": "JWT", "kid": setting(cfg, "keyId")?});
    let claims = json!({
        "iss": setting(cfg, "issuer")?,
        "aud": setting(cfg, "audience")?,
        "sub": format!("rworker:{name}"),
        "iat": now,
        "exp": expires,
        "jti": id,
    });
    let unsigned = format!(
        "{}.{}",
        base64url(header.to_string().as_bytes()),
        base64url(claims.to_string().as_bytes()),
    );
    // Ed25519 signs the whole message in one operation. OpenSSL versions
    // that require a seekable input can read this private file too.
    let path = std::env::temp_dir().join(format!("rworker-jwt-{id}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|err| format!("Create niks3 signing input: {err}"))?;
    let input = SigningInput(path);
    file.write_all(unsigned.as_bytes())
        .map_err(|err| format!("Write niks3 signing input: {err}"))?;
    drop(file);
    let signature = Command::new("openssl")
        .args(["pkeyutl", "-sign", "-rawin", "-inkey", key, "-in"])
        .arg(&input.0)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|err| format!("Run OpenSSL: {err}"))?;
    if !signature.status.success() || signature.stdout.len() != 64 {
        return Err("Failed to sign niks3 token with an Ed25519 key".into());
    }
    Ok(format!("{unsigned}.{}", base64url(&signature.stdout)))
}

pub(super) fn install(
    settings: &Value,
    compute: &mut impl super::mutations::Compute<WorkerId = String>,
    id: &String,
    name: &str,
    metadata: &serde_json::Map<String, Value>,
    expires: i64,
) -> Result<Value, String> {
    let Some(key) = metadata
        .get("host_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    else {
        return Ok(json!({"ok": false, "error": "worker not reachable yet"}));
    };
    let route = match super::worker_ssh::route(settings, name) {
        Ok(route) => route,
        Err(error) => return Ok(json!({"ok": false, "error": error})),
    };
    let Some(ip) = route else {
        return Ok(json!({"ok": false, "error": "worker not reachable yet"}));
    };
    let cfg = &settings["niks3"];
    let token = mint(cfg, name, super::epoch() as i64, expires)?;
    let url = setting(cfg, "serverUrl")?;
    let url_arg = crate::proc::quote_words(&[url]);
    let remote = format!(
        "d=/run/rworker-niks3; test -d \"$d\" || exit 3; umask 027; printf '%s\\n' {url_arg} > \"$d/server-url\" && cat > \"$d/token.new\" && mv -f \"$d/token.new\" \"$d/token\""
    );
    let mut error = String::new();
    for attempt in 0..6 {
        error = match super::worker_ssh::run(settings, ip, key, &remote, Some(&token), 30) {
            Ok(output) if output.status.success() => {
                compute.set_metadata(
                    id,
                    [("niks3_token_exp".into(), json!(expires.to_string()))]
                        .into_iter()
                        .collect(),
                )?;
                return Ok(json!({"ok": true, "expires_at": expires, "server_url": url}));
            }
            Ok(output) if output.status.code() == Some(3) => {
                return Ok(json!({"ok": false, "error": "image-too-old"}));
            }
            Ok(output) => format!("ssh exited {}", output.status.code().unwrap_or(1)),
            Err(error) => error,
        };
        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    eprintln!("niks3 token for {name}: {error}");
    Ok(json!({"ok": false, "error": error}))
}
