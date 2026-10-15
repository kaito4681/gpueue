#!/usr/bin/env bash
# Creates the minisign key pair that signs gpueue releases, once.
#
# - Writes the public key to packaging/minisign.pub (built into `gpueue` for
#   `gpueue update`) and into MINISIGN_PUBKEY in scripts/install.sh; commit both.
# - Writes the password-protected secret key to OUT_DIR (default: a new
#   directory under $HOME) and prints the `gh secret set` commands that store
#   it and its password as the repository secrets MINISIGN_SECRET_KEY and
#   MINISIGN_PASSWORD, which .github/workflows/release.yml signs with.
#
# Keep a backup of the secret key and its password somewhere safe (a password
# manager): losing them means users of `gpueue update` must reinstall with
# install.sh to get a build with the new public key.
#
# Usage: scripts/dist/gen-signing-key.sh [OUT_DIR]
# Uses `minisign` from PATH, or downloads a pinned one (scripts/dist/get-minisign.sh).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
repo="kaito4681/gpueue"
out="${1:-$HOME/gpueue-release-key-$(date +%Y%m%d)}"

if command -v minisign >/dev/null 2>&1; then
    minisign="$(command -v minisign)"
else
    minisign="$("$root/scripts/dist/get-minisign.sh" "$root/target/dist-tools")"
fi

if [ -e "$out/minisign.key" ]; then
    echo "$out/minisign.key already exists; not overwriting it" >&2
    exit 1
fi
mkdir -p "$out"
chmod 700 "$out"

echo "Choose a password for the secret key (it becomes the MINISIGN_PASSWORD secret)."
"$minisign" -G -p "$out/minisign.pub" -s "$out/minisign.key" -c "gpueue release signing key"

key_line="$(sed -n 2p "$out/minisign.pub")"
key_id="$(sed -n 's/^untrusted comment: minisign public key \([0-9A-F]*\)$/\1/p' "$out/minisign.pub")"
printf 'untrusted comment: gpueue release signing key %s\n%s\n' "$key_id" "$key_line" \
    >"$root/packaging/minisign.pub"
sed -i "s|^MINISIGN_PUBKEY=\".*\"$|MINISIGN_PUBKEY=\"$key_line\"|" "$root/scripts/install.sh"
grep -qF "MINISIGN_PUBKEY=\"$key_line\"" "$root/scripts/install.sh" ||
    { echo "failed to update MINISIGN_PUBKEY in scripts/install.sh" >&2; exit 1; }

cat <<EOF

Created:
  $out/minisign.key   the secret key (password-protected); back it up, never commit it
  $out/minisign.pub   the public key, also written to:
  $root/packaging/minisign.pub
  $root/scripts/install.sh (MINISIGN_PUBKEY)

Next steps:
  1. Store the secret key and its password as repository secrets:
       gh secret set MINISIGN_SECRET_KEY -R $repo < "$out/minisign.key"
       gh secret set MINISIGN_PASSWORD -R $repo     # paste the password when asked
  2. Commit packaging/minisign.pub and scripts/install.sh, and merge them before
     tagging a release (the release workflow refuses the test key).
  3. Back up $out somewhere safe, then delete it from this machine.
EOF
