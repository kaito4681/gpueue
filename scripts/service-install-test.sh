#!/usr/bin/env bash
# End-to-end test of `puzuz service install|status|uninstall` and `puzuz doctor`
# with a real systemd (PID 1) in a privileged Docker container. Nothing
# privileged runs on the host.
#
# Usage: scripts/service-install-test.sh [--keep]
# PUZUZ_SYSTEMD_BASE: base image (see scripts/helper-systemd/Dockerfile);
# use puzuz-poc/systemd259:0.1 where apt is not reachable.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_SYSTEMD_IMAGE:-puzuz-helper-systemd:test}"
name="puzuz-service-e2e-$$"
keep="${1:-}"

"$root/scripts/test-in-docker.sh" build -p puzuz -p puzuz-helper
docker build -q -t "$image" \
    --build-arg "BASE=${PUZUZ_SYSTEMD_BASE:-ubuntu:26.04}" \
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
cp "$root/target/docker/debug/puzuz" "$root/target/docker/debug/puzuz-helper" "$stage/"
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

step "install without root fails"
out="$(runuser -u alice -- /build/puzuz service install 2>&1)" && fail "non-root install succeeded"
grep -q "must run as root" <<<"$out" || fail "$out"

step "install from a non-system directory needs --bin-dir"
out="$(/build/puzuz service install 2>&1)" && fail "install without --bin-dir succeeded"
grep -q -- "--bin-dir /usr/local/bin" <<<"$out" || fail "$out"

step "install --bin-dir /usr/local/bin"
/build/puzuz service install --bin-dir /usr/local/bin | tee /tmp/install.out || true
cmp /build/puzuz /usr/local/bin/puzuz
cmp /build/puzuz-helper /usr/local/bin/puzuz-helper
systemctl is-active puzuz-helper.service puzuz.service
systemctl is-enabled puzuz-helper.service puzuz.service
getent passwd puzuz >/dev/null || fail "no puzuz user"
getent group puzuz-admin >/dev/null || fail "no puzuz-admin group"
test -f /etc/puzuz/config.toml || fail "no config"
test -S /run/puzuz/puzuz.sock || fail "no daemon socket"
test -f /var/lib/puzuz/daemon/state.sqlite || fail "no state db"
[ "$(stat -c %U /var/lib/puzuz)" = root ] || fail "/var/lib/puzuz not root-owned"
grep -q "OK    helper: connected" /tmp/install.out || fail "helper not connected after install"

step "status (as alice)"
runuser -u alice -- puzuz service status

step "doctor (as alice)"
runuser -u alice -- puzuz doctor || echo "(doctor exit $? — expected without GPUs/criu permissions)"

step "a job runs through the installed services"
id=$(runuser -u alice -- puzuz add -p -- echo hello-from-service | grep -o '[0-9]\+' | head -1)
runuser -u alice -- puzuz wait "$id" >/dev/null
ok=
for _ in $(seq 20); do
    if runuser -u alice -- puzuz log "$id" | grep -qx hello-from-service; then ok=1; break; fi
    sleep 0.5
done
[ -n "$ok" ] || fail "job output missing from its log"

step "re-running install is idempotent"
echo "grace_secs = 7" >> /etc/puzuz/config.toml
/usr/local/bin/puzuz service install > /tmp/install2.out || true
grep -q "unchanged /etc/systemd/system/puzuz.service" /tmp/install2.out || fail "units rewritten"
grep -q "kept the existing /etc/puzuz/config.toml" /tmp/install2.out || fail "config not kept"
grep -q "grace_secs = 7" /etc/puzuz/config.toml || fail "config overwritten"
systemctl is-active puzuz.service
[ "$(puzuz service install-method)" = service-install ] || fail "install method not detected"

step "a packaged unit next to the installed one is a conflict"
cp /etc/systemd/system/puzuz.service /tmp/puzuz.service.etc
sed 1d /tmp/puzuz.service.etc > /usr/lib/systemd/system/puzuz.service
[ "$(puzuz service install-method)" = mixed ] || fail "mixed setup not detected"
out="$(puzuz doctor 2>&1)" && fail "doctor passed a mixed setup"
grep -q "FAIL  install method: mixed: distribution package" <<<"$out" || fail "$out"
out="$(/usr/local/bin/puzuz service install --force 2>&1)" && fail "install --force over a package succeeded"
grep -q "distribution package" <<<"$out" || fail "$out"
out="$(/usr/local/bin/puzuz service uninstall --purge --yes 2>&1)" && fail "purge of a mixed setup succeeded"
grep -q "refusing to purge" <<<"$out" || fail "$out"
cmp /etc/systemd/system/puzuz.service /tmp/puzuz.service.etc || fail "installed unit changed"
rm /usr/lib/systemd/system/puzuz.service
[ "$(puzuz service install-method)" = service-install ] || fail "conflict not resolved"

step "uninstall keeps data"
/usr/local/bin/puzuz service uninstall
test ! -e /etc/systemd/system/puzuz.service || fail "unit left behind"
systemctl is-active puzuz.service && fail "daemon still active"
test -f /var/lib/puzuz/daemon/state.sqlite || fail "state removed without --purge"

step "uninstall --purge"
echo n | /usr/local/bin/puzuz service uninstall --purge && fail "purge without confirmation"
test -d /var/lib/puzuz || fail "purged although declined"
/usr/local/bin/puzuz service uninstall --purge --yes
test ! -e /var/lib/puzuz || fail "/var/lib/puzuz left"
test ! -e /etc/puzuz || fail "/etc/puzuz left"

step "install from /usr/local/bin needs no --bin-dir"
/usr/local/bin/puzuz service install --no-start > /tmp/install3.out || true
systemctl is-enabled puzuz.service
systemctl is-active puzuz.service && fail "--no-start started the daemon"

step "a package-managed machine is refused"
/usr/local/bin/puzuz service uninstall
printf '[Service]\nExecStart=/usr/bin/puzuz daemon\n' > /usr/lib/systemd/system/puzuz.service
[ "$(puzuz service install-method)" = package ] || fail "package not detected"
out="$(/usr/local/bin/puzuz service install --force 2>&1)" && fail "install over a package succeeded"
grep -q "package manager" <<<"$out" || fail "$out"
test ! -e /etc/systemd/system/puzuz.service || fail "install wrote a unit"
out="$(/usr/local/bin/puzuz service uninstall 2>&1)" && fail "uninstall of a package succeeded"
grep -q "not by \`puzuz service install\`" <<<"$out" || fail "$out"
rm /usr/lib/systemd/system/puzuz.service

step "NixOS is refused"
touch /etc/NIXOS
out="$(/usr/local/bin/puzuz service install 2>&1)" && fail "install on NixOS succeeded"
grep -q "installed as NixOS" <<<"$out" || fail "$out"
rm /etc/NIXOS
echo
echo "=== service e2e passed"
EOF
