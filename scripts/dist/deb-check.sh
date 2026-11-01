#!/bin/sh
# Checks the gpueue .deb inside a plain Debian/Ubuntu container without systemd
# as PID 1 (run by scripts/deb-test.sh): its metadata and contents, that
# `apt-get install ./gpueue_*.deb` succeeds and sets up the user, directories
# and services, that an edited conffile survives a reinstallation, that
# installing next to `gpueue service install` is refused, that a rolled-back
# removal (postinst abort-remove) enables the services again, that postinst
# warns about a CRIU older than 4.2 (and runs none root does not own), and what
# `dpkg -r` and `dpkg -P` keep and delete.
#
# TODO(P4b): `apt-get update` / `apt upgrade` against a real (test) APT
# repository, which P4b adds with the repository itself.
#
# Usage (as root, in the container): deb-check.sh DEB
set -eu

deb="$1"
# Must equal service::MARKER (checked by a unit test in crates/gpueue).
MARKER='# Installed by `gpueue service install`; re-running it overwrites this file.'
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
dpkg-deb --field "$deb" Package | grep -qx gpueue || fail "Package is not gpueue"
dpkg-deb --field "$deb" Depends | grep -q 'systemd (>= 240)' || fail "no Depends on systemd (>= 240)"
dpkg-deb --field "$deb" Depends | grep -q criu && fail "criu must not be a Depends"
dpkg-deb --field "$deb" Recommends | grep -qx criu || fail "criu is not recommended"
dpkg-deb --contents "$deb" >/tmp/contents
for f in ./usr/bin/gpueue ./usr/bin/gpueue-helper \
    ./usr/lib/systemd/system/gpueue.service ./usr/lib/systemd/system/gpueue-helper.service \
    ./usr/lib/sysusers.d/gpueue.conf ./usr/lib/tmpfiles.d/gpueue.conf ./etc/gpueue/config.toml \
    ./etc/apt/sources.list.d/gpueue.sources ./etc/apt/keyrings/gpueue.gpg \
    ./usr/share/bash-completion/completions/gpueue ./usr/share/zsh/vendor-completions/_gpueue \
    ./usr/share/fish/vendor_completions.d/gpueue.fish ./usr/share/doc/gpueue/copyright \
    ./usr/lib/systemd/system/reboot.target.d/gpueue.conf ./usr/lib/systemd/system/poweroff.target.d/gpueue.conf \
    ./usr/lib/systemd/system/halt.target.d/gpueue.conf ./usr/lib/systemd/system/kexec.target.d/gpueue.conf \
    ./usr/lib/systemd/system/soft-reboot.target.d/gpueue.conf; do
    grep -q " $f\$" /tmp/contents || fail "$f is not in the package"
done
dpkg-deb --ctrl-tarfile "$deb" | tar -xOf - ./conffiles >/tmp/conffiles
grep -qx /etc/gpueue/config.toml /tmp/conffiles || fail "config.toml is not a conffile"
grep -qx /etc/apt/sources.list.d/gpueue.sources /tmp/conffiles || fail "gpueue.sources is not a conffile"
grep -qx /etc/apt/keyrings/gpueue.gpg /tmp/conffiles || fail "the archive keyring is not a conffile"

step "installing next to \`gpueue service install\` is refused"
mkdir -p /etc/systemd/system
printf '%s\n[Service]\nExecStart=/usr/local/bin/gpueue daemon\n' "$MARKER" >/etc/systemd/system/gpueue.service
if dpkg -i "$deb" >/tmp/refused 2>&1; then fail "installed next to service install"; fi
grep -q 'sudo gpueue uninstall' /tmp/refused || { cat /tmp/refused; fail "no hint"; }
rm -f /etc/systemd/system/gpueue.service
dpkg -r gpueue >/dev/null 2>&1 || true

step "apt-get install ./$(basename "$deb")"
apt-get install -y "$deb" | tee /tmp/install.out
[ "$(dpkg-query -W -f='${Status}' gpueue)" = "install ok installed" ] || fail "not installed"
grep -q 'systemd is not running' /tmp/install.out || fail "postinst did not say that systemd is not running"
gpueue --version
must_exist /usr/bin/gpueue-helper /usr/lib/systemd/system/gpueue.service \
    /etc/apt/keyrings/gpueue.gpg /etc/apt/sources.list.d/gpueue.sources
for unit in gpueue.service gpueue-helper.service; do
    [ "$(head -n 1 /usr/lib/systemd/system/$unit)" != "$MARKER" ] || fail "$unit has the service install marker"
done
grep -qx 'ExecStart=/usr/bin/gpueue daemon' /usr/lib/systemd/system/gpueue.service || fail "daemon ExecStart="
grep -qx 'ExecStart=/usr/bin/gpueue-helper serve' /usr/lib/systemd/system/gpueue-helper.service || fail "helper ExecStart="
for target in reboot poweroff halt kexec soft-reboot; do
    grep -qx 'JobTimeoutSec=2h' "/usr/lib/systemd/system/$target.target.d/gpueue.conf" ||
        fail "$target.target.d/gpueue.conf does not raise JobTimeoutSec="
done
getent passwd gpueue >/dev/null || fail "no user gpueue"
getent group gpueue-admin >/dev/null || fail "no group gpueue-admin"
[ "$(stat -c '%U %G %a' /var/lib/gpueue)" = "root root 755" ] || fail "/var/lib/gpueue: $(stat -c '%U %G %a' /var/lib/gpueue)"
[ "$(stat -c '%U %a' /var/lib/gpueue/daemon)" = "gpueue 700" ] || fail "/var/lib/gpueue/daemon: $(stat -c '%U %a' /var/lib/gpueue/daemon)"
# Enabled offline, so the services would start on the next boot.
must_exist /etc/systemd/system/multi-user.target.wants/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue-helper.service
# apt reads the repository from gpueue.sources (not fetched: it does not exist yet).
apt-get indextargets --no-release-info 2>/dev/null | grep -q 'github.com/kaito4681/gpueue/releases/download/apt' ||
    fail "apt does not see the gpueue repository"

step "postinst warns about a CRIU older than 4.2, and does not run one root does not own"
# /usr/local/sbin comes first in the helper's PATH, before a distribution CRIU.
fake=/usr/local/sbin/criu
mkdir -p /usr/local/sbin
printf '#!/bin/sh\ntouch /tmp/criu-ran\necho "Version: 3.16"\n' >"$fake"
chmod 755 "$fake"
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
grep -q 'CRIU 4.2 or newer is needed to checkpoint (found 3.16 at /usr/local/sbin/criu)' /tmp/criu.err ||
    { cat /tmp/criu.err; fail "no warning about CRIU 3.16"; }
printf '#!/bin/sh\necho "Version: 4.2.1"\n' >"$fake"
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
! grep -q CRIU /tmp/criu.err || { cat /tmp/criu.err; fail "warned about CRIU 4.2.1"; }
printf '#!/bin/sh\ntouch /tmp/criu-ran\necho "Version: 3.16"\n' >"$fake"
chown 1000 "$fake"
rm -f /tmp/criu-ran
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
grep -q 'is not owned by root' /tmp/criu.err || { cat /tmp/criu.err; fail "no warning about a criu root does not own"; }
must_not_exist /tmp/criu-ran
rm -f "$fake"
# A root-owned CRIU on a path a user could redirect is not run either: in a
# directory a user owns (criu_path), or behind a symlink a user owns.
real=/usr/local/lib/gpueue-test-criu
mkdir -p /usr/local/lib
printf '#!/bin/sh\ntouch /tmp/criu-ran\necho "Version: 3.16"\n' >"$real"
chmod 755 "$real"
mkdir -p /srv/user-dir
cp -p "$real" /srv/user-dir/criu
chown 1000 /srv/user-dir
cp -p /etc/gpueue/config.toml /tmp/config.toml.orig
echo 'criu_path = "/srv/user-dir/criu"' >>/etc/gpueue/config.toml
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
grep -q 'is not owned by root' /tmp/criu.err || { cat /tmp/criu.err; fail "no warning about a criu in a directory a user owns"; }
must_not_exist /tmp/criu-ran
cp -p /tmp/config.toml.orig /etc/gpueue/config.toml
ln -s "$real" "$fake"
chown -h 1000 "$fake"
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
grep -q 'is not owned by root' /tmp/criu.err || { cat /tmp/criu.err; fail "no warning about a criu behind a symlink a user owns"; }
must_not_exist /tmp/criu-ran
# The same symlink owned by root is followed, and its target is what runs.
chown -h 0 "$fake"
/var/lib/dpkg/info/gpueue.postinst configure 0.0.0 >/dev/null 2>/tmp/criu.err
grep -q 'CRIU 4.2 or newer is needed to checkpoint (found 3.16 at /usr/local/sbin/criu)' /tmp/criu.err ||
    { cat /tmp/criu.err; fail "no warning about CRIU 3.16 behind a root-owned symlink"; }
must_exist /tmp/criu-ran
rm -rf "$fake" "$real" /srv/user-dir /tmp/criu-ran

step "a rolled-back removal enables the services again (prerm remove, postinst abort-remove)"
wants=/etc/systemd/system/multi-user.target.wants
# Also when /var/lib/gpueue is missing: prerm creates it for its marker.
rm -rf /var/lib/gpueue
/var/lib/dpkg/info/gpueue.prerm remove
must_not_exist "$wants/gpueue.service" "$wants/gpueue-helper.service"
must_exist /var/lib/gpueue/package-removed
[ "$(stat -c '%U %G %a' /var/lib/gpueue)" = "root root 755" ] || fail "/var/lib/gpueue: $(stat -c '%U %G %a' /var/lib/gpueue)"
/var/lib/dpkg/info/gpueue.postinst abort-remove
must_exist "$wants/gpueue.service" "$wants/gpueue-helper.service" /var/lib/gpueue/daemon
must_not_exist /var/lib/gpueue/package-removed
# Without the marker (prerm did not get that far) abort-remove changes nothing.
/var/lib/dpkg/info/gpueue.postinst abort-remove >/tmp/abort.out
[ ! -s /tmp/abort.out ] || { cat /tmp/abort.out; fail "abort-remove without the marker did something"; }

step "an edited conffile survives a reinstallation (the upgrade path)"
echo '# edited by deb-check' >>/etc/gpueue/config.toml
dpkg -i "$deb"
grep -q 'edited by deb-check' /etc/gpueue/config.toml || fail "config.toml edit lost"

step "dpkg -r keeps the configuration and the state"
mkdir -p /var/lib/gpueue/logs/1000
echo log >/var/lib/gpueue/logs/1000/job.log
dpkg -r gpueue
must_not_exist /usr/bin/gpueue /usr/bin/gpueue-helper /usr/lib/systemd/system/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue.service \
    /usr/lib/systemd/system/reboot.target.d/gpueue.conf
must_exist /etc/gpueue/config.toml /etc/apt/sources.list.d/gpueue.sources /etc/apt/keyrings/gpueue.gpg \
    /var/lib/gpueue/logs/1000/job.log
[ "$(dpkg-query -W -f='${Status}' gpueue)" = "deinstall ok config-files" ] || fail "not in config-files state"

step "a reinstallation after dpkg -r enables the services again"
dpkg -i "$deb"
must_exist /etc/systemd/system/multi-user.target.wants/gpueue.service
must_not_exist /var/lib/gpueue/package-removed
grep -q 'edited by deb-check' /etc/gpueue/config.toml || fail "config.toml edit lost"

step "dpkg -P deletes the configuration, keeps /var/lib/gpueue"
echo token >/etc/gpueue/github-token
dpkg -P gpueue | tee /tmp/purge.out
must_not_exist /etc/gpueue /etc/apt/sources.list.d/gpueue.sources /etc/apt/keyrings/gpueue.gpg
must_exist /var/lib/gpueue/logs/1000/job.log
grep -q 'rm -rf /var/lib/gpueue' /tmp/purge.out || fail "purge did not say how to delete /var/lib/gpueue"
case "$(dpkg-query -W -f='${Status}' gpueue 2>/dev/null || true)" in
*"ok installed" | *config-files) fail "still installed after purge" ;;
esac

printf '\nall .deb checks passed\n'
