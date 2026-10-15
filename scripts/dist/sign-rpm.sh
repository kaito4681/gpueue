#!/usr/bin/env bash
# GPG-signs .rpm files in place with rpmsign (the signature `rpm -K` and dnf
# check), after checking that the secret key belongs to KEYRING, then checks
# each signature against KEYRING alone. The release workflow runs it in a job
# of its own on a fresh runner, apart from the build job, whose dependency
# build scripts run as root: the signing key never meets them.
#
# Usage: scripts/dist/sign-rpm.sh KEY_FILE KEYRING RPM...
#   KEY_FILE  ASCII-armored GPG secret key; its passphrase in RPM_PASSPHRASE
#             (may be empty)
#   KEYRING   the public key the secret key must belong to
#             (packaging/apt/gpueue-archive-keyring.gpg)
# Needs rpmsign and rpm (Debian/Ubuntu: the rpm package) and gpg.
set -euo pipefail

die() {
    echo "sign-rpm: $*" >&2
    exit 1
}

key="${1:?usage: $0 KEY_FILE KEYRING RPM...}"
keyring="${2:?usage: $0 KEY_FILE KEYRING RPM...}"
shift 2
[ "$#" -gt 0 ] || die "no .rpm files given"
for tool in rpmsign rpm gpg gpgconf; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
done

work="$(mktemp -d)"
GNUPGHOME="$work/gnupg"
export GNUPGHOME
cleanup() {
    gpgconf --kill all >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT
mkdir -m 700 "$GNUPGHOME"

gpg --batch --quiet --import "$key" 2>/dev/null || die "cannot import $key"
fpr="$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" && fpr == "" { fpr = $10 } END { print fpr }')"
[ -n "$fpr" ] || die "$key holds no secret key"
fprs="$(gpg --batch --with-colons --import-options show-only --import "$keyring" 2>/dev/null |
    awk -F: '$1 == "fpr" { print $10 }')" || die "cannot read $keyring"
grep -qxF "$fpr" <<<"$fprs" || die "the key $fpr is not in $keyring"

(
    umask 077
    printf '%s' "${RPM_PASSPHRASE:-}" >"$work/passphrase"
)
# The public key alone, in a scratch rpm database, to check the signatures.
gpg --batch --armor --export "$fpr" >"$work/public.asc"
mkdir "$work/rpmdb"
rpm --dbpath "$work/rpmdb" --initdb
rpm --dbpath "$work/rpmdb" --import "$work/public.asc"

for file in "$@"; do
    [ -f "$file" ] || die "no such file: $file"
    # %__gpg is gpg2 on some distributions (Ubuntu's rpm), which may not exist.
    rpmsign --addsign \
        --define "__gpg $(command -v gpg)" \
        --define "_gpg_name $fpr" \
        --define "_gpg_path $GNUPGHOME" \
        --define "_gpg_digest_algo sha256" \
        --define "_gpg_sign_cmd_extra_args --batch --pinentry-mode loopback --passphrase-file $work/passphrase" \
        "$file" >/dev/null || die "cannot sign $file (wrong RPM_PASSPHRASE?)"
    out="$(rpm --dbpath "$work/rpmdb" -K "$file")" || die "$file: the signature does not verify: $out"
    case "$out" in
    *"digests signatures OK"*) echo "signed $file ($fpr)" ;;
    *) die "$file is not signed: $out" ;;
    esac
done
