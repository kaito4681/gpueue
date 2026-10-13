#!/usr/bin/env bash
# Creates the GPG key pair that signs the index (Release/InRelease) of the
# puzuz APT repository, once.
#
# - Writes the public key, in binary ("dearmored") form, to
#   packaging/apt/puzuz-archive-keyring.gpg, which the .deb installs as
#   /usr/share/keyrings/puzuz-archive-keyring.gpg (the Signed-By= of
#   /etc/apt/sources.list.d/puzuz.sources); commit it.
# - Writes the passphrase-protected secret key (ASCII-armored) to OUT_DIR
#   (default: a new directory under $HOME) and prints the `gh secret set`
#   commands that store it and its passphrase as the repository secrets
#   APT_GPG_PRIVATE_KEY and APT_GPG_PASSPHRASE, which the release workflow
#   signs the repository index with.
#
# Keep a backup of the secret key and its passphrase somewhere safe: a new key
# can only reach users through a package signed with the old one.
#
# Usage: scripts/dist/gen-apt-key.sh [OUT_DIR]
#        scripts/dist/gen-apt-key.sh --test
# --test writes a throwaway TEST key (its user ID says so, its secret key is
# discarded) to packaging/apt/ instead; the release workflow refuses it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
repo="kaito4681/puzuz"
keyring="$root/packaging/apt/puzuz-archive-keyring.gpg"
test_key=0
if [ "${1:-}" = --test ]; then
    test_key=1
    out=""
else
    out="${1:-$HOME/puzuz-apt-key-$(date +%Y%m%d)}"
fi

command -v gpg >/dev/null 2>&1 || { echo "gpg is needed (e.g. sudo apt install gnupg)" >&2; exit 1; }

GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
trap 'gpgconf --kill all >/dev/null 2>&1 || true; rm -rf "$GNUPGHOME"' EXIT

if [ "$test_key" = 1 ]; then
    uid="puzuz APT repository TEST KEY (not for releases; secret key discarded)"
    gpg --batch --quiet --pinentry-mode loopback --passphrase '' \
        --quick-gen-key "$uid" rsa4096 sign never
else
    if [ -e "$out/apt-signing-key.asc" ]; then
        echo "$out/apt-signing-key.asc already exists; not overwriting it" >&2
        exit 1
    fi
    uid="puzuz APT repository <https://github.com/$repo>"
    read -r -s -p "Passphrase for the secret key (it becomes the APT_GPG_PASSPHRASE secret): " pass
    echo
    read -r -s -p "Again: " pass2
    echo
    [ -n "$pass" ] && [ "$pass" = "$pass2" ] || { echo "empty or different passphrases" >&2; exit 1; }
    gpg --batch --quiet --pinentry-mode loopback --passphrase-fd 3 \
        --quick-gen-key "$uid" rsa4096 sign never 3<<<"$pass"
fi

fpr="$(gpg --batch --with-colons --list-keys "$uid" | awk -F: '$1 == "fpr" {print $10; exit}')"
[ -n "$fpr" ] || { echo "cannot find the new key" >&2; exit 1; }
mkdir -p "$(dirname "$keyring")"
gpg --batch --export --export-options export-minimal "$fpr" >"$keyring"

if [ "$test_key" = 1 ]; then
    echo "wrote the TEST key $fpr to $keyring"
    exit 0
fi

mkdir -p "$out"
chmod 700 "$out"
(
    umask 077
    gpg --batch --pinentry-mode loopback --passphrase-fd 3 --armor \
        --export-secret-keys "$fpr" >"$out/apt-signing-key.asc" 3<<<"$pass"
)
cp "$keyring" "$out/puzuz-archive-keyring.gpg"

cat <<EOF

Created the key $fpr:
  $out/apt-signing-key.asc         the secret key (passphrase-protected); back it up, never commit it
  $out/puzuz-archive-keyring.gpg   the public key, also written to:
  $keyring

Next steps:
  1. Store the secret key and its passphrase as repository secrets:
       gh secret set APT_GPG_PRIVATE_KEY -R $repo < "$out/apt-signing-key.asc"
       gh secret set APT_GPG_PASSPHRASE -R $repo     # paste the passphrase when asked
  2. Commit packaging/apt/puzuz-archive-keyring.gpg and merge it before tagging a
     release (the release workflow refuses the test key).
  3. Back up $out somewhere safe, then delete it from this machine.
EOF
