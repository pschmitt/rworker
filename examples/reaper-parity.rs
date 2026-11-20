//! Test driver for tests/server-parity/reaper.py.
#[path = "../src/server/reaper.rs"]
mod reaper;

use std::io::{self, BufRead, Write};

use reaper::{AfterProbe, BeforeProbe, Policy, Worker};
use serde_json::{Value, json};

fn operations(input: &Value) -> Vec<Value> {
    let cfg = &input["config"];
    let meta = &input["metadata"];
    let number = |value: &Value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    };
    let policy = Policy {
        boot_timeout: number(&cfg["bootTimeout"]).unwrap(),
        grace_period: number(&cfg["adhocGracePeriod"]).unwrap(),
        max_ttl: number(&cfg["adhocMaxTtl"]).unwrap(),
    };
    let worker = Worker {
        created_at: number(&meta["created_at"]),
        expires_at: number(&meta["expires_at"]).unwrap_or_default(),
        max_lifetime: number(&meta["max_lifetime"]),
        host_key_present: meta.get("host_key").is_some(),
        last_active: number(&meta["last_active"]).unwrap_or_default(),
        token_expires_at: number(&meta["niks3_token_exp"]).unwrap_or_default(),
    };
    let now = number(&input["now"]).unwrap();
    match policy.before_probe(&worker, now) {
        BeforeProbe::Keep => return vec![],
        BeforeProbe::DeleteUnready | BeforeProbe::DeleteAtMaximumLifetime => {
            return vec![json!("delete")];
        }
        BeforeProbe::Probe => {}
    }

    let mut operations = vec![json!("route")];
    if worker.host_key_present {
        operations.push(json!("probe"));
    }
    let activity = if worker.host_key_present {
        input["active"].as_bool()
    } else {
        None
    };
    match policy.after_probe(&worker, now, activity) {
        AfterProbe::Keep => {}
        AfterProbe::DeleteExpired => operations.push(json!("delete")),
        AfterProbe::Busy {
            record_activity,
            renew_token_until,
            ..
        } => {
            if record_activity {
                operations.push(json!(["last_active", (now.trunc() as i64).to_string()]));
            }
            if let Some(end) = renew_token_until {
                operations.push(json!(["token", end]));
            }
        }
    }
    operations
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let input = serde_json::from_str(&line?)?;
        serde_json::to_writer(&mut stdout, &operations(&input))?;
        writeln!(stdout)?;
    }
    Ok(())
}
