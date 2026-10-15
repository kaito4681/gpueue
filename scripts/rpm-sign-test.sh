#!/usr/bin/env bash
# Tests scripts/dist/sign-rpm.sh, as the release workflow's sign-rpm job runs
# it (on Ubuntu, with the distribution's rpm): in an ubuntu:24.04 container,
# with a throwaway passphrase-protected GPG key, it refuses a wrong
# passphrase and a key that is not in the given keyring, then signs a copy of
# the .rpm; in Rocky Linux 9 and Fedora containers, `rpm -K` accepts the
# signature with the imported public key, and not without it or with
# another key.
#
# Usage: scripts/rpm-sign-test.sh RPM
# Environment:
#   GPUEUE_DEB_HTTPS=1  fetch Ubuntu packages over HTTPS, with this machine's CA
#                      certificates, where port 80 is blocked
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rpm="${1:?usage: $0 RPM}"
rpm="$(cd "$(dirname "$rpm")" && pwd)/$(basename "$rpm")"
[ -f "$rpm" ] || { echo "no such file: $rpm" >&2; exit 1; }
mkdir -p "$root/target"
work="$(mktemp -d "$root/target/rpm-sign-test.XXXXXX")"
cleanup() {
    # Files written by root in the containers.
    docker run --rm -v "$work:/out" ubuntu:24.04 chown -R "$(id -u):$(id -g)" /out >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT
cp "$rpm" "$work/test.rpm"

mounts=()
setup=""
if [ "${GPUEUE_DEB_HTTPS:-}" = 1 ]; then
    ca=/etc/ssl/certs/ca-certificates.crt
    [ -f "$ca" ] || { echo "GPUEUE_DEB_HTTPS=1 needs $ca" >&2; exit 1; }
    mounts+=(-v "$ca:/usr/local/share/host-ca.crt:ro")
    setup='sed -i "s|http://|https://|g" /etc/apt/sources.list.d/ubuntu.sources
echo "Acquire::https::CaInfo \"/usr/local/share/host-ca.crt\";" >/etc/apt/apt.conf.d/99host-ca'
fi

# The steps in the Ubuntu container, after installing rpm and gnupg.
cat >"$work/sign.sh" <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
sign=/src/scripts/dist/sign-rpm.sh
for k in release other; do
    export GNUPGHOME="/tmp/gnupg-$k"
    mkdir -m 700 "$GNUPGHOME"
    gpg --batch --quiet --pinentry-mode loopback --passphrase test-passphrase \
        --quick-gen-key "gpueue rpm-sign-test $k key" rsa3072 sign never
    gpg --batch --pinentry-mode loopback --passphrase test-passphrase --armor \
        --export-secret-keys >"/tmp/$k.asc"
    gpg --batch --export >"/tmp/$k.gpg"
    gpg --batch --armor --export >"/out/$k.pub.asc"
    gpgconf --kill all
done
unset GNUPGHOME
before="$(sha256sum </out/test.rpm)"
if RPM_PASSPHRASE=wrong bash "$sign" /tmp/release.asc /tmp/release.gpg /out/test.rpm 2>/tmp/err; then
    fail "signed with a wrong passphrase"
fi
grep -q 'cannot sign' /tmp/err || { cat /tmp/err; fail "a wrong passphrase failed for another reason"; }
echo "refused: a wrong passphrase"
if RPM_PASSPHRASE=test-passphrase bash "$sign" /tmp/release.asc /tmp/other.gpg /out/test.rpm 2>/tmp/err; then
    fail "signed with a key that is not in the keyring"
fi
grep -q 'is not in /tmp/other.gpg' /tmp/err || { cat /tmp/err; fail "another keyring failed for another reason"; }
echo "refused: a key that is not in the keyring"
[ "$(sha256sum </out/test.rpm)" = "$before" ] || fail "a refusal changed the .rpm"
RPM_PASSPHRASE=test-passphrase bash "$sign" /tmp/release.asc /tmp/release.gpg /out/test.rpm
EOF

echo "=== sign-rpm.sh (ubuntu:24.04)"
docker run --rm -i "${mounts[@]}" -v "$root:/src:ro" -v "$work:/out" ubuntu:24.04 bash -euo pipefail <<EOF
$setup
export DEBIAN_FRONTEND=noninteractive
apt-get update -q >/dev/null
apt-get install -y -q --no-install-recommends rpm gnupg >/dev/null 2>&1
bash -euo pipefail /out/sign.sh
EOF

for base in rockylinux/rockylinux:9 fedora:latest; do
    echo "=== rpm -K ($base)"
    docker run --rm -i -v "$work:/out:ro" "$base" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
if rpm -K /out/test.rpm >/tmp/k.out 2>&1; then cat /tmp/k.out; fail "verified without the key"; fi
rpm --import /out/other.pub.asc
if rpm -K /out/test.rpm >/tmp/k.out 2>&1; then cat /tmp/k.out; fail "verified with another key"; fi
rpm --import /out/release.pub.asc
out="$(rpm -K /out/test.rpm)" || fail "does not verify: $out"
case "$out" in *"digests signatures OK"*) echo "$out" ;; *) fail "not signed: $out" ;; esac
EOF
done
echo
echo "rpm signing test passed"
