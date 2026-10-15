//! User and group lookups: who is an administrator, and which primary group
//! a job owner's processes run with.

use std::collections::{HashMap, HashSet};

use gpueue_core::model::Uid;
use gpueue_protocol::helper::Gid;

/// Members of this group (plus root) are gpueue administrators.
pub const ADMIN_GROUP: &str = "gpueue-admin";

/// Account lookups, behind a trait so tests can inject users.
pub trait UserDb: Send + Sync + 'static {
    /// Root, or a member of [`ADMIN_GROUP`] (as primary or supplementary
    /// group).
    fn is_admin(&self, uid: Uid) -> bool;

    /// The user's primary group, or `None` for an unknown user.
    fn primary_gid(&self, uid: Uid) -> Option<Gid>;
}

/// Lookups through the system's NSS (`getpwuid_r`, `getgrnam_r`), so LDAP
/// and other directory users work as well as `/etc/passwd`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemUserDb;

impl UserDb for SystemUserDb {
    fn is_admin(&self, uid: Uid) -> bool {
        if uid.0 == 0 {
            return true;
        }
        let user = match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid.0)) {
            Ok(Some(user)) => user,
            _ => return false,
        };
        let group = match nix::unistd::Group::from_name(ADMIN_GROUP) {
            Ok(Some(group)) => group,
            _ => return false,
        };
        user.gid == group.gid || group.mem.contains(&user.name)
    }

    fn primary_gid(&self, uid: Uid) -> Option<Gid> {
        nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid.0))
            .ok()
            .flatten()
            .map(|user| Gid(user.gid.as_raw()))
    }
}

/// A fixed set of users, for tests. Unknown users get their uid as gid.
#[derive(Debug, Default, Clone)]
pub struct StaticUserDb {
    pub admins: HashSet<Uid>,
    pub gids: HashMap<Uid, Gid>,
}

impl StaticUserDb {
    pub fn with_admins(admins: impl IntoIterator<Item = u32>) -> Self {
        Self {
            admins: admins.into_iter().map(Uid).collect(),
            gids: HashMap::new(),
        }
    }
}

impl UserDb for StaticUserDb {
    fn is_admin(&self, uid: Uid) -> bool {
        uid.0 == 0 || self.admins.contains(&uid)
    }

    fn primary_gid(&self, uid: Uid) -> Option<Gid> {
        Some(self.gids.get(&uid).copied().unwrap_or(Gid(uid.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_always_admin() {
        assert!(SystemUserDb.is_admin(Uid(0)));
        assert!(StaticUserDb::default().is_admin(Uid(0)));
    }

    #[test]
    fn system_lookup_knows_root() {
        assert_eq!(SystemUserDb.primary_gid(Uid(0)), Some(Gid(0)));
        // A uid that surely has no account.
        assert_eq!(SystemUserDb.primary_gid(Uid(4_000_000_000)), None);
        assert!(!SystemUserDb.is_admin(Uid(4_000_000_000)));
    }

    #[test]
    fn static_db() {
        let db = StaticUserDb::with_admins([7]);
        assert!(db.is_admin(Uid(7)));
        assert!(!db.is_admin(Uid(8)));
        assert_eq!(db.primary_gid(Uid(8)), Some(Gid(8)));
    }
}
