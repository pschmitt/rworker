//! Selection and ownership checks shared by control-host mutations.

use super::pools::Kind;

pub(super) struct Target<'a> {
    name: &'a str,
    /// The worker's pool, `None` for an ad-hoc worker.
    pool: Option<String>,
}

impl<'a> Target<'a> {
    /// A worker classified by its metadata (see `pools`). Pool workers are
    /// only selectable where `allow_pool` says so (`down --force`).
    pub fn new(name: &'a str, kind: Option<Kind>, allow_pool: bool) -> Result<Self, String> {
        match kind {
            Some(Kind::Adhoc) => Ok(Self { name, pool: None }),
            Some(Kind::Pool(pool)) if allow_pool => Ok(Self {
                name,
                pool: Some(pool),
            }),
            _ => Err(format!("{name} is not an ad-hoc worker")),
        }
    }

    pub fn check(
        &self,
        owner: Option<&str>,
        caller: Option<&str>,
        force: bool,
    ) -> Result<(), String> {
        let name = self.name;
        if let Some(pool) = &self.pool
            && !force
        {
            return Err(format!(
                "{name} is in the {pool} build pool; builds running on it fail (--force to delete it anyway)"
            ));
        }
        if let (Some(owner), Some(caller)) = (owner, caller)
            && !owner.is_empty()
            && !caller.is_empty()
            && owner != caller
            && !force
        {
            return Err(format!(
                "{name} belongs to {owner}, not {caller} (--force to override)"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_workers_need_allow_pool_and_force() {
        assert!(Target::new("x", Some(Kind::Pool("circus".into())), false).is_err());
        let target = Target::new("x", Some(Kind::Pool("circus".into())), true).unwrap();
        assert!(
            target
                .check(None, None, false)
                .unwrap_err()
                .contains("circus build pool")
        );
        assert!(target.check(None, None, true).is_ok());
    }

    #[test]
    fn unknown_servers_are_never_selected() {
        assert!(Target::new("rofl-15", None, true).is_err());
    }

    #[test]
    fn adhoc_owner_check() {
        let target = Target::new("x", Some(Kind::Adhoc), false).unwrap();
        assert!(target.check(Some("a"), Some("b"), false).is_err());
        assert!(target.check(Some("a"), Some("b"), true).is_ok());
        assert!(target.check(Some("a"), Some("a"), false).is_ok());
    }
}
