#!/bin/sh
# Checks the puzuz .rpm inside a plain Fedora/RHEL-family container without
# systemd as PID 1 (run by scripts/rpm-test.sh): its metadata and contents,
# that installing next to `puzuz service install` is refused, that
# `dnf install ./puzuz-*.rpm` succeeds (the scriptlets must not fail without
# systemd) and sets up the user, directories and services, that
# `dnf upgrade ./<newer>.rpm` keeps an edited config.toml, and what
# `dnf remove` keeps and deletes.
#
# Usage (as root, in the container): rpm-check.sh OLD_RPM NEW_RPM
#   OLD_RPM must be an older version than NEW_RPM (e.g. a pre-release of it).
set -eu

old="$1"
new="$2"
MARKER='# Installed by `puzuz service install`; re-running it overwrites this file.'

step() { printf '\n=== %s\n' "$*"; }
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}
must_exist() { for f in "$@"; do [ -e "$f" ] || fail "missing $f"; done; }
must_not_exist() { for f in "$@"; do [ ! -e "$f" ] || fail "still there: $f"; done; }

step "metadata and contents"
rpm -qip "$old"
[ "$(rpm -qp --qf '%{NAME}' "$old")" = puzuz ] || fail "Name is not puzuz"
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
for f in /usr/bin/puzuz /usr/bin/puzuz-helper \
    /usr/lib/systemd/system/puzuz.service /usr/lib/systemd/system/puzuz-helper.service \
    /usr/lib/sysusers.d/puzuz.conf /usr/lib/tmpfiles.d/puzuz.conf /etc/puzuz /etc/puzuz/config.toml \
    /usr/share/bash-completion/completions/puzuz /usr/share/zsh/site-functions/_puzuz \
    /usr/share/fish/vendor_completions.d/puzuz.fish /usr/share/licenses/puzuz/LICENSE; do
    grep -q "^$f " /tmp/contents || fail "$f is not in the package"
done
grep -qx '/etc/puzuz/config.toml cn' /tmp/contents || fail "config.toml is not %config(noreplace)"
grep -qx '/usr/share/licenses/puzuz/LICENSE l' /tmp/contents || fail "LICENSE is not %license"
grep -q -e '^/etc/apt' -e '^/usr/share/keyrings' -e '^/etc/yum.repos.d' /tmp/contents &&
    fail "the .rpm must not ship APT or dnf repository files"

step "installing next to \`puzuz service install\` is refused"
mkdir -p /etc/systemd/system
printf '%s\n[Service]\nExecStart=/usr/local/bin/puzuz daemon\n' "$MARKER" >/etc/systemd/system/puzuz.service
if rpm -i --nodeps "$old" >/tmp/refused 2>&1; then fail "installed next to service install"; fi
grep -q 'puzuz service uninstall' /tmp/refused || { cat /tmp/refused; fail "no hint"; }
rpm -q puzuz >/dev/null 2>&1 && fail "installed after a failed %pre"
rm -f /etc/systemd/system/puzuz.service

step "dnf install ./$(basename "$old")"
dnf install -y "$old" 2>&1 | tee /tmp/install.out
[ "$(rpm -q --qf '%{VERSION}' puzuz)" = "$(rpm -qp --qf '%{VERSION}' "$old")" ] || fail "not installed"
grep -q 'systemd is not running' /tmp/install.out || fail "%post did not say that systemd is not running"
grep -qi 'scriptlet.*fail\|error:' /tmp/install.out && fail "a scriptlet failed"
puzuz --version
for unit in puzuz.service puzuz-helper.service; do
    [ "$(head -n 1 /usr/lib/systemd/system/$unit)" != "$MARKER" ] || fail "$unit has the service install marker"
done
grep -qx 'ExecStart=/usr/bin/puzuz daemon' /usr/lib/systemd/system/puzuz.service || fail "daemon ExecStart="
grep -qx 'ExecStart=/usr/bin/puzuz-helper serve' /usr/lib/systemd/system/puzuz-helper.service || fail "helper ExecStart="
getent passwd puzuz >/dev/null || fail "no user puzuz"
getent group puzuz-admin >/dev/null || fail "no group puzuz-admin"
[ "$(stat -c '%U %G %a' /var/lib/puzuz)" = "root root 755" ] || fail "/var/lib/puzuz: $(stat -c '%U %G %a' /var/lib/puzuz)"
[ "$(stat -c '%U %a' /var/lib/puzuz/daemon)" = "puzuz 700" ] || fail "/var/lib/puzuz/daemon: $(stat -c '%U %a' /var/lib/puzuz/daemon)"
[ "$(stat -c '%a' /etc/puzuz/config.toml)" = 644 ] || fail "config.toml mode"
# Enabled offline, so the services would start on the next boot.
must_exist /etc/systemd/system/multi-user.target.wants/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz-helper.service
rpm -V puzuz || fail "rpm -V puzuz reports changes right after the installation"

step "dnf upgrade ./$(basename "$new") keeps an edited config.toml"
echo '# edited by rpm-check' >>/etc/puzuz/config.toml
mkdir -p /var/lib/puzuz/logs/1000
echo log >/var/lib/puzuz/logs/1000/job.log
dnf upgrade -y "$new" 2>&1 | tee /tmp/upgrade.out
[ "$(rpm -q --qf '%{VERSION}' puzuz)" = "$(rpm -qp --qf '%{VERSION}' "$new")" ] || fail "not upgraded"
[ "$(rpm -q puzuz | wc -l)" = 1 ] || fail "more than one puzuz installed"
grep -qi 'scriptlet.*fail\|error:' /tmp/upgrade.out && fail "a scriptlet failed"
grep -q 'edited by rpm-check' /etc/puzuz/config.toml || fail "config.toml edit lost"
# The old package's %preun/%postun (argument 1) must not disable the services.
must_exist /etc/systemd/system/multi-user.target.wants/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz-helper.service \
    /var/lib/puzuz/logs/1000/job.log

step "dnf remove keeps the edited configuration (.rpmsave) and the state"
dnf remove -y puzuz 2>&1 | tee /tmp/remove.out
rpm -q puzuz >/dev/null 2>&1 && fail "still installed"
must_not_exist /usr/bin/puzuz /usr/bin/puzuz-helper /usr/lib/systemd/system/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz.service \
    /etc/systemd/system/multi-user.target.wants/puzuz-helper.service
must_exist /etc/puzuz/config.toml.rpmsave /var/lib/puzuz/logs/1000/job.log
must_not_exist /var/lib/puzuz/package-removed
getent passwd puzuz >/dev/null || fail "the user puzuz was deleted (it may own files in /var/lib/puzuz)"

step "a reinstallation after dnf remove enables the services again"
dnf install -y "$new"
must_exist /etc/systemd/system/multi-user.target.wants/puzuz.service /etc/puzuz/config.toml
dnf remove -y puzuz >/dev/null

printf '\nall .rpm checks passed\n'
