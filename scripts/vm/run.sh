#!/bin/bash
# Runs inside the runner container of scripts/vm-reboot-test.sh: prepares
# (once, cached in /cache) and boots the VM, installs puzuz in it and drives
# the reboot scenarios of guest.sh.
#   /stage/bin  release binaries   /stage/e2e  scripts/e2e   /stage/vm  this directory
set -euo pipefail

CACHE=/cache
WORK=/work
IMG_BASE=https://cloud-images.ubuntu.com/minimal/releases/resolute/release
IMG_NAME=ubuntu-26.04-minimal-cloudimg-amd64.img
# Bump the suffix when the provisioning below changes.
PREPARED=$CACHE/prepared-v1.qcow2
KEY=$CACHE/id_ed25519
SERIAL=$WORK/serial.log
MEM_MB=4096
QEMU_PID=

mkdir -p "$WORK"
log() { echo "[$(date +%T)] $*"; }
die() {
    log "ERROR: $*"
    [ -f "$SERIAL" ] && { echo "--- tail of the serial console"; tail -n 60 "$SERIAL"; }
    if [ "${PUZUZ_VM_KEEP:-}" = 1 ]; then
        log "keeping the VM: ssh -F $WORK/ssh_config vm"
        sleep infinity
    fi
    exit 1
}

cat >"$WORK/ssh_config" <<EOF
Host vm
    HostName 127.0.0.1
    Port 2222
    User root
    IdentityFile $KEY
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
    ConnectTimeout 5
    ServerAliveInterval 2
    ServerAliveCountMax 3
EOF
# shellcheck disable=SC2032,SC2033  # "vm" is also the ssh host alias
vm() { ssh -F "$WORK/ssh_config" vm "$@"; }

# start_vm <root disk> [qemu args...]: boots the VM in the background; the
# serial console goes to $SERIAL with a wall-clock timestamp per line. The
# root disk has bootindex=0: without it SeaBIOS may try the (empty) state
# disk first and hang there.
start_vm() {
    local disk=$1
    shift
    qemu-system-x86_64 -enable-kvm -cpu host -smp 2 -m "$MEM_MB" \
        -display none -monitor none -serial stdio \
        -drive "file=$disk,if=none,id=root,format=qcow2,discard=unmap" \
        -device virtio-blk-pci,drive=root,bootindex=0 \
        -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 \
        -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
        "$@" </dev/null 2>&1 \
        > >(python3 -u -c '
import sys, time
for line in sys.stdin.buffer:
    # No \r inside a line (the console has some): readers split on it.
    text = line.decode("utf-8", "replace").rstrip("\r\n").replace("\r", "")
    sys.stdout.write("%.3f %s\n" % (time.time(), text))
' >>"$SERIAL") &
    QEMU_PID=$!
}
# serial_since <t0> <text...>: prints the seconds from <t0> to the first
# serial console line at or after <t0> that contains one of the texts;
# fails if there is none yet.
serial_since() {
    python3 - "$SERIAL" "$@" <<'EOF'
import sys
path, t0, texts = sys.argv[1], float(sys.argv[2]), sys.argv[3:]
for line in open(path, errors="replace", newline="\n"):
    ts, _, text = line.partition(" ")
    try:
        t = float(ts)
    except ValueError:
        continue
    if t >= t0 and any(x in text for x in texts):
        print("%.1f" % (t - t0))
        sys.exit(0)
sys.exit(1)
EOF
}
qemu_running() { [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; }
# wait_ssh <seconds>
wait_ssh() {
    local end=$((SECONDS + $1))
    until vm true 2>/dev/null; do
        qemu_running || die "the VM exited while booting"
        [ "$SECONDS" -lt "$end" ] || die "ssh did not come up within $1 s"
        sleep 1
    done
}
# wait_qemu_exit <seconds>
wait_qemu_exit() {
    local end=$((SECONDS + $1))
    while qemu_running; do
        [ "$SECONDS" -lt "$end" ] || die "the VM did not power off within $1 s"
        sleep 0.5
    done
    wait "$QEMU_PID" || true
    QEMU_PID=
}

# --- the prepared image ------------------------------------------------------

prepare_image() {
    local dir=$CACHE/prepare
    rm -rf "$dir"
    mkdir -p "$dir"
    log "downloading $IMG_NAME"
    curl -fsSL -o "$dir/SHA256SUMS" "$IMG_BASE/SHA256SUMS"
    curl -fsSL -o "$dir/$IMG_NAME" "$IMG_BASE/$IMG_NAME"
    (cd "$dir" && grep " \*$IMG_NAME\$" SHA256SUMS | sha256sum -c -) || die "checksum mismatch"
    [ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N '' -C puzuz-vm-test -f "$KEY"
    qemu-img convert -O qcow2 "$dir/$IMG_NAME" "$dir/disk.qcow2"
    rm -f "$dir/$IMG_NAME"
    qemu-img resize -q "$dir/disk.qcow2" 8G

    mkdir "$dir/seed"
    printf 'instance-id: puzuz-vm-1\nlocal-hostname: puzuz-vm\n' >"$dir/seed/meta-data"
    sed "s|@SSH_KEY@|$(cat "$KEY.pub")|" /stage/vm/user-data.yaml >"$dir/seed/user-data"
    xorriso -as genisoimage -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock \
        "$dir/seed/user-data" "$dir/seed/meta-data"

    log "provisioning the VM image (cloud-init: criu, users; it powers off when done)"
    : >"$SERIAL"
    start_vm "$dir/disk.qcow2" -drive "file=$dir/seed.iso,if=virtio,media=cdrom,readonly=on"
    wait_qemu_exit 1200
    grep -q 'PUZUZ-PROVISION-OK' "$SERIAL" || die "provisioning failed"
    grep -E 'PUZUZ-PROVISION' "$SERIAL" | sed 's/^[0-9.]* //'
    # Compact the image (zeroed and discarded blocks are dropped).
    qemu-img convert -O qcow2 "$dir/disk.qcow2" "$PREPARED.tmp"
    mv "$PREPARED.tmp" "$PREPARED"
    rm -rf "$dir"
    rm -f "$CACHE"/prepared-v[0-9]*.qcow2.tmp
    log "prepared image: $(du -h "$PREPARED" | cut -f1)"
}

if [ ! -f "$PREPARED" ] || [ ! -f "$KEY" ]; then
    # Old versions of the prepared image are no longer used.
    find "$CACHE" -maxdepth 1 -name 'prepared-v*.qcow2' ! -name "$(basename "$PREPARED")" -delete
    prepare_image
fi
[ "${1:-}" != prepare-only ] || exit 0

# --- the test run --------------------------------------------------------------

scenarios=("$@")
[ "${#scenarios[@]}" -gt 0 ] || scenarios=(reboot poweroff inflight)

qemu-img create -q -f qcow2 -b "$PREPARED" -F qcow2 "$WORK/root.qcow2"
# /var/lib/puzuz lives on a disk of its own, mounted from /etc/fstab, so
# that the journal shows when it is unmounted relative to the helper.
qemu-img create -q -f qcow2 "$WORK/state.qcow2" 6G
# shellcheck disable=SC2054  # commas belong to the QEMU option values
STATE_DRIVE=(-drive "file=$WORK/state.qcow2,if=none,id=state,format=qcow2,discard=unmap"
    -device virtio-blk-pci,drive=state,serial=puzuz-state)

: >"$SERIAL"
log "booting the VM"
start_vm "$WORK/root.qcow2" "${STATE_DRIVE[@]}"
wait_ssh 300
vm 'uname -r; systemctl --version | head -n1; criu --version | head -n1'
# The layout of scripts/e2e: /e2e/{scenario.sh,install.sh,q.py,...,bin,vm}.
tar -cf - -C /stage bin vm -C /stage/e2e . | vm 'rm -rf /e2e && mkdir /e2e && tar -C /e2e -xf - && chmod -R a+rX /e2e'
vm bash /e2e/vm/guest.sh setup || die "setup failed"

RESULTS=()
TIMINGS=()
for s in "${scenarios[@]}"; do
    case "$s" in
    reboot | inflight) action=reboot ;;
    poweroff) action=poweroff ;;
    *) die "unknown scenario $s" ;;
    esac
    log "=== scenario $s"
    if ! vm bash /e2e/vm/guest.sh prepare "$s"; then
        RESULTS+=("FAIL $s (prepare)")
        continue
    fi
    t0=$(date +%s.%N)
    # The trigger starts the shutdown itself (for "inflight" right after
    # `puzuz checkpoint`); the ssh connection is cut by the shutdown.
    vm bash /e2e/vm/guest.sh trigger "$s" "$action" || true
    if [ "$action" = poweroff ]; then
        wait_qemu_exit 1800
        log "the VM powered off; booting it again"
        start_vm "$WORK/root.qcow2" "${STATE_DRIVE[@]}"
    else
        # The VM resets in place (the same QEMU process).
        end=$((SECONDS + 1800))
        until serial_since "$t0" 'reboot: Restarting system' >/dev/null; do
            qemu_running || die "the VM exited during the reboot"
            [ "$SECONDS" -lt "$end" ] || die "the VM did not reboot"
            sleep 0.5
        done
    fi
    wait_ssh 600
    t_up=$(date +%s.%N)
    # Times from the serial console: the kernel's last message before the
    # reset / power-off.
    down=$(serial_since "$t0" 'reboot: Restarting system' 'reboot: Power down') || down='?'
    timing="$s: shutdown ${down}s (trigger to kernel ${action}), back on ssh after $(python3 -c "print('%.1f' % ($t_up - $t0))")s"
    log "$timing"
    TIMINGS+=("$timing")
    if vm bash /e2e/vm/guest.sh verify "$s"; then
        RESULTS+=("PASS $s")
    else
        RESULTS+=("FAIL $s")
    fi
done

log "shutting the VM down"
vm systemctl poweroff || true
wait_qemu_exit 300 || true

echo
echo "=== timings"
printf '%s\n' "${TIMINGS[@]}"
echo "=== summary"
printf '%s\n' "${RESULTS[@]}"
! printf '%s\n' "${RESULTS[@]}" | grep -q '^FAIL'
