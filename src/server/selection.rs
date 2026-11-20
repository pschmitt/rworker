//! Selection and ownership checks shared by control-host mutations.

pub(super) struct Target<'a> {
    name: &'a str,
    pub prefix: &'a str,
    pool: bool,
}

impl<'a> Target<'a> {
    pub fn select(
        name: &'a str,
        adhoc_prefix: &'a str,
        pool_prefix: &'a str,
        allow_pool: bool,
    ) -> Result<Self, String> {
        let prefix = if name.starts_with(adhoc_prefix) {
            adhoc_prefix
        } else if allow_pool && name.starts_with(pool_prefix) {
            pool_prefix
        } else {
            return Err(format!("{name} is not an ad-hoc worker"));
        };
        Ok(Self {
            name,
            prefix,
            pool: prefix == pool_prefix,
        })
    }

    pub fn check(
        &self,
        owner: Option<&str>,
        caller: Option<&str>,
        force: bool,
    ) -> Result<(), String> {
        let name = self.name;
        if self.pool && !force {
            return Err(format!(
                "{name} is in nixbot's build pool; builds running on it fail (--force to delete it anyway)"
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
