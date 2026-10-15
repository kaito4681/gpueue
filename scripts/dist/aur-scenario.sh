#!/usr/bin/env bash
# Runs inside the Arch Linux container of scripts/dist/aur-test.sh (as root, systemd as
# PID 1). /stage holds the updated package directory (pkg/) and the fake release (repo/).
set -euo pipefail

step() { echo; echo "=== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

services_up() {
    systemctl is-active -q gpueue-helper.service gpueue.service &&
        runuser -u alice -- gpueue status >/dev/null 2>&1
}
wait_for() {
    local _
    for _ in $(seq 100); do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}
log_lines() { wc -l <"/var/lib/gpueue/logs/1000/$1.log"; }
job_alive() {
    local before
    systemctl is-active -q "gpueue-job-$1.service" || return 1
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
pkg="$(ls /home/builder/pkg/gpueue-bin-*-1-x86_64.pkg.tar.zst)"

step "pacman -U"
pacman -U --noconfirm "$pkg"
for f in /usr/bin/gpueue /usr/bin/gpueue-helper \
    /usr/lib/systemd/system/gpueue.service /usr/lib/systemd/system/gpueue-helper.service \
    /usr/lib/sysusers.d/gpueue.conf /usr/lib/tmpfiles.d/gpueue.conf /etc/gpueue/config.toml \
    /usr/share/bash-completion/completions/gpueue /usr/share/zsh/site-functions/_gpueue \
    /usr/share/fish/vendor_completions.d/gpueue.fish /usr/share/licenses/gpueue-bin/LICENSE \
    /usr/share/doc/gpueue-bin/install.md /usr/lib/systemd/system/reboot.target.d/gpueue.conf \
    /usr/lib/systemd/system/poweroff.target.d/gpueue.conf /usr/lib/systemd/system/halt.target.d/gpueue.conf \
    /usr/lib/systemd/system/kexec.target.d/gpueue.conf /usr/lib/systemd/system/soft-reboot.target.d/gpueue.conf; do
    [ -s "$f" ] || fail "$f is missing or empty"
done
grep -qx 'ExecStart=/usr/bin/gpueue daemon' /usr/lib/systemd/system/gpueue.service ||
    fail "gpueue.service does not run /usr/bin/gpueue"
grep -q 'gpueue' /usr/share/bash-completion/completions/gpueue || fail "empty bash completion"
pacman -Qkk gpueue-bin
# The pacman hooks of systemd.
getent passwd gpueue >/dev/null || fail "no gpueue user (sysusers hook)"
getent group gpueue-admin >/dev/null || fail "no gpueue-admin group (sysusers hook)"
[ "$(stat -c %U /var/lib/gpueue/daemon)" = gpueue ] || fail "no /var/lib/gpueue/daemon (tmpfiles hook)"
[ "$(stat -c %U:%a /etc/gpueue /etc/gpueue/config.toml)" = "$(printf 'root:755\nroot:644')" ] ||
    fail "/etc/gpueue must be owned by root"
systemctl is-enabled -q gpueue.service && fail "gpueue.service was enabled by the package"
[ "$(systemctl show -p JobTimeoutUSec --value reboot.target)" = 2h ] ||
    fail "reboot.target: JobTimeoutUSec=$(systemctl show -p JobTimeoutUSec --value reboot.target)"

step "enable the services and run a job"
systemctl enable --now gpueue-helper.service gpueue.service
wait_for services_up || { systemctl status --no-pager gpueue-helper.service gpueue.service || true; fail "services did not come up"; }
gpueue --version
gpueue doctor || echo "(doctor reported failures; criu is not installed)"
gpueue doctor 2>&1 | grep -i "install method" || true
id="$(cd /home/alice && runuser -u alice -- gpueue add -p -- echo hello-from-aur | grep -o '[0-9]\+' | head -n1)"
(cd /home/alice && runuser -u alice -- gpueue wait "$id")
runuser -u alice -- gpueue log "$id" | grep -q hello-from-aur || fail "job $id did not run"
echo "job $id ran"
out="$(gpueue update --yes 2>&1 || true)"
grep -q "yay -Syu" <<<"$out" ||
    fail "gpueue update did not refuse the pacman package and name yay/pacman: $out"
echo "gpueue update refuses the package installation: $out"

step "upgrade (pkgrel 2) while a job runs"
long="$(cd /home/alice && runuser -u alice -- gpueue add -p -- \
    'i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done' | grep -o '[0-9]\+' | head -n1)"
wait_for job_alive "$long" || fail "job $long does not run"
echo "# changed by the administrator" >>/etc/gpueue/config.toml
build 2
pacman -U --noconfirm /home/builder/pkg/gpueue-bin-*-2-x86_64.pkg.tar.zst
[ "$(pacman -Q gpueue-bin)" = "gpueue-bin $(sed -n 's/^pkgver=//p' /stage/pkg/PKGBUILD)-2" ] ||
    fail "not upgraded"
grep -q "changed by the administrator" /etc/gpueue/config.toml || fail "config.toml was replaced"
systemctl restart gpueue-helper.service gpueue.service
wait_for services_up || fail "services did not come back"
job_alive "$long" || fail "job $long stopped during the upgrade"
echo "job $long survived the upgrade"
runuser -u alice -- gpueue kill "$long"

if [ "${GPUEUE_AUR_NAMCAP:-1}" = 1 ]; then
    step "namcap"
    if pacman -Sy --noconfirm --needed namcap >/dev/null 2>&1; then
        (cd /home/builder/pkg && namcap PKGBUILD && namcap gpueue-bin-*-2-x86_64.pkg.tar.zst) || true
    else
        echo "namcap could not be installed; skipped"
    fi
fi

step "pacman -R"
systemctl disable --now gpueue.service gpueue-helper.service
pacman -R --noconfirm gpueue-bin
for f in /usr/bin/gpueue /usr/bin/gpueue-helper /usr/lib/systemd/system/gpueue.service; do
    [ ! -e "$f" ] || fail "$f was not removed"
done
[ -d /var/lib/gpueue ] || fail "/var/lib/gpueue should be kept"
ls /etc/gpueue
echo
echo "aur-test: all checks passed"
