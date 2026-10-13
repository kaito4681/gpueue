//! The minisign public key that signs puzuz releases, built into the binary from
//! `packaging/minisign.pub` (created by `scripts/dist/gen-signing-key.sh`). `scripts/install.sh`
//! carries the same key; a test keeps the two in sync, and the release workflow refuses the
//! test key that development builds carry.

/// The public key file: an `untrusted comment:` line and the base64 key.
// Used by `puzuz update` (next change); only the tests read it until then.
#[cfg_attr(not(test), allow(dead_code))]
pub const RELEASE_PUBKEY: &str = include_str!("../../../packaging/minisign.pub");

/// The base64 key line of [`RELEASE_PUBKEY`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn release_pubkey_base64() -> &'static str {
    RELEASE_PUBKEY
        .lines()
        .find(|line| !line.starts_with("untrusted comment:") && !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");

    #[test]
    fn install_sh_carries_the_same_key() {
        let key = release_pubkey_base64();
        assert!(key.starts_with("RW"), "{key}");
        assert!(
            INSTALL_SH
                .lines()
                .any(|line| line == format!("MINISIGN_PUBKEY=\"{key}\"")),
            "MINISIGN_PUBKEY in scripts/install.sh differs from packaging/minisign.pub"
        );
    }
}
