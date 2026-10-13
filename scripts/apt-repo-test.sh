#!/usr/bin/env bash
# APT repository test (scenario 19): builds a flat repository with
# scripts/dist/apt-repo.sh and a throwaway GPG key, serves it over HTTP on a
# Docker network, and in a plain container (no systemd as PID 1) per
# distribution:
#   - installs v1 of the .deb with `apt-get install ./…`, points its
#     puzuz.sources at the test repository (and the test key) and checks that
#     `apt-get update` verifies the index (and refuses it with another key);
#   - publishes v2, then `apt-get update && apt-get upgrade` installs it,
#     keeping the edited config.toml and puzuz.sources; `puzuz update` refuses;
#   - `apt-get remove` keeps the conffiles, `apt-get purge` deletes them and
#     keeps /var/lib/puzuz;
#   - installing from the repository with `apt-get install puzuz` works.
# It also checks apt-repo.sh itself: keeping the newest versions in Debian
# version order, Filename: being the file name (0.2.0-rc.1 in the name,
# 0.2.0~rc.1 in the package), and its refusals.
#
# v1 and v2 are the same package with the versions 9.9.9-rc.1 (9.9.9~rc.1 in
# the package) and 9.9.9 written into it, so only one build is needed.
#
# Usage: scripts/apt-repo-test.sh [DEB]
#   DEB: the package to start from (default: build one from the current tree,
#        like scripts/deb-test.sh)
# Environment:
#   PUZUZ_APT_BASES  distributions to test (default "debian:12 ubuntu:24.04";
#                    also supported: ubuntu:22.04 ubuntu:26.04)
#   PUZUZ_DEB_HTTPS=1  fetch distribution packages over HTTPS, with this
#                    machine's CA certificates, where port 80 is blocked
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
read -r -a bases <<<"${PUZUZ_APT_BASES:-debian:12 ubuntu:24.04}"
deb="${1:-}"
tools_image=debian:12

if [ -z "$deb" ]; then
    "$root/scripts/test-in-docker.sh" build --locked -p puzuz -p puzuz-helper
    deb="$("$root/scripts/dist/build-package.sh" "$root/target/docker/debug" "$root/target/apt-repo-test-deb")"
fi
deb="$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")"
[ -f "$deb" ] || { echo "no such file: $deb" >&2; exit 1; }

mkdir -p "$root/target"
work="$(mktemp -d "$root/target/apt-repo-test.XXXXXX")"
id="$$"
net="puzuz-apt-test-$id"
server="puzuz-apt-repo-$id"
client="puzuz-apt-client-$id"

cleanup() {
    # Files written by root in the containers.
    docker exec "$server" chown -R "$(id -u):$(id -g)" /w >/dev/null 2>&1 || true
    docker rm -f "$server" "$client" >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

mounts=()
setup=""
if [ "${PUZUZ_DEB_HTTPS:-}" = 1 ]; then
    ca=/etc/ssl/certs/ca-certificates.crt
    [ -f "$ca" ] || { echo "PUZUZ_DEB_HTTPS=1 needs $ca" >&2; exit 1; }
    mounts+=(-v "$ca:/usr/local/share/host-ca.crt:ro")
    # Only the distribution's sources: the test repository is plain HTTP.
    setup='sed -i "s|http://|https://|g" /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/ubuntu.sources /etc/apt/sources.list.d/debian.sources 2>/dev/null || true
echo "Acquire::https::CaInfo \"/usr/local/share/host-ca.crt\";" >/etc/apt/apt.conf.d/99host-ca'
fi

docker network create "$net" >/dev/null
mkdir -p "$work/pkg"
cp "$deb" "$work/pkg/orig.deb"

# The repository side: tools, the throwaway key, apt-repo.sh, the HTTP server.
docker run -d --name "$server" --network "$net" --network-alias repo "${mounts[@]}" \
    -v "$work:/w" -v "$root:/src:ro" "$tools_image" sleep infinity >/dev/null
docker exec -i "$server" bash -euo pipefail <<EOF
$setup
export DEBIAN_FRONTEND=noninteractive
apt-get update -q >/dev/null
apt-get install -y -q --no-install-recommends apt-utils gnupg python3 >/dev/null
EOF

echo "=== apt-repo.sh"
docker exec -i "$server" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
cd /w
export GNUPGHOME=/w/gnupg
mkdir -m 700 "$GNUPGHOME"
gpg --batch --quiet --pinentry-mode loopback --passphrase test-passphrase \
    --quick-gen-key "puzuz APT repository (apt-repo-test throwaway key)" rsa3072 sign never
gpg --batch --pinentry-mode loopback --passphrase test-passphrase --armor --export-secret-keys >key.asc
gpg --batch --export >test-keyring.gpg
gpgconf --kill all
unset GNUPGHOME
export APT_GPG_PRIVATE_KEY="$(cat key.asc)" APT_GPG_PASSPHRASE=test-passphrase APT_KEYRING=/w/test-keyring.gpg
repo=/src/scripts/dist/apt-repo.sh

# The same package with another version written into it, under the release
# asset name puzuz_<tag version>_<arch>.deb.
reversion() { # SRC TAG_VERSION PACKAGE_VERSION OUT_DIR
    local x arch
    x="$(mktemp -d)"
    dpkg-deb -R "$1" "$x"
    sed -i "s/^Version: .*/Version: $3/" "$x/DEBIAN/control"
    arch="$(dpkg-deb --field "$1" Architecture)"
    mkdir -p "$4"
    dpkg-deb --root-owner-group -b "$x" "$4/puzuz_$2_$arch.deb" >/dev/null
    rm -rf "$x"
}
reversion pkg/orig.deb 9.9.9-rc.1 '9.9.9~rc.1' debs
reversion pkg/orig.deb 9.9.9 9.9.9 debs
arch="$(dpkg-deb --field pkg/orig.deb Architecture)"
mv "debs/puzuz_9.9.9-rc.1_$arch.deb" debs/v1.deb
mv "debs/puzuz_9.9.9_$arch.deb" debs/v2.deb
cp debs/v1.deb "debs/puzuz_9.9.9-rc.1_$arch.deb"
cp debs/v2.deb "debs/puzuz_9.9.9_$arch.deb"

# Pruning: tiny packages of both architectures, newest five per architecture.
mkpkg() { # VERSION(tag) ARCH
    local x
    x="$(mktemp -d)"
    mkdir -p "$x/DEBIAN" "$x/etc/apt/sources.list.d"
    cp /src/packaging/apt/puzuz.sources "$x/etc/apt/sources.list.d/"
    printf 'Package: puzuz\nVersion: %s\nArchitecture: %s\nMaintainer: test <t@example.com>\nDescription: test\n' \
        "${1/-/\~}" "$2" >"$x/DEBIAN/control"
    dpkg-deb --root-owner-group -b "$x" "fake/puzuz_$1_$2.deb" >/dev/null
    rm -rf "$x"
}
mkdir fake
for v in 0.1.0 0.2.0 0.9.0 0.10.0-rc.1 0.10.0 1.0.0 1.0.1; do mkpkg "$v" amd64; done
for v in 0.9.0 1.0.0; do mkpkg "$v" arm64; done
PUZUZ_APT_KEEP=5 "$repo" update prune fake/puzuz_0.1.0_amd64.deb fake/puzuz_0.10.0_amd64.deb fake/puzuz_0.9.0_arm64.deb
PUZUZ_APT_KEEP=5 "$repo" update prune fake/*.deb
expected="puzuz_0.10.0-rc.1_amd64.deb puzuz_0.10.0_amd64.deb puzuz_0.9.0_amd64.deb puzuz_0.9.0_arm64.deb puzuz_1.0.0_amd64.deb puzuz_1.0.0_arm64.deb puzuz_1.0.1_amd64.deb"
[ "$(cd prune && echo puzuz_*.deb)" = "$expected" ] || fail "kept $(cd prune && echo puzuz_*.deb)"
grep -qx 'Filename: puzuz_0.10.0-rc.1_amd64.deb' prune/Packages || fail "Filename: of the pre-release"
grep -qx 'Version: 0.10.0~rc.1' prune/Packages || fail "Version: of the pre-release"
[ "$(grep -c '^Package: puzuz$' prune/Packages)" = 7 ] || fail "Packages does not list 7 packages"
grep -q '^Filename: \./' prune/Packages && fail "Filename: is not the bare file name"
zcat prune/Packages.gz | cmp -s - prune/Packages || fail "Packages.gz differs from Packages"
grep -qx 'Origin: puzuz' prune/Release || fail "no Origin"
grep -qx 'Architectures: amd64 arm64' prune/Release || fail "no Architectures"
grep -Eq '^(Suite|Codename):' prune/Release && fail "Release has a Suite or Codename"
grep -q ' Packages.gz$' prune/Release || fail "Release does not list Packages.gz"
gpgv --quiet --keyring /w/test-keyring.gpg prune/InRelease 2>/dev/null || fail "InRelease"
gpgv --quiet --keyring /w/test-keyring.gpg prune/Release.gpg prune/Release 2>/dev/null || fail "Release.gpg"
cmp -s prune/puzuz.gpg test-keyring.gpg || fail "puzuz.gpg is not the keyring"
cmp -s prune/puzuz.sources /src/packaging/apt/puzuz.sources || fail "puzuz.sources"
echo "pruning and the index: ok"

# Refusals.
if APT_GPG_PASSPHRASE=wrong "$repo" update prune 2>/dev/null; then fail "signed with a wrong passphrase"; fi
if APT_KEYRING=/src/packaging/apt/puzuz-archive-keyring.gpg "$repo" update prune 2>/dev/null; then
    fail "signed with a key the keyring does not have"
fi
mkdir -p clash && cp fake/puzuz_1.0.0_arm64.deb clash/puzuz_1.0.1_amd64.deb
if "$repo" update prune clash/puzuz_1.0.1_amd64.deb 2>/dev/null; then fail "accepted a name/contents mismatch"; fi
cp fake/puzuz_1.0.0_amd64.deb clash/puzuz_1.0.2_amd64.deb
if "$repo" update prune clash/puzuz_1.0.2_amd64.deb 2>/dev/null; then fail "accepted a file name that is not its version"; fi
echo "refusals: ok"

# The test repository: v1 for now.
"$repo" update state "debs/puzuz_9.9.9-rc.1_$arch.deb"
mkdir serve && cp state/* serve/
EOF
docker exec -d "$server" sh -c 'cd /w/serve && exec python3 -m http.server 8000 >/w/http.log 2>&1'

publish_v2() {
    docker exec -i "$server" bash -euo pipefail <<'EOF'
cd /w
export APT_GPG_PRIVATE_KEY="$(cat key.asc)" APT_GPG_PASSPHRASE=test-passphrase APT_KEYRING=/w/test-keyring.gpg
/src/scripts/dist/apt-repo.sh update state debs/puzuz_9.9.9_*.deb
# Packages first, the index last, as the workflow uploads them.
cp state/puzuz_*.deb serve/
for f in puzuz.gpg puzuz.sources Packages Packages.gz Release Release.gpg InRelease; do cp "state/$f" serve/; done
EOF
}

for base in "${bases[@]}"; do
    echo
    echo "=== $base"
    docker rm -f "$client" >/dev/null 2>&1 || true
    docker run -d --name "$client" --network "$net" "${mounts[@]}" \
        -v "$work/debs:/debs:ro" -v "$work/test-keyring.gpg:/test-keyring.gpg:ro" \
        "$base" sleep infinity >/dev/null
    docker exec -i "$client" bash -euo pipefail <<EOF
$setup
export DEBIAN_FRONTEND=noninteractive
apt-get update -q >/dev/null
EOF
    docker exec -i "$client" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive
status() { dpkg-query -W -f='${Status} ${Version}' puzuz 2>/dev/null || true; }
sources=/etc/apt/sources.list.d/puzuz.sources

step "apt-get install ./v1 (9.9.9-rc.1)"
apt-get install -y -q /debs/v1.deb >/dev/null
[ "$(status)" = "install ok installed 9.9.9~rc.1" ] || fail "status: $(status)"
grep -qx 'URIs: https://github.com/kaito4681/puzuz/releases/download/apt/' "$sources" || fail "packaged URIs"

step "apt-get update refuses the index signed by another key"
sed -i 's|^URIs:.*|URIs: http://repo:8000/|' "$sources"
if apt-get update -q >/tmp/update.out 2>&1 && ! grep -q 'repo:8000' /tmp/update.out; then
    cat /tmp/update.out; fail "no error"
fi
grep -Eq 'NO_PUBKEY|not signed|signatures couldn.t be verified' /tmp/update.out || { cat /tmp/update.out; fail "no signature error"; }
apt-cache madison puzuz | grep -q 'repo:8000' && fail "apt uses the unverified index"

step "apt-get update verifies the index with the right key"
install -D -m 0644 /test-keyring.gpg /etc/apt/keyrings/puzuz-test.gpg
sed -i 's|^Signed-By:.*|Signed-By: /etc/apt/keyrings/puzuz-test.gpg|' "$sources"
echo '# edited by apt-repo-test' >>"$sources"
echo '# edited by apt-repo-test' >>/etc/puzuz/config.toml
apt-get update -q 2>&1 | tee /tmp/update.out
grep -E '^(W|E|Err):' /tmp/update.out && fail "apt-get update reported a problem"
grep -q 'http://repo:8000 ./ InRelease' /tmp/update.out || fail "InRelease was not fetched"
apt-cache madison puzuz | grep -q '9.9.9~rc.1 | *http://repo:8000' || { apt-cache madison puzuz; fail "v1 not in the repository"; }
apt-get -s upgrade | grep -q '^Inst puzuz' && fail "an upgrade before v2 is published"
true
EOF

    publish_v2

    docker exec -i "$client" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive
status() { dpkg-query -W -f='${Status} ${Version}' puzuz 2>/dev/null || true; }
sources=/etc/apt/sources.list.d/puzuz.sources

step "apt-get update && apt-get upgrade installs v2 (9.9.9)"
apt-get update -q 2>&1 | tee /tmp/update.out
grep -E '^(W|E|Err):' /tmp/update.out && fail "apt-get update reported a problem"
apt-get upgrade -y -q </dev/null
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
grep -q 'edited by apt-repo-test' /etc/puzuz/config.toml || fail "config.toml edit lost"
grep -q 'edited by apt-repo-test' "$sources" || fail "puzuz.sources edit lost"
grep -qx 'URIs: http://repo:8000/' "$sources" || fail "puzuz.sources URIs lost"
[ ! -e "$sources.dpkg-dist" ] && [ ! -e "$sources.dpkg-new" ] || fail "dpkg left a new puzuz.sources"
if puzuz update >/tmp/pu.out 2>&1; then fail "puzuz update did not refuse"; fi
grep -q 'apt upgrade' /tmp/pu.out || { cat /tmp/pu.out; fail "puzuz update does not point to apt"; }

step "apt-get remove keeps the configuration"
apt-get remove -y -q puzuz >/dev/null
[ "$(status)" = "deinstall ok config-files 9.9.9" ] || fail "status: $(status)"
[ -e /etc/puzuz/config.toml ] && [ -e "$sources" ] || fail "conffiles removed"
[ ! -e /usr/bin/puzuz ] || fail "/usr/bin/puzuz still there"
apt-get update -q 2>&1 | grep -E '^(W|E|Err):' && fail "apt-get update after remove"

step "apt-get purge deletes it, keeps /var/lib/puzuz"
apt-get purge -y -q puzuz >/dev/null
[ ! -e /etc/puzuz ] && [ ! -e "$sources" ] || fail "purge left /etc/puzuz or puzuz.sources"
[ -d /var/lib/puzuz ] || fail "/var/lib/puzuz deleted"
[ -z "$(status)" ] || [ "$(status)" = "unknown ok not-installed " ] || fail "status after purge: $(status)"

step "the repository added by hand: apt-get install puzuz"
/usr/lib/apt/apt-helper download-file http://repo:8000/puzuz.gpg /tmp/puzuz.gpg >/dev/null
/usr/lib/apt/apt-helper download-file http://repo:8000/puzuz.sources /tmp/puzuz.sources >/dev/null
cmp -s /tmp/puzuz.gpg /test-keyring.gpg || fail "puzuz.gpg"
grep -qx 'Signed-By: /usr/share/keyrings/puzuz-archive-keyring.gpg' /tmp/puzuz.sources || fail "puzuz.sources"
printf 'Types: deb\nURIs: http://repo:8000/\nSuites: ./\nSigned-By: /etc/apt/keyrings/puzuz-test.gpg\n' >"$sources"
apt-get update -q >/dev/null
apt-get install -y -q -o Dpkg::Options::=--force-confold puzuz </dev/null >/dev/null
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
echo
echo "all APT repository checks passed"
EOF
    # Back to v1 for the next distribution.
    docker exec "$server" bash -euo pipefail -c '
cd /w && rm -rf state serve/*
export APT_GPG_PRIVATE_KEY="$(cat key.asc)" APT_GPG_PASSPHRASE=test-passphrase APT_KEYRING=/w/test-keyring.gpg
/src/scripts/dist/apt-repo.sh update state debs/puzuz_9.9.9-rc.1_*.deb 2>/dev/null
cp state/* serve/'
done

echo
echo "requests to the test repository:"
docker exec "$server" sh -c "grep -o '\"GET [^\"]*\" [0-9]*' /w/http.log | sort | uniq -c"
echo
echo "APT repository test passed on: ${bases[*]}"
