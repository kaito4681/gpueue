//! The minisign public key that signs puzuz releases, built into the binary from
//! `packaging/minisign.pub` (created by `scripts/dist/gen-signing-key.sh`). `scripts/install.sh`
//! carries the same key; a test keeps the two in sync, and the release workflow refuses the
//! test key that development builds carry. `scripts/install.sh` also pins the minisign release it
//! downloads when minisign is not installed; another test keeps that pin equal to
//! `scripts/dist/get-minisign.sh`'s.

/// The public key file: an `untrusted comment:` line and the base64 key.
pub const RELEASE_PUBKEY: &str = include_str!("../../../packaging/minisign.pub");

/// The base64 key line of [`RELEASE_PUBKEY`].
#[cfg(test)]
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
    const GET_MINISIGN_SH: &str = include_str!("../../../scripts/dist/get-minisign.sh");

    /// The value of the top-level shell assignment `NAME=value` (quotes removed).
    fn assignment<'a>(script: &'a str, name: &str) -> Option<&'a str> {
        let prefix = format!("{name}=");
        script
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .map(|value| value.trim_matches('"'))
    }

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

    #[test]
    fn install_sh_pins_the_minisign_of_get_minisign_sh() {
        let pairs = [
            ("MINISIGN_VERSION", "version"),
            ("MINISIGN_SHA256", "sha256"),
        ];
        for (install_name, get_name) in pairs {
            let install = assignment(INSTALL_SH, install_name)
                .unwrap_or_else(|| panic!("no {install_name}= in scripts/install.sh"));
            let get = assignment(GET_MINISIGN_SH, get_name)
                .unwrap_or_else(|| panic!("no {get_name}= in scripts/dist/get-minisign.sh"));
            assert!(!install.is_empty());
            assert_eq!(
                install, get,
                "{install_name} in scripts/install.sh differs from {get_name} in \
                 scripts/dist/get-minisign.sh"
            );
        }
        let sha256 = assignment(INSTALL_SH, "MINISIGN_SHA256").unwrap();
        assert!(
            sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "{sha256}"
        );
        let install_url = assignment(INSTALL_SH, "MINISIGN_URL").expect("MINISIGN_URL");
        let get_url = assignment(GET_MINISIGN_SH, "url").expect("url");
        assert_eq!(
            install_url.replace("$MINISIGN_VERSION", "$version"),
            get_url,
            "MINISIGN_URL in scripts/install.sh differs from url in scripts/dist/get-minisign.sh"
        );
    }
}
