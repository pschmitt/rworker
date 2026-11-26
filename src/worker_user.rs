//! The account a worker is logged into. Current images and leases have one,
//! `rworker`; a worker reports it (the image's Glance property
//! `rworker_login_user`, a lease's record), and older workers that report
//! nothing still have the account of their kind.

/// The login account of current workers of every kind.
pub const WORKER_USER: &str = "rworker";
/// The login account of OpenStack workers from images without `rworker_login_user`.
pub const LEGACY_OPENSTACK_USER: &str = "agent";
/// The login account of CI leases created before they had one recorded.
pub const LEGACY_LEASE_USER: &str = "rworker-build";

/// The login account a worker reports, or the one of its kind when it reports none.
pub fn login_user(reported: Option<&str>, lease: bool) -> &str {
    match reported.filter(|user| !user.is_empty()) {
        Some(user) => user,
        None if lease => LEGACY_LEASE_USER,
        None => LEGACY_OPENSTACK_USER,
    }
}

/// The home of a worker's login account, where per-user files (the Nix
/// daemon of a CI lease) live.
pub fn home(user: &str) -> String {
    format!("/home/{user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_without_a_reported_account_keep_their_kinds() {
        assert_eq!(login_user(Some("rworker"), false), "rworker");
        assert_eq!(login_user(Some("rworker"), true), "rworker");
        assert_eq!(login_user(None, false), "agent");
        assert_eq!(login_user(Some(""), true), "rworker-build");
        assert_eq!(home("rworker"), "/home/rworker");
    }
}
