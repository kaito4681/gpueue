//! Resolving job owners' user names.

use std::cell::RefCell;
use std::collections::HashMap;

use puzuz_core::model::Uid;

/// Caches uid → user name lookups. Unknown users are shown as their uid.
#[derive(Debug, Default)]
pub struct UserNames {
    /// Look users up in the system database (NSS); off in tests.
    system: bool,
    cache: RefCell<HashMap<Uid, String>>,
}

impl UserNames {
    pub fn system() -> Self {
        Self {
            system: true,
            cache: RefCell::default(),
        }
    }

    /// Fixed names only, no system lookups (for tests).
    #[cfg(test)]
    pub fn fixed(names: &[(u32, &str)]) -> Self {
        Self {
            system: false,
            cache: RefCell::new(
                names
                    .iter()
                    .map(|(uid, name)| (Uid(*uid), (*name).to_owned()))
                    .collect(),
            ),
        }
    }

    pub fn name(&self, uid: Uid) -> String {
        if let Some(name) = self.cache.borrow().get(&uid) {
            return name.clone();
        }
        let name = self
            .system
            .then(|| lookup(uid))
            .flatten()
            .unwrap_or_else(|| uid.to_string());
        self.cache.borrow_mut().insert(uid, name.clone());
        name
    }
}

fn lookup(uid: Uid) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid.0))
        .ok()
        .flatten()
        .map(|user| user.name)
}
