#!/bin/sh
# Checks the gpueue .rpm inside a plain Fedora/RHEL-family container without
# systemd as PID 1 (run by scripts/rpm-test.sh): its metadata and contents,
# that installing next to `gpueue service install` is refused, that
# `dnf install ./gpueue-*.rpm` succeeds (the scriptlets must not fail without
# systemd) and sets up the user, directories and services, that
# `dnf upgrade ./<newer>.rpm` keeps an edited config.toml, and what
# `dnf remove` keeps and deletes.
#
# Usage (as root, in the container): rpm-check.sh OLD_RPM NEW_RPM
#   OLD_RPM must be an older version than NEW_RPM (e.g. a pre-release of it).
set -eu

old="$1"
new="$2"
MARKER='# Installed by `gpueue service install`; re-running it overwrites this file.'

step() { printf '\n=== %s\n' "$*"; }
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}
must_exist() { for f in "$@"; do [ -e "$f" ] || fail "missing $f"; done; }
must_not_exist() { for f in "$@"; do [ ! -e "$f" ] || fail "still there: $f"; done; }

step "metadata and contents"
rpm -qip "$old"
[ "$(rpm -qp --qf '%{NAME}' "$old")" = gpueue ] || fail "Name is not gpueue"
for f in "$old" "$new"; do
    case "$(rpm -qp --qf '%{VERSION}-%{RELEASE}.%{ARCH}' "$f")" in
    *-*-*) fail "$f: '-' in the version" ;;
    *-1.x86_64 | *-1.aarch64) ;;
    *) fail "$f: unexpected release/arch $(rpm -qp --qf '%{RELEASE}.%{ARCH}' "$f")" ;;
    esac
done
printf 'versions: %s -> %s\n' "$(rpm -qp --qf '%{VERSION}' "$old")" "$(rpm -qp --qf '%{VERSION}' "$new")"
[ "$(rpm --eval "%{lua: print(rpm.vercmp('$(rpm -qp --qf '%{EVR}' "$old")', '$(rpm -qp --qf '%{EVR}' "$new")'))}")" = -1 ] ||
    fail "$old is not older than $new"
rpm -qp --requires "$old" >/tmp/requires
cat /tmp/requires
grep -qx 'systemd' /tmp/requires || fail "no Requires: systemd"
grep -qx 'glibc >= 2.34' /tmp/requires || fail "no Requires: glibc >= 2.34"
grep -q criu /tmp/requires && fail "criu must not be a Requires"
rpm -qp --recommends "$old" | grep -qx criu || fail "criu is not recommended"
rpm -qp --qf '[%{FILENAMES} %{FILEFLAGS:fflags}\n]' "$old" >/tmp/contents
cat /tmp/contents
for f in /usr/bin/gpueue /usr/bin/gpueue-helper \
    /usr/lib/systemd/system/gpueue.service /usr/lib/systemd/system/gpueue-helper.service \
    /usr/lib/sysusers.d/gpueue.conf /usr/lib/tmpfiles.d/gpueue.conf /etc/gpueue /etc/gpueue/config.toml \
    /usr/share/bash-completion/completions/gpueue /usr/share/zsh/site-functions/_gpueue \
    /usr/share/fish/vendor_completions.d/gpueue.fish /usr/share/licenses/gpueue/LICENSE; do
    grep -q "^$f " /tmp/contents || fail "$f is not in the package"
done
grep -qx '/etc/gpueue/config.toml cn' /tmp/contents || fail "config.toml is not %config(noreplace)"
grep -qx '/usr/share/licenses/gpueue/LICENSE l' /tmp/contents || fail "LICENSE is not %license"
grep -q -e '^/etc/apt' -e '^/usr/share/keyrings' -e '^/etc/yum.repos.d' /tmp/contents &&
    fail "the .rpm must not ship APT or dnf repository files"

step "installing next to \`gpueue service install\` is refused"
mkdir -p /etc/systemd/system
printf '%s\n[Service]\nExecStart=/usr/local/bin/gpueue daemon\n' "$MARKER" >/etc/systemd/system/gpueue.service
if rpm -i --nodeps "$old" >/tmp/refused 2>&1; then fail "installed next to service install"; fi
grep -q 'sudo gpueue uninstall' /tmp/refused || { cat /tmp/refused; fail "no hint"; }
rpm -q gpueue >/dev/null 2>&1 && fail "installed after a failed %pre"
rm -f /etc/systemd/system/gpueue.service

step "dnf install ./$(basename "$old")"
dnf install -y "$old" 2>&1 | tee /tmp/install.out
[ "$(rpm -q --qf '%{VERSION}' gpueue)" = "$(rpm -qp --qf '%{VERSION}' "$old")" ] || fail "not installed"
grep -q 'systemd is not running' /tmp/install.out || fail "%post did not say that systemd is not running"
grep -qi 'scriptlet.*fail\|error:' /tmp/install.out && fail "a scriptlet failed"
gpueue --version
for unit in gpueue.service gpueue-helper.service; do
    [ "$(head -n 1 /usr/lib/systemd/system/$unit)" != "$MARKER" ] || fail "$unit has the service install marker"
done
grep -qx 'ExecStart=/usr/bin/gpueue daemon' /usr/lib/systemd/system/gpueue.service || fail "daemon ExecStart="
grep -qx 'ExecStart=/usr/bin/gpueue-helper serve' /usr/lib/systemd/system/gpueue-helper.service || fail "helper ExecStart="
getent passwd gpueue >/dev/null || fail "no user gpueue"
getent group gpueue-admin >/dev/null || fail "no group gpueue-admin"
[ "$(stat -c '%U %G %a' /var/lib/gpueue)" = "root root 755" ] || fail "/var/lib/gpueue: $(stat -c '%U %G %a' /var/lib/gpueue)"
[ "$(stat -c '%U %a' /var/lib/gpueue/daemon)" = "gpueue 700" ] || fail "/var/lib/gpueue/daemon: $(stat -c '%U %a' /var/lib/gpueue/daemon)"
[ "$(stat -c '%a' /etc/gpueue/config.toml)" = 644 ] || fail "config.toml mode"
# Enabled offline, so the services would start on the next boot.
must_exist /etc/systemd/system/multi-user.target.wants/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue-helper.service
rpm -V gpueue || fail "rpm -V gpueue reports changes right after the installation"

step "dnf upgrade ./$(basename "$new") keeps an edited config.toml"
echo '# edited by rpm-check' >>/etc/gpueue/config.toml
mkdir -p /var/lib/gpueue/logs/1000
echo log >/var/lib/gpueue/logs/1000/job.log
dnf upgrade -y "$new" 2>&1 | tee /tmp/upgrade.out
[ "$(rpm -q --qf '%{VERSION}' gpueue)" = "$(rpm -qp --qf '%{VERSION}' "$new")" ] || fail "not upgraded"
[ "$(rpm -q gpueue | wc -l)" = 1 ] || fail "more than one gpueue installed"
grep -qi 'scriptlet.*fail\|error:' /tmp/upgrade.out && fail "a scriptlet failed"
grep -q 'edited by rpm-check' /etc/gpueue/config.toml || fail "config.toml edit lost"
# The old package's %preun/%postun (argument 1) must not disable the services.
must_exist /etc/systemd/system/multi-user.target.wants/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue-helper.service \
    /var/lib/gpueue/logs/1000/job.log

step "dnf remove keeps the edited configuration (.rpmsave) and the state"
dnf remove -y gpueue 2>&1 | tee /tmp/remove.out
rpm -q gpueue >/dev/null 2>&1 && fail "still installed"
must_not_exist /usr/bin/gpueue /usr/bin/gpueue-helper /usr/lib/systemd/system/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue.service \
    /etc/systemd/system/multi-user.target.wants/gpueue-helper.service
must_exist /etc/gpueue/config.toml.rpmsave /var/lib/gpueue/logs/1000/job.log
must_not_exist /var/lib/gpueue/package-removed
getent passwd gpueue >/dev/null || fail "the user gpueue was deleted (it may own files in /var/lib/gpueue)"

step "a reinstallation after dnf remove enables the services again"
dnf install -y "$new"
must_exist /etc/systemd/system/multi-user.target.wants/gpueue.service /etc/gpueue/config.toml
dnf remove -y gpueue >/dev/null

printf '\nall .rpm checks passed\n'
