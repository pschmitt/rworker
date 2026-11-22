//! Verify GitHub-hosted runner identity before processing a lease claim.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;

const ISSUER: &str = "https://token.actions.githubusercontent.com";
const JWKS_URL: &str = "https://token.actions.githubusercontent.com/.well-known/jwks";
const AUDIENCE: &str = "rworker-job";
const MAX_TOKEN_BYTES: usize = 16 * 1024;

struct Cache {
    keys: Vec<Jwk>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    keys: Vec::new(),
    fetched: None,
    attempted: None,
});

#[derive(Clone, Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    n: String,
    e: String,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: String,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    exp: u64,
    nbf: u64,
    repository: String,
    repository_id: String,
    workflow_ref: String,
    #[serde(rename = "ref")]
    git_ref: String,
    event_name: String,
    runner_environment: String,
    run_id: String,
    run_attempt: String,
}

pub(super) fn verify(
    token: &str,
    repository: &str,
    repository_id: &str,
) -> Result<u64, &'static str> {
    let invalid = "Invalid GitHub identity";
    let claims: Claims = signed_claims(token, &CACHE, JWKS_URL).map_err(|_| invalid)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid)?
        .as_secs();
    if claims.iss != ISSUER || claims.aud != AUDIENCE || claims.exp <= now || claims.nbf > now {
        return Err(invalid);
    }
    let workflow_ref = format!("{repository}/.github/workflows/rworker-job.yaml@refs/heads/main");
    if claims.repository != repository
        || claims.repository_id != repository_id
        || claims.workflow_ref != workflow_ref
        || claims.git_ref != "refs/heads/main"
        || claims.event_name != "workflow_dispatch"
        || claims.runner_environment != "github-hosted"
        || claims.run_attempt != "1"
    {
        return Err("Unexpected GitHub workflow identity");
    }
    claims
        .run_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or("Invalid GitHub run ID")
}

fn signed_claims<T: serde::de::DeserializeOwned>(
    token: &str,
    cache: &Mutex<Cache>,
    jwks_url: &str,
) -> Result<T, &'static str> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err("Invalid OIDC identity");
    }
    let invalid = "Invalid OIDC identity";
    let (signed, signature) = token.rsplit_once('.').ok_or(invalid)?;
    let (header, payload) = signed.split_once('.').ok_or(invalid)?;
    let decode = |segment: &str| URL_SAFE_NO_PAD.decode(segment).map_err(|_| invalid);
    let header: Header = serde_json::from_slice(&decode(header)?).map_err(|_| invalid)?;
    if header.alg != "RS256" {
        return Err(invalid);
    }
    let key = key_for(cache, jwks_url, &header.kid)?;
    let public_key = RsaPublicKeyComponents {
        n: decode(&key.n)?,
        e: decode(&key.e)?,
    };
    public_key
        .verify(
            &RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &decode(signature)?,
        )
        .map_err(|_| invalid)?;
    serde_json::from_slice(&decode(payload)?).map_err(|_| invalid)
}

fn key_for(cache: &Mutex<Cache>, jwks_url: &str, kid: &str) -> Result<Jwk, &'static str> {
    let mut cache = cache.lock().map_err(|_| "GitHub key cache unavailable")?;
    let lookup = |cache: &Cache| {
        cache
            .fetched
            .filter(|time| time.elapsed() < Duration::from_secs(3600))
            .and_then(|_| {
                cache
                    .keys
                    .iter()
                    .find(|key| key.kid == kid && key.kty == "RSA")
            })
            .cloned()
    };
    if let Some(key) = lookup(&cache) {
        return Ok(key);
    }
    if cache
        .attempted
        .is_some_and(|time| time.elapsed() < Duration::from_secs(60))
    {
        return Err("Unknown GitHub signing key");
    }
    cache.attempted = Some(Instant::now());
    let response = super::http::json_request("GET", jwks_url, &[], None)
        .map_err(|_| "Fetch GitHub signing keys")?;
    cache.keys = serde_json::from_value(response["keys"].clone())
        .map_err(|_| "Invalid GitHub signing keys")?;
    cache.fetched = Some(Instant::now());
    lookup(&cache).ok_or("Unknown GitHub signing key")
}

#[derive(Deserialize)]
struct GitLabClaims {
    iss: String,
    aud: String,
    exp: u64,
    nbf: u64,
    job_project_id: String,
    job_project_path: String,
    pipeline_id: String,
    pipeline_source: String,
    ref_path: String,
    ref_protected: String,
    runner_id: u64,
    ci_config_ref_uri: String,
}

pub(super) fn verify_gitlab(token: &str, config: &serde_json::Value) -> Result<u64, &'static str> {
    static CACHES: Mutex<std::collections::BTreeMap<String, std::sync::Arc<Mutex<Cache>>>> =
        Mutex::new(std::collections::BTreeMap::new());
    let invalid = "Invalid GitLab identity";
    let issuer = config["instance"].as_str().ok_or(invalid)?;
    let host = super::gitlab::instance_host(issuer).ok_or(invalid)?;
    let project = config["project"].as_str().ok_or(invalid)?;
    let project_id = config["projectId"].as_u64().ok_or(invalid)?;
    let runner_id = config["runnerId"]
        .as_u64()
        .filter(|id| *id != 0)
        .ok_or(invalid)?;
    let cache = CACHES
        .lock()
        .map_err(|_| invalid)?
        .entry(issuer.into())
        .or_insert_with(|| {
            std::sync::Arc::new(Mutex::new(Cache {
                keys: Vec::new(),
                fetched: None,
                attempted: None,
            }))
        })
        .clone();
    let jwks_url = format!("{issuer}/oauth/discovery/keys");
    let claims: GitLabClaims = signed_claims(token, &cache, &jwks_url).map_err(|_| invalid)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid)?
        .as_secs();
    if claims.iss != issuer
        || claims.aud != AUDIENCE
        || claims.exp <= now
        || claims.nbf > now
        || claims.job_project_id.parse::<u64>() != Ok(project_id)
        || claims.job_project_path != project
        || claims.pipeline_source != "api"
        || claims.ref_path != "refs/heads/main"
        || claims.ref_protected != "true"
        || claims.runner_id != runner_id
        || claims.ci_config_ref_uri != format!("{host}/{project}//.gitlab-ci.yml@refs/heads/main")
    {
        return Err(invalid);
    }
    claims
        .pipeline_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or(invalid)
}
