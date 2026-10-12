#!/usr/bin/env bash
# Real-reboot test: puzuz installed with `puzuz service install` in a
# QEMU/KVM virtual machine (Ubuntu 26.04 minimal cloud image, systemd 259,
# criu 4.2), which is rebooted and powered off for real, with CPU-only jobs.
# The VM runs inside an unprivileged Docker container that only gets
# --device /dev/kvm (and no AppArmor profile, see below); nothing runs
# privileged on the host.
#
# Scenarios (see scripts/vm/guest.sh):
#   reboot    counters (alice, bob) and a --no-checkpoint job, `systemctl reboot`
#   poweroff  the same with `systemctl poweroff` and a fresh boot of the VM
#   inflight  `systemctl reboot` while `puzuz checkpoint` of a 1 GiB job runs
# After each boot the previous boot's journal is checked for the stop order
# (daemon, helper dumps, job units, /var/lib/puzuz unmounted) and the jobs
# are restored. The shutdown duration is measured on the serial console.
#
# The prepared VM image (~1 GB) and the ssh key are cached in the Docker
# volume puzuz-vm-cache; each run uses throw-away overlays inside the
# container. Remove the cache with `docker volume rm puzuz-vm-cache`.
#
# Usage: scripts/vm-reboot-test.sh [scenario...]   (default: all three)
#        scripts/vm-reboot-test.sh prepare-only     (only build the cached image)
# Environment:
#   PUZUZ_VM_SKIP_BUILD=1  reuse target/docker/release binaries
#   PUZUZ_VM_CACHE         cache volume (default puzuz-vm-cache)
#   PUZUZ_VM_KEEP=1        on a failure, keep the VM running for debugging
#                          (`docker exec -it puzuz-vm-<pid> ssh -F /work/ssh_config vm`;
#                          `docker rm -f` the container when done)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_VM_RUNNER_IMAGE:-puzuz-vm-runner:test}"
cache="${PUZUZ_VM_CACHE:-puzuz-vm-cache}"
name="puzuz-vm-$$"

if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "/dev/kvm is not accessible (KVM is required)" >&2
    exit 1
fi
if [ "${PUZUZ_VM_SKIP_BUILD:-}" != 1 ]; then
    "$root/scripts/test-in-docker.sh" build --release -p puzuz -p puzuz-helper
fi

stage="$(mktemp -d)"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

mkdir "$stage/ctx" "$stage/bin" "$stage/e2e"
cp "$root/scripts/vm/Dockerfile" "$stage/ctx/"
cp /etc/ssl/certs/ca-certificates.crt "$stage/ctx/ca.crt"
docker build -q -t "$image" "$stage/ctx" >/dev/null

cp "$root/target/docker/release/puzuz" "$root/target/docker/release/puzuz-helper" "$stage/bin/"
cp "$root"/scripts/e2e/*.sh "$root"/scripts/e2e/*.py "$root"/scripts/e2e/*.toml "$stage/e2e/"
cp -r "$root/scripts/vm" "$stage/vm"
chmod -R a+rX "$stage"

# apparmor=unconfined: under docker-default, QEMU may not signal its own
# vCPU threads where the profile is stacked (rootless/userns Docker:
# "qemu:cpus_kick_thread: Permission denied"). The container is otherwise
# unprivileged.
docker run --rm --name "$name" -e PUZUZ_VM_KEEP --device /dev/kvm --security-opt apparmor=unconfined \
    -v "$cache":/cache -v "$stage":/stage:ro \
    "$image" bash /stage/vm/run.sh "$@"
