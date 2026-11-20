//! Legacy reaper policy. The caller supplies activity and executes decisions.

pub(super) struct Policy {
    pub boot_timeout: f64,
    pub grace_period: f64,
    pub max_ttl: f64,
}

pub(super) struct Worker {
    pub created_at: Option<f64>,
    pub expires_at: f64,
    pub max_lifetime: Option<f64>,
    pub host_key_present: bool,
    pub last_active: f64,
    pub token_expires_at: f64,
}

#[derive(Debug, PartialEq)]
pub(super) enum BeforeProbe {
    Keep,
    Probe,
    DeleteUnready,
    DeleteAtMaximumLifetime,
}

#[derive(Debug, PartialEq)]
pub(super) enum AfterProbe {
    Keep,
    DeleteExpired,
    Busy {
        record_activity: bool,
        report_expired: bool,
        renew_token_until: Option<f64>,
    },
}

impl Policy {
    fn lifetime_end(&self, worker: &Worker, now: f64) -> f64 {
        worker.created_at.unwrap_or(now) + worker.max_lifetime.unwrap_or(self.max_ttl)
    }

    pub fn before_probe(&self, worker: &Worker, now: f64) -> BeforeProbe {
        if !worker.host_key_present && now - worker.created_at.unwrap_or(now) > self.boot_timeout {
            BeforeProbe::DeleteUnready
        } else if worker.expires_at == 0.0 || worker.expires_at > now {
            BeforeProbe::Keep
        } else if now >= self.lifetime_end(worker, now) {
            BeforeProbe::DeleteAtMaximumLifetime
        } else {
            BeforeProbe::Probe
        }
    }

    pub fn after_probe(&self, worker: &Worker, now: f64, active: Option<bool>) -> AfterProbe {
        let expires = worker.expires_at;
        let last = worker.last_active;
        let lifetime_end = self.lifetime_end(worker, now);
        match active {
            Some(true) => {
                let token_exp = worker.token_expires_at;
                let end = (now + self.grace_period).min(lifetime_end);
                AfterProbe::Busy {
                    record_activity: now - last > 300.0,
                    report_expired: last < expires,
                    renew_token_until: (token_exp != 0.0
                        && token_exp < now + 600.0
                        && end > token_exp)
                        .then_some(end),
                }
            }
            None if now < expires.max(last) + self.grace_period => AfterProbe::Keep,
            Some(false)
                if last >= expires && now < (last + self.grace_period).min(lifetime_end) =>
            {
                AfterProbe::Keep
            }
            _ => AfterProbe::DeleteExpired,
        }
    }
}
