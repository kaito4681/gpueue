#!/bin/sh
# Checks the puzuz .deb inside a plain Debian/Ubuntu container without systemd
# as PID 1 (run by scripts/deb-test.sh): its metadata and contents, that
# `apt-get install ./puzuz_*.deb` succeeds and sets up the user, directories
# and services, that an edited conffile survives a reinstallation, that
# installing next to `puzuz service install` is refused, that a rolled-back
# removal (postinst abort-remove) enables the services again, and what
# `dpkg -r` and `dpkg -P` keep and delete.
#
# TODO(P4b): `apt-get update` / `apt upgrade` against a real (test) APT
# repository, which P4b adds with the repository itself.
#
# Usage (as root, in the container): deb-check.sh DEB
set -eu

deb="$1"
# Must equal service::MARKER (checked by a unit test in crates/puzuz).
MARKER='# Installed by `puzuz service install`; re-running it overwrites this file.'
export DEBIAN_FRONTEND=noninteractive

step() { printf '\n=== %s\n' "$*"; }
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}
must_exist() { for f in "$@"; do [ -e "$f" ] || fail "missing $f"; done; }
must_not_exist() { for f in "$@"; do [ ! -e "$f" ] || fail "still there: $f"; done; }

step "metadata and contents"
dpkg-deb --info "$deb"
dpkg-deb --field "$deb" Package | grep -qx puzuz || fail "Package is not puzuz"
dpkg-deb --field "$deb" Depends | grep -q 'systemd (>= 240)' || fail "no Depends on systemd (>= 240)"
dpkg-deb --field "$deb" Depends | grep -q criu && fail "criu must not be a Depends"
dpkg-deb --field "$deb" Recommends | grep -qx criu || fail "criu is not recommended"
dpkg-deb --contents "$deb" >/tmp/contents
for f in ./usr/bin/puzuz ./usr/bin/puzuz-helper \
    ./usr/lib/systemd/system/puzuz.service ./usr/lib/systemd/system/puzuz-helper.service \
    ./usr/lib/sysusers.d/puzuz.conf ./usr/lib/tmpfiles.d/puzuz.conf ./etc/puzuz/config.toml \
    ./etc/apt/sources.list.d/puzuz.sources ./etc/apt/keyrings/puzuz.gpg \
    ./usr/share/bash-completion/completions/puzuz ./usr/share/zsh/vendor-completions/_puzuz \
    ./usr/share/fish/vendor_completions.d/puzuz.fish ./usr/share/doc/puzuz/copyright; do
    grep -q " $f\$" /tmp/contents || fail "$f is not in the package"
done
dpkg-deb --ctrl-tarfile "$deb" | tar -xOf - ./conffiles >/tmp/conffiles
grep -qx /etc/puzuz/config.toml /tmp/conffiles || fail "config.toml is not a conffile"
grep -qx /etc/apt/sources.list.d/puzuz.sources /tmp/conffiles || fail "puzuz.sources is not a conffile"
grep -qx /etc/apt/keyrings/puzuz.gpg /tmp/conffiles || fail "the archive keyring is not a conffile"

step "installing next to \`puzuz service install\` is refused"
mkdir -p /etc/systemd/system
printf '%s\n[Service]\nExecStart=/usr/local/bin/puzuz daemon\n' "$MARKER" >/etc/systemd/system/puzuz.service
if dpkg -i "$deb" >/tmp/refused 2>&1; then fail "installed next to service install"; fi
grep -q 'sudo puzuz uninstall' /tmp/refused || { cat /tmp/refused; fail "no hint"; }
rm -f /etc/systemd/system/puzuz.service
dpkg -r puzuz >/dev/null 2>&1 || true

step "apt-get install ./$(basename "$deb")"
apt-get install -y "$deb" | tee /tmp/install.out
[ "$(dpkg-query -W -f='${Status}' puzuz)" = "install ok installed" ] || fail "not installed"
grep -q 'systemd is not running' /tmp/install.out || fail "postinst did not say that systemd is not running"
puzuz --version
must_exist /usr/bin/puzuz-helper /usr/lib/systemd/system/puzuz.service \
    /etc/apt/keyrings/puzuz.gpg /etc/apt/sources.list.d/puzuz.sources
for unit in puzuz.service puzuz-helper.service; do
    [ "$(head -n 1 /usr/lib/systemd/system/$unit)" != "$MARKER" ] || fail "$unit has the service install marker"
done
grep -qx 'ExecStart=/usr/bin/puzuz daemon' /usr/lib/systemd/system/puzuz.service || fail "daemon ExecStart="
grep -qx 'ExecStart=/usr/bin/puzuz-helper serve' /usr/lib/systemd/system/puzuz-helper.service || fail "helper ExecStart="
getent passwd puzuz >/dev/null || fail "no user puzuz"
getent group puzuz-admin >/dev/null || fail "no group puzuz-admin"
[ "$(stat -c '%U %G %a' /var/lib/puzuz)" = "root root 755" ] || fail "/var/lib/puzuz: $(stat -c '%U %G %a' /var/lib/puzuz)"
[ "$(stat -c '%U %a' /var/lib/puzuz/daemon)" = "puzuz 700" ] || fail "/var/lib/puzuz/daemon: $(stat -c '%U %a' /var/lib/puzuz/daemon)"
# Enabled offline, so the services would start on the next boot.
must_exist /etc/systemd/system/multi-user.target.wants/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz-helper.service
# apt reads the repository from puzuz.sources (not fetched: it does not exist yet).
apt-get indextargets --no-release-info 2>/dev/null | grep -q 'github.com/kaito4681/puzuz/releases/download/apt' ||
    fail "apt does not see the puzuz repository"

step "a rolled-back removal enables the services again (prerm remove, postinst abort-remove)"
wants=/etc/systemd/system/multi-user.target.wants
# Also when /var/lib/puzuz is missing: prerm creates it for its marker.
rm -rf /var/lib/puzuz
/var/lib/dpkg/info/puzuz.prerm remove
must_not_exist "$wants/puzuz.service" "$wants/puzuz-helper.service"
must_exist /var/lib/puzuz/package-removed
[ "$(stat -c '%U %G %a' /var/lib/puzuz)" = "root root 755" ] || fail "/var/lib/puzuz: $(stat -c '%U %G %a' /var/lib/puzuz)"
/var/lib/dpkg/info/puzuz.postinst abort-remove
must_exist "$wants/puzuz.service" "$wants/puzuz-helper.service" /var/lib/puzuz/daemon
must_not_exist /var/lib/puzuz/package-removed
# Without the marker (prerm did not get that far) abort-remove changes nothing.
/var/lib/dpkg/info/puzuz.postinst abort-remove >/tmp/abort.out
[ ! -s /tmp/abort.out ] || { cat /tmp/abort.out; fail "abort-remove without the marker did something"; }

step "an edited conffile survives a reinstallation (the upgrade path)"
echo '# edited by deb-check' >>/etc/puzuz/config.toml
dpkg -i "$deb"
grep -q 'edited by deb-check' /etc/puzuz/config.toml || fail "config.toml edit lost"

step "dpkg -r keeps the configuration and the state"
mkdir -p /var/lib/puzuz/logs/1000
echo log >/var/lib/puzuz/logs/1000/job.log
dpkg -r puzuz
must_not_exist /usr/bin/puzuz /usr/bin/puzuz-helper /usr/lib/systemd/system/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz.service
must_exist /etc/puzuz/config.toml /etc/apt/sources.list.d/puzuz.sources /etc/apt/keyrings/puzuz.gpg \
    /var/lib/puzuz/logs/1000/job.log
[ "$(dpkg-query -W -f='${Status}' puzuz)" = "deinstall ok config-files" ] || fail "not in config-files state"

step "a reinstallation after dpkg -r enables the services again"
dpkg -i "$deb"
must_exist /etc/systemd/system/multi-user.target.wants/puzuz.service
must_not_exist /var/lib/puzuz/package-removed
grep -q 'edited by deb-check' /etc/puzuz/config.toml || fail "config.toml edit lost"

step "dpkg -P deletes the configuration, keeps /var/lib/puzuz"
echo token >/etc/puzuz/github-token
dpkg -P puzuz | tee /tmp/purge.out
must_not_exist /etc/puzuz /etc/apt/sources.list.d/puzuz.sources /etc/apt/keyrings/puzuz.gpg
must_exist /var/lib/puzuz/logs/1000/job.log
grep -q 'rm -rf /var/lib/puzuz' /tmp/purge.out || fail "purge did not say how to delete /var/lib/puzuz"
case "$(dpkg-query -W -f='${Status}' puzuz 2>/dev/null || true)" in
*"ok installed" | *config-files) fail "still installed after purge" ;;
esac

printf '\nall .deb checks passed\n'
