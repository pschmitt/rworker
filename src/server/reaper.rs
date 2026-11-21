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

/// With `reapProtectInUse` a worker is deleted only after it was positively
/// seen idle. That cannot apply to one that never became ready: it has no
/// host key, so no client can hold a pinned session on it and it cannot be
/// probed either. Counting it as "could be in use" would keep it forever.
// The reaper-parity example includes this file and only needs the policy.
#[allow(dead_code)]
pub(super) fn protected(
    decision: &BeforeProbe,
    protect_in_use: bool,
    active: Option<bool>,
) -> bool {
    protect_in_use && *decision != BeforeProbe::DeleteUnready && active != Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: Policy = Policy {
        boot_timeout: 900.0,
        grace_period: 3600.0,
        max_ttl: 86400.0,
    };

    fn worker(host_key_present: bool, created: f64, expires: f64) -> Worker {
        Worker {
            created_at: Some(created),
            expires_at: expires,
            max_lifetime: Some(86400.0),
            host_key_present,
            last_active: 0.0,
            token_expires_at: 0.0,
        }
    }

    #[test]
    fn a_worker_without_host_key_is_unready_after_the_boot_timeout() {
        let w = worker(false, 1000.0, 8200.0);
        assert_eq!(POLICY.before_probe(&w, 1500.0), BeforeProbe::Keep);
        assert_eq!(POLICY.before_probe(&w, 2000.0), BeforeProbe::DeleteUnready);
    }

    #[test]
    fn protect_in_use_never_keeps_an_unready_worker() {
        // No host key: activity is unknowable, yet nobody can be using it.
        assert!(!protected(&BeforeProbe::DeleteUnready, true, None));
    }

    #[test]
    fn protect_in_use_still_needs_proof_of_idleness_for_the_rest() {
        assert!(protected(&BeforeProbe::Probe, true, None));
        assert!(protected(&BeforeProbe::Probe, true, Some(true)));
        assert!(!protected(&BeforeProbe::Probe, true, Some(false)));
        assert!(protected(&BeforeProbe::DeleteAtMaximumLifetime, true, None));
    }

    #[test]
    fn without_protect_in_use_nothing_is_protected() {
        assert!(!protected(&BeforeProbe::Probe, false, None));
        assert!(!protected(&BeforeProbe::DeleteUnready, false, None));
    }
}
