#!/usr/bin/env bash
# Runs inside the Arch Linux container of scripts/dist/aur-test.sh (as root, systemd as
# PID 1). /stage holds the updated package directory (pkg/) and the fake release (repo/).
set -euo pipefail

step() { echo; echo "=== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

services_up() {
    systemctl is-active -q puzuz-helper.service puzuz.service &&
        runuser -u alice -- puzuz status >/dev/null 2>&1
}
wait_for() {
    local _
    for _ in $(seq 100); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}
log_lines() { wc -l <"/var/lib/puzuz/logs/1000/$1.log"; }
job_alive() {
    local before
    systemctl is-active -q "puzuz-job-$1.service" || return 1
    before="$(log_lines "$1")"
    sleep 1
    [ "$(log_lines "$1")" -gt "$before" ]
}

# build PKGREL: builds the package as an unprivileged user from the fake release.
build() {
    rm -rf /home/builder/pkg
    cp -r /stage/pkg /home/builder/pkg
    sed -i -e 's|\${url}/|file:///stage/repo/|g' -e "s|^pkgrel=.*|pkgrel=$1|" /home/builder/pkg/PKGBUILD
    chown -R builder: /home/builder/pkg
    (cd /home/builder/pkg && runuser -u builder -- makepkg --noconfirm --cleanbuild)
}

step "users"
# The archlinux image's pacman.conf skips /usr/share/doc and more (NoExtract).
sed -i '/^NoExtract/d' /etc/pacman.conf
useradd -m -u 1001 builder
useradd -m -u 1000 alice

step "makepkg"
build 1
pkg="$(ls /home/builder/pkg/puzuz-bin-*-1-x86_64.pkg.tar.zst)"

step "pacman -U"
pacman -U --noconfirm "$pkg"
for f in /usr/bin/puzuz /usr/bin/puzuz-helper \
    /usr/lib/systemd/system/puzuz.service /usr/lib/systemd/system/puzuz-helper.service \
    /usr/lib/sysusers.d/puzuz.conf /usr/lib/tmpfiles.d/puzuz.conf /etc/puzuz/config.toml \
    /usr/share/bash-completion/completions/puzuz /usr/share/zsh/site-functions/_puzuz \
    /usr/share/fish/vendor_completions.d/puzuz.fish /usr/share/licenses/puzuz-bin/LICENSE \
    /usr/share/doc/puzuz-bin/install.md; do
    [ -s "$f" ] || fail "$f is missing or empty"
done
grep -qx 'ExecStart=/usr/bin/puzuz daemon' /usr/lib/systemd/system/puzuz.service ||
    fail "puzuz.service does not run /usr/bin/puzuz"
grep -q 'puzuz' /usr/share/bash-completion/completions/puzuz || fail "empty bash completion"
pacman -Qkk puzuz-bin
# The pacman hooks of systemd.
getent passwd puzuz >/dev/null || fail "no puzuz user (sysusers hook)"
getent group puzuz-admin >/dev/null || fail "no puzuz-admin group (sysusers hook)"
[ "$(stat -c %U /var/lib/puzuz/daemon)" = puzuz ] || fail "no /var/lib/puzuz/daemon (tmpfiles hook)"
[ "$(stat -c %U:%a /etc/puzuz /etc/puzuz/config.toml)" = "$(printf 'root:755\nroot:644')" ] ||
    fail "/etc/puzuz must be owned by root"
systemctl is-enabled -q puzuz.service && fail "puzuz.service was enabled by the package"

step "enable the services and run a job"
systemctl enable --now puzuz-helper.service puzuz.service
wait_for services_up || { systemctl status --no-pager puzuz-helper.service puzuz.service || true; fail "services did not come up"; }
puzuz --version
puzuz doctor || echo "(doctor reported failures; criu is not installed)"
puzuz doctor 2>&1 | grep -i "install method" || true
id="$(cd /home/alice && runuser -u alice -- puzuz add -p -- echo hello-from-aur | grep -o '[0-9]\+' | head -n1)"
(cd /home/alice && runuser -u alice -- puzuz wait "$id")
runuser -u alice -- puzuz log "$id" | grep -q hello-from-aur || fail "job $id did not run"
echo "job $id ran"
out="$(puzuz update --yes 2>&1 || true)"
grep -q "yay -Syu" <<<"$out" ||
    fail "puzuz update did not refuse the pacman package and name yay/pacman: $out"
echo "puzuz update refuses the package installation: $out"

step "upgrade (pkgrel 2) while a job runs"
long="$(cd /home/alice && runuser -u alice -- puzuz add -p -- \
    'i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done' | grep -o '[0-9]\+' | head -n1)"
wait_for job_alive "$long" || fail "job $long does not run"
echo "# changed by the administrator" >>/etc/puzuz/config.toml
build 2
pacman -U --noconfirm /home/builder/pkg/puzuz-bin-*-2-x86_64.pkg.tar.zst
[ "$(pacman -Q puzuz-bin)" = "puzuz-bin $(sed -n 's/^pkgver=//p' /stage/pkg/PKGBUILD)-2" ] ||
    fail "not upgraded"
grep -q "changed by the administrator" /etc/puzuz/config.toml || fail "config.toml was replaced"
systemctl restart puzuz-helper.service puzuz.service
wait_for services_up || fail "services did not come back"
job_alive "$long" || fail "job $long stopped during the upgrade"
echo "job $long survived the upgrade"
runuser -u alice -- puzuz kill "$long"

if [ "${PUZUZ_AUR_NAMCAP:-1}" = 1 ]; then
    step "namcap"
    if pacman -Sy --noconfirm --needed namcap >/dev/null 2>&1; then
        (cd /home/builder/pkg && namcap PKGBUILD && namcap puzuz-bin-*-2-x86_64.pkg.tar.zst) || true
    else
        echo "namcap could not be installed; skipped"
    fi
fi

step "pacman -R"
systemctl disable --now puzuz.service puzuz-helper.service
pacman -R --noconfirm puzuz-bin
for f in /usr/bin/puzuz /usr/bin/puzuz-helper /usr/lib/systemd/system/puzuz.service; do
    [ ! -e "$f" ] || fail "$f was not removed"
done
[ -d /var/lib/puzuz ] || fail "/var/lib/puzuz should be kept"
ls /etc/puzuz
echo
echo "aur-test: all checks passed"
