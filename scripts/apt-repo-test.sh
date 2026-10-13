#!/usr/bin/env bash
# APT repository test (scenario 19): builds a flat repository with
# scripts/dist/apt-repo.sh and a throwaway GPG key, serves it over HTTP on a
# Docker network, and in a plain container (no systemd as PID 1) per
# distribution:
#   - installs v1 of the .deb with `apt-get install ./…`; its own
#     puzuz.sources and archive keyring are used as they are, and
#     `apt-get update` verifies the index (and refuses another key);
#   - publishes v2, then `apt-get update && apt-get upgrade` installs it,
#     keeping the edited config.toml and puzuz.sources; `puzuz update` refuses;
#   - `apt-get remove` keeps the conffiles (the source and its keyring:
#     `apt-get update` still works), `apt-get purge` deletes them and keeps
#     /var/lib/puzuz;
#   - adding the repository by hand with the published puzuz.gpg and
#     puzuz.sources, then `apt-get install puzuz`, works without a conffile
#     prompt.
# It also checks apt-repo.sh itself: keeping the newest versions in Debian
# version order, Filename: being the file name (0.2.0-rc.1 in the name,
# 0.2.0~rc.1 in the package), and its refusals, which leave the directory
# untouched.
#
# v1 and v2 are the package given (or built) with the versions 9.9.9-rc.1
# (9.9.9~rc.1 in the package) and 9.9.9 written into it, and with the
# throwaway key as its archive keyring and the test repository's URL in its
# puzuz.sources (as a real release has the real key and the GitHub URL), so
# only one build is needed.
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

# Sourced by the server-side steps.
cat >"$work/env.sh" <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
export APT_GPG_PRIVATE_KEY="$(cat /w/key.asc)" APT_GPG_PASSPHRASE=test-passphrase
repo=/src/scripts/dist/apt-repo.sh
cd /w
EOF

echo "=== apt-repo.sh"
docker exec -i "$server" bash -euo pipefail <<'EOF'
export GNUPGHOME=/w/gnupg
mkdir -m 700 "$GNUPGHOME"
gpg --batch --quiet --pinentry-mode loopback --passphrase test-passphrase \
    --quick-gen-key "puzuz APT repository (apt-repo-test throwaway key)" rsa3072 sign never
gpg --batch --pinentry-mode loopback --passphrase test-passphrase --armor --export-secret-keys >/w/key.asc
gpg --batch --export >/w/test-keyring.gpg
gpgconf --kill all
unset GNUPGHOME
. /w/env.sh

# The package with another version, the test key and the test repository's
# URL, under the release asset name puzuz_<tag version>_<arch>.deb.
testpkg() { # SRC TAG_VERSION OUT_DIR
    local x arch
    x="$(mktemp -d)"
    dpkg-deb -R "$1" "$x"
    sed -i "s/^Version: .*/Version: ${2/-/\~}/" "$x/DEBIAN/control"
    cp /w/test-keyring.gpg "$x/etc/apt/keyrings/puzuz.gpg"
    sed -i 's|^URIs:.*|URIs: http://repo:8000/|' "$x/etc/apt/sources.list.d/puzuz.sources"
    (cd "$x" && md5sum etc/apt/keyrings/puzuz.gpg etc/apt/sources.list.d/puzuz.sources) >"$x/md5.new"
    grep -v -e ' etc/apt/keyrings/puzuz.gpg$' -e ' etc/apt/sources.list.d/puzuz.sources$' \
        "$x/DEBIAN/md5sums" >>"$x/md5.new" || true
    mv "$x/md5.new" "$x/DEBIAN/md5sums"
    arch="$(dpkg-deb --field "$1" Architecture)"
    mkdir -p "$3"
    dpkg-deb --root-owner-group -b "$x" "$3/puzuz_$2_$arch.deb" >/dev/null
    rm -rf "$x"
}
testpkg pkg/orig.deb 9.9.9-rc.1 debs
testpkg pkg/orig.deb 9.9.9 debs
arch="$(dpkg-deb --field pkg/orig.deb Architecture)"
cp "debs/puzuz_9.9.9-rc.1_$arch.deb" debs/v1.deb
cp "debs/puzuz_9.9.9_$arch.deb" debs/v2.deb

# Pruning: tiny packages of both architectures, newest five per architecture.
mkpkg() { # VERSION(tag) ARCH [OUT_DIR]
    local x
    x="$(mktemp -d)"
    mkdir -p "$x/DEBIAN" "$x/etc/apt/sources.list.d" "$x/etc/apt/keyrings"
    cp /src/packaging/apt/puzuz.sources "$x/etc/apt/sources.list.d/"
    cp /w/test-keyring.gpg "$x/etc/apt/keyrings/puzuz.gpg"
    printf 'Package: puzuz\nVersion: %s\nArchitecture: %s\nMaintainer: test <t@example.com>\nDescription: test\n' \
        "${1/-/\~}" "$2" >"$x/DEBIAN/control"
    mkdir -p "${3:-fake}"
    dpkg-deb --root-owner-group -b "$x" "${3:-fake}/puzuz_$1_$2.deb" >/dev/null
    rm -rf "$x"
}
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
cmp -s prune/puzuz.gpg test-keyring.gpg || fail "puzuz.gpg is not the packages' keyring"
cmp -s prune/puzuz.sources /src/packaging/apt/puzuz.sources || fail "puzuz.sources"
echo "pruning and the index: ok"

# puzuz.gpg and puzuz.sources come from the newest kept package, also when
# an older version is (re)published.
mkdir -p older
cp /src/packaging/apt/puzuz.sources older/s && echo '# older' >>older/s
x="$(mktemp -d)" && dpkg-deb -R fake/puzuz_0.9.0_amd64.deb "$x" && cp older/s "$x/etc/apt/sources.list.d/puzuz.sources"
sed -i 's/^Version: .*/Version: 0.9.5/' "$x/DEBIAN/control"
dpkg-deb --root-owner-group -b "$x" older/puzuz_0.9.5_amd64.deb >/dev/null && rm -rf "$x"
cp -r prune prune2
PUZUZ_APT_KEEP=10 "$repo" update prune2 older/puzuz_0.9.5_amd64.deb 2>/dev/null
cmp -s prune2/puzuz.sources /src/packaging/apt/puzuz.sources || fail "puzuz.sources taken from the older package"
echo "keyring and sources from the newest package: ok"

# Refusals, each leaving the directory as it was.
snapshot() { (cd prune && sha256sum ./*); }
snapshot >before.sum
refused() { # DESCRIPTION, then the command
    local what="$1"
    shift
    if "$@" 2>/tmp/refused.err; then fail "accepted: $what"; fi
    snapshot | cmp -s - before.sum || fail "the directory changed after: $what"
    echo "refused ($what): $(tail -n 1 /tmp/refused.err)"
}
refused "wrong passphrase" env APT_GPG_PASSPHRASE=wrong "$repo" update prune
refused "signing key not in the keyring" env APT_KEYRING=/src/packaging/apt/puzuz-archive-keyring.gpg "$repo" update prune
mkdir -p bad
cp fake/puzuz_1.0.0_arm64.deb bad/puzuz_1.0.1_amd64.deb
refused "same name, other contents" "$repo" update prune bad/puzuz_1.0.1_amd64.deb
cp fake/puzuz_1.0.0_amd64.deb bad/puzuz_1.0.2_amd64.deb
refused "file name is not the version" "$repo" update prune bad/puzuz_1.0.2_amd64.deb
echo 'not a package' >bad/puzuz_2.0.0_amd64.deb
refused "not a .deb" "$repo" update prune bad/puzuz_2.0.0_amd64.deb
mkpkg 3.0.0 amd64 bad
truncate -s "$(($(stat -c %s bad/puzuz_3.0.0_amd64.deb) - 100))" bad/puzuz_3.0.0_amd64.deb
refused "truncated .deb" "$repo" update prune bad/puzuz_3.0.0_amd64.deb
mkpkg 1.0.01 amd64 bad
refused "duplicate version (1.0.01 = 1.0.1)" "$repo" update prune bad/puzuz_1.0.01_amd64.deb
# The package as built from the tree ships the committed TEST key.
testkey="bad/puzuz_$(dpkg-deb --field pkg/orig.deb Version | tr '~' '-')_$arch.deb"
cp pkg/orig.deb "$testkey"
if "$repo" update testkey "$testkey" 2>/tmp/refused.err; then fail "accepted: packages with the TEST key"; fi
[ ! -e testkey ] || fail "wrote testkey/ for packages with the TEST key"
echo "refused (TEST key): $(tail -n 1 /tmp/refused.err)"
echo "refusals: ok"

# The test repository: v1 for now.
"$repo" update state "debs/puzuz_9.9.9-rc.1_$arch.deb"
mkdir serve && cp state/* serve/
EOF
docker exec -d "$server" sh -c 'cd /w/serve && exec python3 -m http.server 8000 >/w/http.log 2>&1'

publish_v2() {
    docker exec -i "$server" bash -euo pipefail <<'EOF'
. /w/env.sh
"$repo" update state debs/puzuz_9.9.9_*.deb
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
        -v "$work/debs:/debs:ro" -v "$root/packaging/apt/puzuz-archive-keyring.gpg:/other-keyring.gpg:ro" \
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
# Buffered: `apt-cache … | grep -q` can fail with SIGPIPE under pipefail.
madison() { local out; out="$(apt-cache madison puzuz)"; printf '%s\n' "$out"; }
clean_update() {
    apt-get update -q 2>&1 | tee /tmp/update.out
    if grep -E '^(W|E|Err):' /tmp/update.out; then fail "apt-get update reported a problem"; fi
    grep -q 'http://repo:8000 ./ InRelease' /tmp/update.out || fail "InRelease was not fetched"
}
sources=/etc/apt/sources.list.d/puzuz.sources
keyring=/etc/apt/keyrings/puzuz.gpg

step "apt-get install ./v1 (9.9.9-rc.1)"
apt-get install -y -q /debs/v1.deb >/dev/null
[ "$(status)" = "install ok installed 9.9.9~rc.1" ] || fail "status: $(status)"
grep -qx "Signed-By: $keyring" "$sources" || fail "Signed-By is not $keyring"

step "apt-get update refuses the index with another key"
cp -p "$keyring" /tmp/keyring.orig
cp /other-keyring.gpg "$keyring"
if apt-get update -q >/tmp/update.out 2>&1 && ! grep -q 'repo:8000' /tmp/update.out; then
    cat /tmp/update.out; fail "no error"
fi
grep -Eq 'NO_PUBKEY|not signed|signatures couldn.t be verified' /tmp/update.out || { cat /tmp/update.out; fail "no signature error"; }
madison | grep -q 'repo:8000' && fail "apt uses the unverified index"
cp -p /tmp/keyring.orig "$keyring"

step "apt-get update verifies the index with the package's own files"
echo '# edited by apt-repo-test' >>"$sources"
echo '# edited by apt-repo-test' >>/etc/puzuz/config.toml
clean_update
madison | grep -q '9.9.9~rc.1 | *http://repo:8000' || { madison; fail "v1 not in the repository"; }
if apt-get -s upgrade | grep '^Inst puzuz'; then fail "an upgrade before v2 is published"; fi
EOF

    publish_v2

    docker exec -i "$client" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive
status() { dpkg-query -W -f='${Status} ${Version}' puzuz 2>/dev/null || true; }
# Buffered: `apt-cache … | grep -q` can fail with SIGPIPE under pipefail.
madison() { local out; out="$(apt-cache madison puzuz)"; printf '%s\n' "$out"; }
clean_update() {
    apt-get update -q 2>&1 | tee /tmp/update.out
    if grep -E '^(W|E|Err):' /tmp/update.out; then fail "apt-get update reported a problem"; fi
    grep -q 'http://repo:8000 ./ InRelease' /tmp/update.out || fail "InRelease was not fetched"
}
no_conffile_prompt() {
    if grep -Ei "configuration file|conffile" "$1"; then fail "dpkg asked about a conffile"; fi
    if ls /etc/apt/sources.list.d/*.dpkg-* /etc/apt/keyrings/*.dpkg-* /etc/puzuz/*.dpkg-* 2>/dev/null; then
        fail "dpkg left a .dpkg-* file"
    fi
}
sources=/etc/apt/sources.list.d/puzuz.sources
keyring=/etc/apt/keyrings/puzuz.gpg

step "apt-get update && apt-get upgrade installs v2 (9.9.9)"
clean_update
apt-get upgrade -y -q </dev/null 2>&1 | tee /tmp/upgrade.out
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
grep -q 'edited by apt-repo-test' /etc/puzuz/config.toml || fail "config.toml edit lost"
grep -q 'edited by apt-repo-test' "$sources" || fail "puzuz.sources edit lost"
no_conffile_prompt /tmp/upgrade.out
if puzuz update >/tmp/pu.out 2>&1; then fail "puzuz update did not refuse"; fi
grep -q 'apt upgrade' /tmp/pu.out || { cat /tmp/pu.out; fail "puzuz update does not point to apt"; }

step "apt-get remove keeps the source and its keyring"
apt-get remove -y -q puzuz >/dev/null
[ "$(status)" = "deinstall ok config-files 9.9.9" ] || fail "status: $(status)"
[ -e /etc/puzuz/config.toml ] && [ -e "$sources" ] && [ -e "$keyring" ] || fail "conffiles removed"
[ ! -e /usr/bin/puzuz ] || fail "/usr/bin/puzuz still there"
clean_update
madison | grep -q '9.9.9 | *http://repo:8000' || fail "the repository is gone after remove"

step "apt-get purge deletes the source and the keyring, keeps /var/lib/puzuz"
apt-get purge -y -q puzuz >/dev/null
[ ! -e /etc/puzuz ] || fail "purge left /etc/puzuz"
[ ! -e "$sources" ] && [ ! -e "$keyring" ] || fail "purge left puzuz.sources or the keyring"
[ -d /var/lib/puzuz ] || fail "/var/lib/puzuz deleted"
[ -z "$(status)" ] || [ "$(status)" = "unknown ok not-installed " ] || fail "status after purge: $(status)"

step "the repository added by hand (published puzuz.gpg and puzuz.sources): apt-get install puzuz"
mkdir -p /etc/apt/keyrings
/usr/lib/apt/apt-helper download-file http://repo:8000/puzuz.gpg "$keyring" >/dev/null
/usr/lib/apt/apt-helper download-file http://repo:8000/puzuz.sources "$sources" >/dev/null
cp "$keyring" /tmp/puzuz.gpg && cp "$sources" /tmp/puzuz.sources
clean_update
apt-get install -y -q puzuz </dev/null 2>&1 | tee /tmp/install.out
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
no_conffile_prompt /tmp/install.out
cmp -s "$keyring" /tmp/puzuz.gpg && cmp -s "$sources" /tmp/puzuz.sources || fail "the files changed"
dpkg-query -S "$keyring" "$sources" >/dev/null || fail "the package does not own the files"
clean_update
echo
echo "all APT repository checks passed"
EOF
    # Back to v1 for the next distribution.
    docker exec -i "$server" bash -euo pipefail <<'EOF'
. /w/env.sh
rm -rf state serve/*
"$repo" update state debs/puzuz_9.9.9-rc.1_*.deb 2>/dev/null
cp state/* serve/
EOF
done

echo
echo "requests to the test repository:"
docker exec "$server" sh -c "grep -o '\"GET [^\"]*\" [0-9]*' /w/http.log | sort | uniq -c"
echo
echo "APT repository test passed on: ${bases[*]}"
