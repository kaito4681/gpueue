#!/usr/bin/env bash
# End-to-end test of `gpueue service install|status|uninstall` and `gpueue doctor`
# with a real systemd (PID 1) in a privileged Docker container. Nothing
# privileged runs on the host.
#
# Usage: scripts/service-install-test.sh [--keep]
# GPUEUE_SYSTEMD_BASE: base image (see scripts/helper-systemd/Dockerfile);
# use gpueue-poc/systemd259:0.1 where apt is not reachable.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${GPUEUE_SYSTEMD_IMAGE:-gpueue-helper-systemd:test}"
name="gpueue-service-e2e-$$"
keep="${1:-}"

"$root/scripts/test-in-docker.sh" build -p gpueue -p gpueue-helper
docker build -q -t "$image" \
    --build-arg "BASE=${GPUEUE_SYSTEMD_BASE:-ubuntu:26.04}" \
    "$root/scripts/helper-systemd" >/dev/null

stage="$(mktemp -d)"
cleanup() {
    if [ "$keep" != "--keep" ]; then
        docker rm -f "$name" >/dev/null 2>&1 || true
    else
        echo "container kept: docker exec -it $name bash"
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
cp "$root/target/docker/debug/gpueue" "$root/target/docker/debug/gpueue-helper" "$stage/"
chmod -R a+rX "$stage"

docker run -d --name "$name" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock \
    -v "$stage":/build:ro \
    "$image" >/dev/null
for _ in $(seq 50); do
    state="$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running | degraded) break ;; esac
    sleep 0.2
done
echo "container systemd: ${state:-unknown}"

docker exec -i "$name" bash -s <<'EOF'
set -euo pipefail
fail() { echo "FAIL: $*"; exit 1; }
step() { echo; echo "=== $*"; }

# A dummy .deb named gpueue that ships both units in /usr/lib/systemd/system.
make_deb() {
    local root=/tmp/gpueue-deb
    rm -rf "$root"
    mkdir -p "$root/DEBIAN" "$root/usr/lib/systemd/system"
    printf 'Package: gpueue\nVersion: 0.0.1\nArchitecture: all\nMaintainer: test <test@example.com>\nDescription: dummy gpueue package for the e2e test\n' >"$root/DEBIAN/control"
    printf '[Service]\nExecStart=/usr/bin/gpueue daemon\n' >"$root/usr/lib/systemd/system/gpueue.service"
    printf '[Service]\nExecStart=/usr/bin/gpueue-helper serve\n' >"$root/usr/lib/systemd/system/gpueue-helper.service"
    dpkg-deb --build --root-owner-group "$root" /tmp/gpueue.deb >/dev/null
}
make_deb

step "install without root fails"
out="$(runuser -u alice -- /build/gpueue service install 2>&1)" && fail "non-root install succeeded"
grep -q "must run as root" <<<"$out" || fail "$out"

step "install from a non-system directory copies the binaries to /usr/local/bin"
# /build is a mount owned by the invoking user: install from a root-owned copy.
install -d -m 0755 /opt/gpueue-src
install -m 0755 /build/gpueue /build/gpueue-helper /opt/gpueue-src/
/opt/gpueue-src/gpueue service install | tee /tmp/install.out || true
grep -q "installing into /usr/local/bin" /tmp/install.out || fail "no note about /usr/local/bin"
cmp /build/gpueue /usr/local/bin/gpueue
cmp /build/gpueue-helper /usr/local/bin/gpueue-helper
grep -q '^ExecStart=/usr/local/bin/gpueue daemon' /etc/systemd/system/gpueue.service || fail "unit does not run /usr/local/bin/gpueue"
[ "$(stat -c '%U %a' /var/lib/gpueue/install-receipt)" = "root 644" ] || fail "receipt not root 0644"
grep -qx 'copied = \["/usr/local/bin/gpueue", "/usr/local/bin/gpueue-helper"\]' /var/lib/gpueue/install-receipt ||
    fail "receipt: $(cat /var/lib/gpueue/install-receipt)"
systemctl is-active gpueue-helper.service gpueue.service
systemctl is-enabled gpueue-helper.service gpueue.service
getent passwd gpueue >/dev/null || fail "no gpueue user"
getent group gpueue-admin >/dev/null || fail "no gpueue-admin group"
test -f /etc/gpueue/config.toml || fail "no config"
test -S /run/gpueue/gpueue.sock || fail "no daemon socket"
test -f /var/lib/gpueue/daemon/state.sqlite || fail "no state db"
[ "$(stat -c %U /var/lib/gpueue)" = root ] || fail "/var/lib/gpueue not root-owned"
grep -q "OK    helper: connected" /tmp/install.out || fail "helper not connected after install"
# The drop-ins raise the shutdown targets' JobTimeoutSec= (30 min by default).
for target in reboot poweroff halt kexec; do
    [ "$(systemctl show -p JobTimeoutUSec --value "$target.target")" = 2h ] ||
        fail "$target.target: $(systemctl show -p JobTimeoutUSec -p JobRunningTimeoutUSec "$target.target")"
    [ "$(systemctl show -p JobRunningTimeoutUSec --value "$target.target")" = 2h ] ||
        fail "$target.target: JobRunningTimeoutUSec not raised"
done

step "status (as alice)"
runuser -u alice -- gpueue service status

step "doctor (as alice)"
runuser -u alice -- gpueue doctor || echo "(doctor exit $? — expected without GPUs/criu permissions)"

step "a job runs through the installed services"
id=$(runuser -u alice -- gpueue add -p -- echo hello-from-service | grep -o '[0-9]\+' | head -1)
runuser -u alice -- gpueue wait "$id" >/dev/null
ok=
for _ in $(seq 20); do
    if runuser -u alice -- gpueue log "$id" | grep -qx hello-from-service; then ok=1; break; fi
    sleep 0.5
done
[ -n "$ok" ] || fail "job output missing from its log"

step "re-running install is idempotent"
echo "grace_secs = 7" >> /etc/gpueue/config.toml
/usr/local/bin/gpueue service install > /tmp/install2.out || true
grep -q "unchanged /etc/systemd/system/gpueue.service" /tmp/install2.out || fail "units rewritten"
grep -q "kept the existing /etc/gpueue/config.toml" /tmp/install2.out || fail "config not kept"
grep -q "grace_secs = 7" /etc/gpueue/config.toml || fail "config overwritten"
grep -q "installing into" /tmp/install2.out && fail "a re-run chose a directory again"
grep -q 'gpueue-helper"\]$' /var/lib/gpueue/install-receipt || fail "a re-run forgot the copies"
systemctl is-active gpueue.service
[ "$(gpueue service install-method)" = service-install ] || fail "install method not detected"

step "a packaged unit next to the installed one is a conflict"
cp /etc/systemd/system/gpueue.service /tmp/gpueue.service.etc
dpkg -i /tmp/gpueue.deb >/dev/null
[ "$(gpueue service install-method)" = mixed ] || fail "mixed setup not detected"
out="$(gpueue doctor 2>&1)" && fail "doctor passed a mixed setup"
grep -q "FAIL  install method: mixed: distribution package (gpueue, dpkg" <<<"$out" || fail "$out"
out="$(/usr/local/bin/gpueue service install --force 2>&1)" && fail "install --force over a package succeeded"
grep -q "distribution package" <<<"$out" || fail "$out"
out="$(/usr/local/bin/gpueue uninstall --purge --yes 2>&1)" && fail "purge of a mixed setup succeeded"
grep -q "refusing to purge" <<<"$out" || fail "$out"
cmp /etc/systemd/system/gpueue.service /tmp/gpueue.service.etc || fail "installed unit changed"
dpkg -r gpueue >/dev/null
[ "$(gpueue service install-method)" = service-install ] || fail "conflict not resolved"

step "gpueue uninstall keeps data and removes the copied binaries"
/usr/local/bin/gpueue uninstall | tee /tmp/uninstall.out
test ! -e /etc/systemd/system/gpueue.service || fail "unit left behind"
for target in reboot poweroff halt kexec soft-reboot; do
    test ! -e "/etc/systemd/system/$target.target.d" || fail "$target.target.d left behind"
done
[ "$(systemctl show -p JobTimeoutUSec --value reboot.target)" = 30min ] ||
    fail "reboot.target still has JobTimeoutUSec=$(systemctl show -p JobTimeoutUSec --value reboot.target)"
systemctl is-active gpueue.service && fail "daemon still active"
test -f /var/lib/gpueue/daemon/state.sqlite || fail "state removed without --purge"
test ! -e /usr/local/bin/gpueue || fail "copied gpueue left behind"
test ! -e /usr/local/bin/gpueue-helper || fail "copied gpueue-helper left behind"
test ! -e /var/lib/gpueue/install-receipt || fail "receipt left behind"
test -x /build/gpueue || fail "the source binary was removed"

step "uninstall --purge (also as service uninstall)"
/opt/gpueue-src/gpueue service install >/dev/null || true
echo n | /usr/local/bin/gpueue uninstall --purge && fail "purge without confirmation"
test -d /var/lib/gpueue || fail "purged although declined"
/usr/local/bin/gpueue service uninstall --purge --yes
test ! -e /var/lib/gpueue || fail "/var/lib/gpueue left"
test ! -e /etc/gpueue || fail "/etc/gpueue left"
test ! -e /usr/local/bin/gpueue || fail "copied gpueue left after service uninstall --purge"

step "binaries already in /usr/local/bin are used in place and kept by uninstall"
install -m 0755 /build/gpueue /build/gpueue-helper /usr/local/bin/
/usr/local/bin/gpueue service install --no-start > /tmp/install3.out || true
grep -q "installing into" /tmp/install3.out && fail "in-place install chose a directory"
grep -qx 'copied = \[\]' /var/lib/gpueue/install-receipt || fail "receipt: $(cat /var/lib/gpueue/install-receipt)"
systemctl is-enabled gpueue.service
systemctl is-active gpueue.service && fail "--no-start started the daemon"

step "a package-managed machine is refused"
/usr/local/bin/gpueue uninstall
test -x /usr/local/bin/gpueue || fail "uninstall removed a binary it did not copy"
dpkg -i /tmp/gpueue.deb >/dev/null
[ "$(gpueue service install-method)" = package ] || fail "package not detected"
out="$(/usr/local/bin/gpueue service install --force 2>&1)" && fail "install over a package succeeded"
grep -q "distribution package (gpueue, dpkg" <<<"$out" || fail "$out"
grep -q "sudo apt update && sudo apt upgrade" <<<"$out" || fail "$out"
test ! -e /etc/systemd/system/gpueue.service || fail "install wrote a unit"
out="$(/usr/local/bin/gpueue uninstall 2>&1)" && fail "uninstall of a package succeeded"
grep -q "not by \`gpueue service install\`" <<<"$out" || fail "$out"
grep -q "sudo apt remove gpueue" <<<"$out" || fail "$out"
dpkg -r gpueue >/dev/null

step "a unit no package owns needs --force"
printf '[Service]\nExecStart=/usr/bin/gpueue daemon\n' > /usr/lib/systemd/system/gpueue.service
printf '[Service]\nExecStart=/usr/bin/gpueue-helper serve\n' > /usr/lib/systemd/system/gpueue-helper.service
[ "$(gpueue service install-method)" = unowned ] || fail "unowned unit not detected"
runuser -u alice -- gpueue doctor > /tmp/doctor-unowned.out 2>&1 || true
grep -q "WARN  install method: a unit no package owns" /tmp/doctor-unowned.out || fail "$(cat /tmp/doctor-unowned.out)"
out="$(/usr/local/bin/gpueue service install --no-start 2>&1)" && fail "install over an unowned unit succeeded"
grep -q "no package owns /usr/lib/systemd/system/gpueue.service" <<<"$out" || fail "$out"
/usr/local/bin/gpueue service install --no-start --force > /tmp/install-unowned.out || true
grep -q "warning: no package owns" /tmp/install-unowned.out || fail "$(cat /tmp/install-unowned.out)"
[ "$(gpueue service install-method)" = service-install ] || fail "install --force did not take over"
runuser -u alice -- gpueue doctor > /tmp/doctor-leftover.out 2>&1 || true
grep -q "WARN  install method: gpueue service install .*no package owns" /tmp/doctor-leftover.out || fail "$(cat /tmp/doctor-leftover.out)"
/usr/local/bin/gpueue service uninstall
rm /usr/lib/systemd/system/gpueue.service /usr/lib/systemd/system/gpueue-helper.service

step "units from /nix/store are refused"
ln -s /nix/store/abc-gpueue.service /etc/systemd/system/gpueue.service
[ "$(gpueue service install-method)" = nix ] || fail "nix unit not detected"
out="$(/usr/local/bin/gpueue service install --force 2>&1)" && fail "install over a nix unit succeeded"
grep -q "installed as Nix" <<<"$out" || fail "$out"
rm /etc/systemd/system/gpueue.service

step "NixOS without gpueue is refused"
cp -P /etc/os-release /tmp/os-release.saved
rm /etc/os-release
printf 'NAME=NixOS\nID=nixos\n' > /etc/os-release
[ "$(gpueue service install-method)" = none ] || fail "NixOS without units is not none"
out="$(/usr/local/bin/gpueue service install 2>&1)" && fail "install on NixOS succeeded"
grep -q "this is NixOS" <<<"$out" || fail "$out"
runuser -u alice -- gpueue doctor > /tmp/doctor-nixos.out 2>&1 || true
grep -q "INFO  install method: not installed (NixOS" /tmp/doctor-nixos.out || fail "$(cat /tmp/doctor-nixos.out)"
rm /etc/os-release
mv /tmp/os-release.saved /etc/os-release
echo
echo "=== service e2e passed"
EOF
