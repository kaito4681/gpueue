#!/usr/bin/env bash
# APT repository test (scenario 19): builds a flat repository with
# scripts/dist/apt-repo.sh and a throwaway GPG key, serves it over HTTP on a
# Docker network, and in a plain container (no systemd as PID 1) per
# distribution:
#   - installs v1 of the .deb with `apt-get install ./…`; its own
#     gpueue.sources and archive keyring are used as they are, and
#     `apt-get update` verifies the index (and refuses another key);
#   - publishes v2, then `apt-get update && apt-get upgrade` installs it,
#     keeping the edited config.toml and gpueue.sources; `gpueue update` refuses;
#   - `apt-get remove` keeps the conffiles (the source and its keyring:
#     `apt-get update` still works), `apt-get purge` deletes them and keeps
#     /var/lib/gpueue;
#   - adding the repository by hand with the published gpueue.gpg and
#     gpueue.sources, then `apt-get install gpueue`, works without a conffile
#     prompt.
# It also checks apt-repo.sh itself: keeping the newest versions in Debian
# version order, Filename: being the file name (0.2.0-rc.1 in the name,
# 0.2.0~rc.1 in the package), and its refusals, which leave the directory
# untouched, among them packages already in the repository that do not match
# its signed index; and `apt-repo.sh github` against a fake gh: publishing,
# refusing a tampered package in the release `apt`, and putting the previous
# index back after a failed upload.
#
# v1 and v2 are the package given (or built) with the versions 9.9.9-rc.1
# (9.9.9~rc.1 in the package) and 9.9.9 written into it, and with the
# throwaway key as its archive keyring and the test repository's URL in its
# gpueue.sources (as a real release has the real key and the GitHub URL), so
# only one build is needed.
#
# Usage: scripts/apt-repo-test.sh [DEB]
#   DEB: the package to start from (default: build one from the current tree,
#        like scripts/deb-test.sh)
# Environment:
#   GPUEUE_APT_BASES  distributions to test (default "debian:12 ubuntu:24.04";
#                    also supported: ubuntu:22.04 ubuntu:26.04)
#   GPUEUE_DEB_HTTPS=1  fetch distribution packages over HTTPS, with this
#                    machine's CA certificates, where port 80 is blocked
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
read -r -a bases <<<"${GPUEUE_APT_BASES:-debian:12 ubuntu:24.04}"
deb="${1:-}"
tools_image=debian:12

if [ -z "$deb" ]; then
    "$root/scripts/test-in-docker.sh" build --locked -p gpueue -p gpueue-helper
    deb="$("$root/scripts/dist/build-package.sh" "$root/target/docker/debug" "$root/target/apt-repo-test-deb")"
fi
deb="$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")"
[ -f "$deb" ] || { echo "no such file: $deb" >&2; exit 1; }

mkdir -p "$root/target"
work="$(mktemp -d "$root/target/apt-repo-test.XXXXXX")"
id="$$"
net="gpueue-apt-test-$id"
server="gpueue-apt-repo-$id"
client="gpueue-apt-client-$id"

cleanup() {
    # Files written by root in the containers.
    docker exec "$server" chown -R "$(id -u):$(id -g)" /w >/dev/null 2>&1 || true
    docker rm -f "$server" "$client" >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

mounts=()
# Retry transient mirror failures (every apt-get of the containers).
setup='printf "Acquire::Retries \"5\";\nAcquire::http::Timeout \"30\";\nAcquire::https::Timeout \"30\";\n" >/etc/apt/apt.conf.d/80-gpueue-retries'
if [ "${GPUEUE_DEB_HTTPS:-}" = 1 ]; then
    ca=/etc/ssl/certs/ca-certificates.crt
    [ -f "$ca" ] || { echo "GPUEUE_DEB_HTTPS=1 needs $ca" >&2; exit 1; }
    mounts+=(-v "$ca:/usr/local/share/host-ca.crt:ro")
    # Only the distribution's sources: the test repository is plain HTTP.
    setup+=$'\n''sed -i "s|http://|https://|g" /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/ubuntu.sources /etc/apt/sources.list.d/debian.sources 2>/dev/null || true
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
# apt-get update exits 0 even when an index failed to download: fail on E:/Err:.
apt-get update -q >/tmp/update.out 2>&1 || { cat /tmp/update.out >&2; exit 1; }
if grep -E '^(E|Err):' /tmp/update.out >&2; then echo "apt-get update failed" >&2; exit 1; fi
apt-get install -y -q --no-install-recommends apt-utils gnupg python3 curl >/dev/null
EOF

# `apt-repo.sh github` runs against a fake gh (on directories /w/gh/<release>)
# with this minisign binary.
"$root/scripts/dist/get-minisign.sh" "$root/target/dist-tools" >/dev/null
mkdir -p "$work/bin"
cat >"$work/bin/gh" <<'EOF'
#!/bin/bash
# A fake of the gh commands scripts/dist/apt-repo.sh runs: releases are the
# directories /w/gh/<tag>, assets their files; every release is published
# and stable. FAKE_GH_FAIL_UPLOAD=GLOB makes every upload of a path matching GLOB fail.
set -euo pipefail
r=/w/gh
all="$*"
echo "gh $all" >>/w/gh.log
if [ "$1" = api ]; then
    cat /w/gh-minisign.pub
    exit
fi
cmd="$1 $2" tag="$3"
shift 3
dir=. pats=() files=()
while [ "$#" -gt 0 ]; do
    case "$1" in
    -D) dir="$2" && shift 2 ;;
    -p) pats+=("$2") && shift 2 ;;
    -R | --json | -q | --title | --notes | --latest) shift 2 ;;
    -*) shift ;;
    *) files+=("$1") && shift ;;
    esac
done
case "$cmd" in
"release view")
    [ -d "$r/$tag" ] || { echo "release not found" >&2; exit 1; }
    case "$all" in
    *isDraft*) echo "false false" ;;
    *assets*) ls "$r/$tag" ;;
    esac
    ;;
"release create") mkdir -p "$r/$tag" ;;
"release download")
    mkdir -p "$dir"
    n=0
    shopt -s nullglob
    for p in "${pats[@]}"; do
        for f in "$r/$tag"/$p; do cp "$f" "$dir/" && n=$((n + 1)); done
    done
    [ "$n" -gt 0 ] || { echo "no assets match the file pattern" >&2; exit 1; }
    ;;
"release upload")
    for f in "${files[@]}"; do
        case "$f" in ${FAKE_GH_FAIL_UPLOAD:-/nothing}) echo "HTTP 502" >&2 && exit 1 ;; esac
    done
    cp "${files[@]}" "$r/$tag/"
    ;;
"release delete-asset") rm "$r/$tag/${files[0]}" ;;
*) echo "fake gh: unsupported: $cmd" >&2; exit 1 ;;
esac
EOF
chmod +x "$work/bin/gh"

# Sourced by the server-side steps.
cat >"$work/env.sh" <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
export APT_GPG_PRIVATE_KEY="$(cat /w/key.asc)" APT_GPG_PASSPHRASE=test-passphrase
# The existing index is verified with the throwaway key, not the packaged one.
export APT_VERIFY_KEYRING=/w/test-keyring.gpg
repo=/src/scripts/dist/apt-repo.sh
cd /w
EOF

echo "=== apt-repo.sh"
docker exec -i "$server" bash -euo pipefail <<'EOF'
export GNUPGHOME=/w/gnupg
mkdir -m 700 "$GNUPGHOME"
gpg --batch --quiet --pinentry-mode loopback --passphrase test-passphrase \
    --quick-gen-key "gpueue APT repository (apt-repo-test throwaway key)" rsa3072 sign never
gpg --batch --pinentry-mode loopback --passphrase test-passphrase --armor --export-secret-keys >/w/key.asc
gpg --batch --export >/w/test-keyring.gpg
gpgconf --kill all
# Two more throwaway keys: another one, which apt-get and apt-repo.sh refuse
# to verify the index with, and one whose user ID marks it as a TEST key,
# which apt-repo.sh refuses to publish packages with.
for k in other test-marked; do
    export GNUPGHOME="/w/gnupg-$k"
    mkdir -m 700 "$GNUPGHOME"
    uid="gpueue APT repository (apt-repo-test $k key)"
    [ "$k" = test-marked ] && uid="gpueue APT repository TEST KEY (apt-repo-test)"
    gpg --batch --quiet --pinentry-mode loopback --passphrase '' --quick-gen-key "$uid" rsa3072 sign never
    gpg --batch --export >"/w/$k-keyring.gpg"
    gpgconf --kill all
done
unset GNUPGHOME
. /w/env.sh

# The package with another version, the test key and the test repository's
# URL, under the release asset name gpueue_<tag version>_<arch>.deb.
testpkg() { # SRC TAG_VERSION OUT_DIR
    local x arch
    x="$(mktemp -d)"
    dpkg-deb -R "$1" "$x"
    sed -i "s/^Version: .*/Version: ${2/-/\~}/" "$x/DEBIAN/control"
    cp /w/test-keyring.gpg "$x/etc/apt/keyrings/gpueue.gpg"
    sed -i 's|^URIs:.*|URIs: http://repo:8000/|' "$x/etc/apt/sources.list.d/gpueue.sources"
    (cd "$x" && md5sum etc/apt/keyrings/gpueue.gpg etc/apt/sources.list.d/gpueue.sources) >"$x/md5.new"
    grep -v -e ' etc/apt/keyrings/gpueue.gpg$' -e ' etc/apt/sources.list.d/gpueue.sources$' \
        "$x/DEBIAN/md5sums" >>"$x/md5.new" || true
    mv "$x/md5.new" "$x/DEBIAN/md5sums"
    arch="$(dpkg-deb --field "$1" Architecture)"
    mkdir -p "$3"
    dpkg-deb --root-owner-group -b "$x" "$3/gpueue_$2_$arch.deb" >/dev/null
    rm -rf "$x"
}
testpkg pkg/orig.deb 9.9.9-rc.1 debs
testpkg pkg/orig.deb 9.9.9 debs
arch="$(dpkg-deb --field pkg/orig.deb Architecture)"
cp "debs/gpueue_9.9.9-rc.1_$arch.deb" debs/v1.deb
cp "debs/gpueue_9.9.9_$arch.deb" debs/v2.deb

# Pruning: tiny packages of both architectures, newest five per architecture.
mkpkg() { # VERSION(tag) ARCH [OUT_DIR]
    local x
    x="$(mktemp -d)"
    mkdir -p "$x/DEBIAN" "$x/etc/apt/sources.list.d" "$x/etc/apt/keyrings"
    cp /src/packaging/apt/gpueue.sources "$x/etc/apt/sources.list.d/"
    cp "${MKPKG_KEYRING:-/w/test-keyring.gpg}" "$x/etc/apt/keyrings/gpueue.gpg"
    printf 'Package: gpueue\nVersion: %s\nArchitecture: %s\nMaintainer: test <t@example.com>\nDescription: test\n' \
        "${1/-/\~}" "$2" >"$x/DEBIAN/control"
    mkdir -p "${3:-fake}"
    dpkg-deb --root-owner-group -b "$x" "${3:-fake}/gpueue_$1_$2.deb" >/dev/null
    rm -rf "$x"
}
for v in 0.1.0 0.2.0 0.9.0 0.10.0-rc.1 0.10.0 1.0.0 1.0.1; do mkpkg "$v" amd64; done
for v in 0.9.0 1.0.0; do mkpkg "$v" arm64; done
GPUEUE_APT_KEEP=5 "$repo" update prune fake/gpueue_0.1.0_amd64.deb fake/gpueue_0.10.0_amd64.deb fake/gpueue_0.9.0_arm64.deb
GPUEUE_APT_KEEP=5 "$repo" update prune fake/*.deb
expected="gpueue_0.10.0-rc.1_amd64.deb gpueue_0.10.0_amd64.deb gpueue_0.9.0_amd64.deb gpueue_0.9.0_arm64.deb gpueue_1.0.0_amd64.deb gpueue_1.0.0_arm64.deb gpueue_1.0.1_amd64.deb"
[ "$(cd prune && echo gpueue_*.deb)" = "$expected" ] || fail "kept $(cd prune && echo gpueue_*.deb)"
grep -qx 'Filename: gpueue_0.10.0-rc.1_amd64.deb' prune/Packages || fail "Filename: of the pre-release"
grep -qx 'Version: 0.10.0~rc.1' prune/Packages || fail "Version: of the pre-release"
[ "$(grep -c '^Package: gpueue$' prune/Packages)" = 7 ] || fail "Packages does not list 7 packages"
grep -q '^Filename: \./' prune/Packages && fail "Filename: is not the bare file name"
zcat prune/Packages.gz | cmp -s - prune/Packages || fail "Packages.gz differs from Packages"
grep -qx 'Origin: gpueue' prune/Release || fail "no Origin"
grep -qx 'Architectures: amd64 arm64' prune/Release || fail "no Architectures"
grep -Eq '^(Suite|Codename):' prune/Release && fail "Release has a Suite or Codename"
grep -q ' Packages.gz$' prune/Release || fail "Release does not list Packages.gz"
gpgv --quiet --keyring /w/test-keyring.gpg prune/InRelease 2>/dev/null || fail "InRelease"
gpgv --quiet --keyring /w/test-keyring.gpg prune/Release.gpg prune/Release 2>/dev/null || fail "Release.gpg"
cmp -s prune/gpueue.gpg test-keyring.gpg || fail "gpueue.gpg is not the packages' keyring"
cmp -s prune/gpueue.sources /src/packaging/apt/gpueue.sources || fail "gpueue.sources"
echo "pruning and the index: ok"

# gpueue.gpg and gpueue.sources come from the newest kept package, also when
# an older version is (re)published.
mkdir -p older
cp /src/packaging/apt/gpueue.sources older/s && echo '# older' >>older/s
x="$(mktemp -d)" && dpkg-deb -R fake/gpueue_0.9.0_amd64.deb "$x" && cp older/s "$x/etc/apt/sources.list.d/gpueue.sources"
sed -i 's/^Version: .*/Version: 0.9.5/' "$x/DEBIAN/control"
dpkg-deb --root-owner-group -b "$x" older/gpueue_0.9.5_amd64.deb >/dev/null && rm -rf "$x"
cp -r prune prune2
GPUEUE_APT_KEEP=10 "$repo" update prune2 older/gpueue_0.9.5_amd64.deb 2>/dev/null
cmp -s prune2/gpueue.sources /src/packaging/apt/gpueue.sources || fail "gpueue.sources taken from the older package"
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
refused "signing key not in the keyring" env APT_KEYRING=/w/other-keyring.gpg "$repo" update prune
mkdir -p bad
cp fake/gpueue_1.0.0_arm64.deb bad/gpueue_1.0.1_amd64.deb
refused "same name, other contents" "$repo" update prune bad/gpueue_1.0.1_amd64.deb
cp fake/gpueue_1.0.0_amd64.deb bad/gpueue_1.0.2_amd64.deb
refused "file name is not the version" "$repo" update prune bad/gpueue_1.0.2_amd64.deb
echo 'not a package' >bad/gpueue_2.0.0_amd64.deb
refused "not a .deb" "$repo" update prune bad/gpueue_2.0.0_amd64.deb
mkpkg 3.0.0 amd64 bad
truncate -s "$(($(stat -c %s bad/gpueue_3.0.0_amd64.deb) - 100))" bad/gpueue_3.0.0_amd64.deb
refused "truncated .deb" "$repo" update prune bad/gpueue_3.0.0_amd64.deb
mkpkg 1.0.01 amd64 bad
refused "duplicate version (1.0.01 = 1.0.1)" "$repo" update prune bad/gpueue_1.0.01_amd64.deb
# A package that ships a TEST key (scripts/dist/gen-apt-key.sh --test).
MKPKG_KEYRING=/w/test-marked-keyring.gpg mkpkg 4.0.0 amd64 testkey-debs
if "$repo" update testkey testkey-debs/gpueue_4.0.0_amd64.deb 2>/tmp/refused.err; then fail "accepted: packages with the TEST key"; fi
grep -q 'ships the TEST archive key' /tmp/refused.err || fail "packages with the TEST key refused for another reason"
[ ! -e testkey ] || fail "wrote testkey/ for packages with the TEST key"
echo "refused (TEST key): $(tail -n 1 /tmp/refused.err)"
echo "refusals: ok"

# The packages already in the repository are verified against its signed
# index before they are indexed and signed again.
existing() { rm -rf "$1" && cp -r prune "$1"; }
refused_in() { # DIR PATTERN, then the command
    local d="$1" pattern="$2" sum
    shift 2
    sum="$(cd "$d" && sha256sum ./*)"
    if "$@" 2>/tmp/refused.err; then fail "accepted: $pattern"; fi
    grep -qF -- "$pattern" /tmp/refused.err || { cat /tmp/refused.err; fail "refused, but not with: $pattern"; }
    [ "$(cd "$d" && sha256sum ./*)" = "$sum" ] || fail "$d changed after: $pattern"
    echo "refused ($pattern)"
}
# reowned DEB: the same package with one more file (still a valid package).
reowned() {
    local x
    x="$(mktemp -d)"
    dpkg-deb -R "$1" "$x"
    echo tampered >"$x/tampered"
    dpkg-deb --root-owner-group -b "$x" "$1" >/dev/null
    rm -rf "$x"
}
existing tampered
reowned tampered/gpueue_1.0.0_amd64.deb
refused_in tampered "gpueue_1.0.0_amd64.deb differs from the signed index" "$repo" update tampered
refused_in tampered "gpueue_1.0.0_amd64.deb differs from the signed index" "$repo" update tampered fake/gpueue_0.9.0_amd64.deb
# ... also with Packages rewritten to match it.
h="$(sha256sum <tampered/gpueue_1.0.0_amd64.deb)"
awk -v h="${h%% *}" '/^Filename: gpueue_1.0.0_amd64.deb$/ { f = 1 } /^SHA256: / && f { $0 = "SHA256: " h; f = 0 } { print }' \
    tampered/Packages >tampered/Packages.new && mv tampered/Packages.new tampered/Packages
refused_in tampered "the existing Packages does not match the signed InRelease" "$repo" update tampered
# ... and with Release and InRelease rewritten too, signed by another key.
existing tampered2
GNUPGHOME=/w/gnupg-other gpg --batch --yes --quiet --pinentry-mode loopback --passphrase '' \
    --clearsign -o tampered2/InRelease tampered2/Release
refused_in tampered2 "the existing InRelease does not verify" "$repo" update tampered2
existing noindex
rm noindex/InRelease
refused_in noindex "has packages but no InRelease" "$repo" update noindex
# A package the signed index does not list (an interrupted run uploads the
# packages first): dropped, not indexed.
existing leftover
mkpkg 2.0.0 amd64 leftover
"$repo" update leftover 2>/tmp/leftover.err
grep -qF "gpueue_2.0.0_amd64.deb is not in the signed index" /tmp/leftover.err || fail "no warning about the leftover"
[ ! -e leftover/gpueue_2.0.0_amd64.deb ] || fail "the leftover was kept"
grep -q 'gpueue_2.0.0' leftover/Packages && fail "the leftover was indexed"
echo "verifying the existing packages: ok"

# `apt-repo.sh github` with the fake gh: a release v1.0.2 (checksummed and
# signed with a throwaway minisign key) added to the release `apt`.
export PATH="/w/bin:$PATH" MINISIGN=/src/target/dist-tools/minisign GPUEUE_APT_RETRY_DELAY=0
"$MINISIGN" -G -W -p /w/gh-minisign.pub -s /w/gh-minisign.key >/dev/null
ghrelease() { # TAG_VERSION
    mkdir -p "/w/gh/v$1"
    mkpkg "$1" amd64 "/w/gh/v$1"
    (cd "/w/gh/v$1" && sha256sum "gpueue_$1_amd64.deb" >"gpueue_$1_amd64.deb.sha256")
    "$MINISIGN" -S -s /w/gh-minisign.key -m "/w/gh/v$1/gpueue_$1_amd64.deb" >/dev/null
    # The version-less copy release.yml attaches for releases/latest/download.
    cp "/w/gh/v$1/gpueue_$1_amd64.deb" "/w/gh/v$1/gpueue_amd64.deb"
    (cd "/w/gh/v$1" && sha256sum gpueue_amd64.deb >gpueue_amd64.deb.sha256)
    "$MINISIGN" -S -s /w/gh-minisign.key -m "/w/gh/v$1/gpueue_amd64.deb" >/dev/null
}
mkdir -p /w/gh
cp -r prune /w/gh/apt
for v in 1.0.2 1.0.3; do ghrelease "$v"; done
"$repo" github v1.0.2
[ -f /w/gh/apt/gpueue_1.0.2_amd64.deb ] || fail "github: v1.0.2 not uploaded"
[ ! -e /w/gh/apt/gpueue_0.9.0_amd64.deb ] || fail "github: the pruned package was not deleted"
[ ! -e /w/gh/apt/gpueue_amd64.deb ] || fail "github: the version-less copy was published"
[ "$(grep -c '^Filename: gpueue_1.0.2_amd64.deb$' /w/gh/apt/Packages)" = 1 ] || fail "github: v1.0.2 indexed more than once"
grep -qx 'Filename: gpueue_1.0.2_amd64.deb' /w/gh/apt/Packages || fail "github: v1.0.2 not indexed"
gpgv --quiet --keyring /w/test-keyring.gpg /w/gh/apt/InRelease 2>/dev/null || fail "github: InRelease"
ghsnapshot() { (cd /w/gh/apt && sha256sum ./*); }
# A tampered package in the release `apt`: refused, nothing uploaded.
cp /w/gh/apt/gpueue_1.0.0_amd64.deb /tmp/good.deb
reowned /w/gh/apt/gpueue_1.0.0_amd64.deb
ghsnapshot >/tmp/gh.sum
if "$repo" github v1.0.3 2>/tmp/refused.err; then fail "github: accepted a tampered package"; fi
grep -qF "gpueue_1.0.0_amd64.deb differs from the signed index" /tmp/refused.err || { cat /tmp/refused.err; fail "github: refused for another reason"; }
ghsnapshot | cmp -s - /tmp/gh.sum || fail "github: the release changed after a refusal"
echo "refused (github: tampered package)"
cp /tmp/good.deb /w/gh/apt/gpueue_1.0.0_amd64.deb
# The upload of InRelease fails (every attempt): the previous index is put
# back; v1.0.3 stays as an unlisted leftover.
cp /w/gh/apt/InRelease /w/gh/apt/Packages /tmp/
: >/w/gh.log
if FAKE_GH_FAIL_UPLOAD='*/repo/InRelease' "$repo" github v1.0.3 2>/tmp/refused.err; then fail "github: a failed upload passed"; fi
grep -qF "rerun the workflow for v1.0.3" /tmp/refused.err || { cat /tmp/refused.err; fail "github: no recovery hint"; }
[ "$(grep -c 'release upload apt.*/repo/InRelease' /w/gh.log)" = 3 ] || fail "github: InRelease upload not tried 3 times"
grep -qF 'restored the previous index' /tmp/refused.err || { cat /tmp/refused.err; fail "github: the previous index was not restored"; }
cmp -s /tmp/InRelease /w/gh/apt/InRelease && cmp -s /tmp/Packages /w/gh/apt/Packages || fail "github: the previous index was not restored"
echo "github: a failed upload restores the previous index: ok"
# The re-run publishes v1.0.3 (dropping and re-uploading the leftover).
"$repo" github v1.0.3 2>/tmp/rerun.err
grep -qF "gpueue_1.0.3_amd64.deb is not in the signed index" /tmp/rerun.err || fail "github: the leftover was trusted"
grep -qx 'Filename: gpueue_1.0.3_amd64.deb' /w/gh/apt/Packages || fail "github: v1.0.3 not indexed"
cmp -s /w/gh/apt/gpueue_1.0.3_amd64.deb /w/gh/v1.0.3/gpueue_1.0.3_amd64.deb || fail "github: v1.0.3 differs from its release"
gpgv --quiet --keyring /w/test-keyring.gpg /w/gh/apt/InRelease 2>/dev/null || fail "github: InRelease after the re-run"
echo "apt-repo.sh github: ok"

# The test repository: v1 for now.
"$repo" update state "debs/gpueue_9.9.9-rc.1_$arch.deb"
mkdir serve && cp state/* serve/
EOF
docker exec -d "$server" sh -c 'cd /w/serve && exec python3 -m http.server 8000 >/w/http.log 2>&1'
docker exec "$server" curl -sf --retry 30 --retry-delay 1 --retry-connrefused -o /dev/null http://127.0.0.1:8000/gpueue.sources ||
    { echo "the test repository's HTTP server did not come up" >&2; docker exec "$server" cat /w/http.log >&2 || true; exit 1; }

publish_v2() {
    docker exec -i "$server" bash -euo pipefail <<'EOF'
. /w/env.sh
"$repo" update state debs/gpueue_9.9.9_*.deb
# Packages first, the index last, as the workflow uploads them.
cp state/gpueue_*.deb serve/
for f in gpueue.gpg gpueue.sources Packages Packages.gz Release Release.gpg InRelease; do cp "state/$f" serve/; done
EOF
}

for base in "${bases[@]}"; do
    echo
    echo "=== $base"
    docker rm -f "$client" >/dev/null 2>&1 || true
    docker run -d --name "$client" --network "$net" "${mounts[@]}" \
        -v "$work/debs:/debs:ro" -v "$work/other-keyring.gpg:/other-keyring.gpg:ro" \
        "$base" sleep infinity >/dev/null
    docker exec -i "$client" bash -euo pipefail <<EOF
$setup
export DEBIAN_FRONTEND=noninteractive
apt-get update -q >/tmp/update.out 2>&1 || { cat /tmp/update.out >&2; exit 1; }
if grep -E '^(E|Err):' /tmp/update.out >&2; then echo "apt-get update failed" >&2; exit 1; fi
EOF
    docker exec -i "$client" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive
status() { dpkg-query -W -f='${Status} ${Version}' gpueue 2>/dev/null || true; }
# Whether `apt-cache madison gpueue` has a line matching the pattern. Not
# `… | grep -q`, even with the output buffered first: bash's printf writes one
# line per write(2), so when grep -q exits on a match before the last line,
# the next write dies of SIGPIPE and pipefail turns the match into a failure.
in_madison() { local out; out="$(apt-cache madison gpueue)"; grep -q "$1" <<<"$out"; }
clean_update() {
    apt-get update -q 2>&1 | tee /tmp/update.out
    # Only the test repository is judged: warnings about the distribution's
    # mirrors say nothing about it (the mirrors were checked before).
    if grep -E '^(W|E|Err):.*repo:8000' /tmp/update.out; then fail "apt-get update reported a problem with the test repository"; fi
    grep -q 'http://repo:8000 ./ InRelease' /tmp/update.out || fail "InRelease was not fetched"
}
sources=/etc/apt/sources.list.d/gpueue.sources
keyring=/etc/apt/keyrings/gpueue.gpg

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
in_madison 'repo:8000' && fail "apt uses the unverified index"
cp -p /tmp/keyring.orig "$keyring"

step "apt-get update verifies the index with the package's own files"
echo '# edited by apt-repo-test' >>"$sources"
echo '# edited by apt-repo-test' >>/etc/gpueue/config.toml
clean_update
in_madison '9.9.9~rc.1 | *http://repo:8000' || { apt-cache madison gpueue; fail "v1 not in the repository"; }
if apt-get -s upgrade | grep '^Inst gpueue'; then fail "an upgrade before v2 is published"; fi
EOF

    publish_v2

    docker exec -i "$client" bash -euo pipefail <<'EOF'
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n--- %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive
status() { dpkg-query -W -f='${Status} ${Version}' gpueue 2>/dev/null || true; }
# Whether `apt-cache madison gpueue` has a line matching the pattern. Not
# `… | grep -q`, even with the output buffered first: bash's printf writes one
# line per write(2), so when grep -q exits on a match before the last line,
# the next write dies of SIGPIPE and pipefail turns the match into a failure.
in_madison() { local out; out="$(apt-cache madison gpueue)"; grep -q "$1" <<<"$out"; }
clean_update() {
    apt-get update -q 2>&1 | tee /tmp/update.out
    # Only the test repository is judged: warnings about the distribution's
    # mirrors say nothing about it (the mirrors were checked before).
    if grep -E '^(W|E|Err):.*repo:8000' /tmp/update.out; then fail "apt-get update reported a problem with the test repository"; fi
    grep -q 'http://repo:8000 ./ InRelease' /tmp/update.out || fail "InRelease was not fetched"
}
no_conffile_prompt() {
    if grep -Ei "configuration file|conffile" "$1"; then fail "dpkg asked about a conffile"; fi
    if ls /etc/apt/sources.list.d/*.dpkg-* /etc/apt/keyrings/*.dpkg-* /etc/gpueue/*.dpkg-* 2>/dev/null; then
        fail "dpkg left a .dpkg-* file"
    fi
}
sources=/etc/apt/sources.list.d/gpueue.sources
keyring=/etc/apt/keyrings/gpueue.gpg

step "apt-get update && apt-get upgrade installs v2 (9.9.9)"
clean_update
apt-get upgrade -y -q </dev/null 2>&1 | tee /tmp/upgrade.out
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
grep -q 'edited by apt-repo-test' /etc/gpueue/config.toml || fail "config.toml edit lost"
grep -q 'edited by apt-repo-test' "$sources" || fail "gpueue.sources edit lost"
no_conffile_prompt /tmp/upgrade.out
if gpueue update >/tmp/pu.out 2>&1; then fail "gpueue update did not refuse"; fi
grep -q 'apt upgrade' /tmp/pu.out || { cat /tmp/pu.out; fail "gpueue update does not point to apt"; }

step "apt-get remove keeps the source and its keyring"
apt-get remove -y -q gpueue >/dev/null
[ "$(status)" = "deinstall ok config-files 9.9.9" ] || fail "status: $(status)"
[ -e /etc/gpueue/config.toml ] && [ -e "$sources" ] && [ -e "$keyring" ] || fail "conffiles removed"
[ ! -e /usr/bin/gpueue ] || fail "/usr/bin/gpueue still there"
clean_update
in_madison '9.9.9 | *http://repo:8000' || { apt-cache madison gpueue; fail "the repository is gone after remove"; }

step "apt-get purge deletes the source and the keyring, keeps /var/lib/gpueue"
apt-get purge -y -q gpueue >/dev/null
[ ! -e /etc/gpueue ] || fail "purge left /etc/gpueue"
[ ! -e "$sources" ] && [ ! -e "$keyring" ] || fail "purge left gpueue.sources or the keyring"
[ -d /var/lib/gpueue ] || fail "/var/lib/gpueue deleted"
[ -z "$(status)" ] || [ "$(status)" = "unknown ok not-installed " ] || fail "status after purge: $(status)"

step "the repository added by hand (published gpueue.gpg and gpueue.sources): apt-get install gpueue"
mkdir -p /etc/apt/keyrings
/usr/lib/apt/apt-helper download-file http://repo:8000/gpueue.gpg "$keyring" >/dev/null
/usr/lib/apt/apt-helper download-file http://repo:8000/gpueue.sources "$sources" >/dev/null
cp "$keyring" /tmp/gpueue.gpg && cp "$sources" /tmp/gpueue.sources
clean_update
apt-get install -y -q gpueue </dev/null 2>&1 | tee /tmp/install.out
[ "$(status)" = "install ok installed 9.9.9" ] || fail "status: $(status)"
no_conffile_prompt /tmp/install.out
cmp -s "$keyring" /tmp/gpueue.gpg && cmp -s "$sources" /tmp/gpueue.sources || fail "the files changed"
dpkg-query -S "$keyring" "$sources" >/dev/null || fail "the package does not own the files"
clean_update
echo
echo "all APT repository checks passed"
EOF
    # Back to v1 for the next distribution.
    docker exec -i "$server" bash -euo pipefail <<'EOF'
. /w/env.sh
rm -rf state serve/*
"$repo" update state debs/gpueue_9.9.9-rc.1_*.deb 2>/dev/null
cp state/* serve/
EOF
done

echo
echo "requests to the test repository:"
docker exec "$server" sh -c "grep -o '\"GET [^\"]*\" [0-9]*' /w/http.log | sort | uniq -c"
echo
echo "APT repository test passed on: ${bases[*]}"
