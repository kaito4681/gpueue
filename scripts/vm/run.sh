#!/bin/bash
# Runs inside the runner container of scripts/vm-reboot-test.sh: prepares
# (once per distribution, cached in /cache) and boots one VM, installs gpueue
# in it and drives the scenarios of guest.sh on it.
#   /stage/pkg  the packages (new.deb|new.rpm, old.deb|old.rpm for "upgrade")
#   /stage/e2e  scripts/e2e   /stage/vm  this directory
#
# Usage: run.sh prepare-only DISTRO
#        run.sh DISTRO INSTALL SCENARIO...
#   INSTALL: package (the distribution's package of the tree), service
#            (`gpueue service install` of the package's binaries), upgrade
#            (the previous release's package; the scenario upgrades it)
# Prints one "RESULT PASS|FAIL|SKIP DISTRO/SCENARIO[: reason]" line per
# scenario and "TIMING ..." lines; exits non-zero if a scenario failed.
set -euo pipefail

# shellcheck source=scripts/vm/distros.sh
. /stage/vm/distros.sh

CACHE=/cache
WORK=/work
SERIAL=$WORK/serial.log
MEM_MB=${GPUEUE_VM_MEM_MB:-4096}
QEMU_PID=

[ "$#" -ge 2 ] || { echo "usage: run.sh prepare-only DISTRO | DISTRO INSTALL SCENARIO..." >&2; exit 2; }
if [ "$1" = prepare-only ]; then
    DISTRO=$2
    shift 2
else
    DISTRO=$1
    INSTALL=$2
    shift 2
fi
distro_info "$DISTRO"
PREPARED=$CACHE/$DISTRO-prepared-v$VM_IMAGE_VERSION.qcow2
CACHE_KEY=$CACHE/id_ed25519
KEY=$WORK/id_ed25519

mkdir -p "$WORK"
log() { echo "[$(date +%T)] $*"; }
die() {
    log "ERROR: $*"
    if [ -f "$SERIAL" ]; then
        echo "--- tail of the serial console (all of it: $CACHE/last-serial-$DISTRO.log)"
        tail -n 60 "$SERIAL"
        cp "$SERIAL" "$CACHE/last-serial-$DISTRO.log" || true
    fi
    if [ "${GPUEUE_VM_KEEP:-}" = 1 ]; then
        log "keeping the VM: ssh -F $WORK/ssh_config vm"
        sleep infinity
    fi
    exit 1
}
# A bind-mounted cache directory (CI) is handed back to the invoking user.
own_cache() {
    [ -z "${GPUEUE_VM_CACHE_OWNER:-}" ] || chown -R "$GPUEUE_VM_CACHE_OWNER" "$CACHE" || true
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
# stop_qemu <seconds>: like wait_qemu_exit, but not fatal: kills QEMU when
# it is still running after <seconds>.
stop_qemu() {
    local end=$((SECONDS + $1))
    while qemu_running && [ "$SECONDS" -lt "$end" ]; do sleep 0.5; done
    if qemu_running; then
        log "the VM did not power off within $1 s; killing QEMU"
        kill "$QEMU_PID" 2>/dev/null || true
    fi
    [ -z "$QEMU_PID" ] || wait "$QEMU_PID" || true
    QEMU_PID=
}

# --- the prepared image ------------------------------------------------------

# download_image <dir>: the distribution's cloud image as <dir>/image,
# checked against its published checksum.
download_image() {
    local dir=$1 name sums line
    case "$DISTRO_SUMS_KIND" in
    sha256 | sha512)
        name=${DISTRO_IMG_URL##*/}
        curl -fsSL --retry 5 --retry-all-errors -o "$dir/SUMS" "$DISTRO_SUMS_URL"
        # "<hash> *<name>" (Ubuntu) or "<hash>  <name>" (Debian).
        line=$(grep -E "^[0-9a-f]+ [ *]$name\$" "$dir/SUMS") || die "$name is not in $DISTRO_SUMS_URL"
        log "downloading $DISTRO_IMG_URL"
        curl -fsSL --retry 5 --retry-all-errors -o "$dir/image" "$DISTRO_IMG_URL"
        (cd "$dir" && echo "${line%% *}  image" | "${DISTRO_SUMS_KIND}sum" -c -) || die "checksum mismatch"
        ;;
    fedora)
        # The directory listing names the CHECKSUM file of the build, whose
        # lines are "SHA256 (<image>) = <hash>".
        sums=$(curl -fsSL --retry 5 --retry-all-errors "$DISTRO_IMG_URL" | grep -oE 'Fedora-Cloud-[0-9][^"]*-x86_64-CHECKSUM' | head -n1) \
            || die "no CHECKSUM file in $DISTRO_IMG_URL"
        curl -fsSL --retry 5 --retry-all-errors -o "$dir/SUMS" "$DISTRO_IMG_URL$sums"
        line=$(grep -E '^SHA256 \(Fedora-Cloud-Base-Generic-[^)]*\.x86_64\.qcow2\) = [0-9a-f]{64}$' "$dir/SUMS") \
            || die "no Generic qcow2 image in $sums"
        name=$(sed -E 's/^SHA256 \(([^)]*)\).*/\1/' <<<"$line")
        log "downloading $DISTRO_IMG_URL$name"
        curl -fsSL --retry 5 --retry-all-errors -o "$dir/image" "$DISTRO_IMG_URL$name"
        (cd "$dir" && echo "${line##* }  image" | sha256sum -c -) || die "checksum mismatch"
        ;;
    esac
}

prepare_image() {
    local dir=$CACHE/prepare-$DISTRO
    rm -rf "$dir"
    mkdir -p "$dir"
    [ -f "$CACHE_KEY" ] || ssh-keygen -q -t ed25519 -N '' -C gpueue-vm-test -f "$CACHE_KEY"
    install -m 0600 "$CACHE_KEY" "$KEY"
    download_image "$dir"
    qemu-img convert -O qcow2 "$dir/image" "$dir/disk.qcow2"
    rm -f "$dir/image"
    qemu-img resize -q "$dir/disk.qcow2" 8G

    mkdir "$dir/seed"
    printf 'instance-id: gpueue-vm-1\nlocal-hostname: gpueue-vm\n' >"$dir/seed/meta-data"
    { sed "s|@SSH_KEY@|$(cat "$CACHE_KEY.pub")|" /stage/vm/user-data.yaml; echo "$DISTRO_USER_DATA"; } \
        >"$dir/seed/user-data"
    xorriso -as genisoimage -quiet -output "$dir/seed.iso" -volid cidata -joliet -rock \
        "$dir/seed/user-data" "$dir/seed/meta-data"

    log "provisioning the $DISTRO image (cloud-init: packages, users; it powers off when done)"
    : >"$SERIAL"
    start_vm "$dir/disk.qcow2" -drive "file=$dir/seed.iso,if=virtio,media=cdrom,readonly=on"
    wait_qemu_exit 1800
    grep -q 'GPUEUE-PROVISION-OK' "$SERIAL" || die "provisioning failed"
    grep -E 'GPUEUE-PROVISION' "$SERIAL" | sed 's/^[0-9.]* //'
    # Compact the image (zeroed and discarded blocks are dropped).
    qemu-img convert -O qcow2 "$dir/disk.qcow2" "$PREPARED.tmp"
    mv "$PREPARED.tmp" "$PREPARED"
    rm -rf "$dir"
    log "prepared image: $(du -h "$PREPARED" | cut -f1)"
}

if [ ! -f "$PREPARED" ] || [ ! -f "$CACHE_KEY" ]; then
    # Old versions of the prepared image are no longer used (prepared-v1 was
    # Ubuntu 26.04's before there were several distributions).
    find "$CACHE" -maxdepth 1 \( -name "$DISTRO-prepared-v*.qcow2*" -o -name 'prepared-v*.qcow2*' \) \
        ! -name "$(basename "$PREPARED")" -delete
    prepare_image
    own_cache
fi
install -m 0600 "$CACHE_KEY" "$KEY"
[ -z "${INSTALL:-}" ] && exit 0

# --- the test run --------------------------------------------------------------

scenarios=("$@")
[ "${#scenarios[@]}" -gt 0 ] || die "no scenarios"

qemu-img create -q -f qcow2 -b "$PREPARED" -F qcow2 "$WORK/root.qcow2"
# /var/lib/gpueue lives on a disk of its own, mounted from /etc/fstab, so
# that the journal shows when it is unmounted relative to the helper.
qemu-img create -q -f qcow2 "$WORK/state.qcow2" 6G
# shellcheck disable=SC2054  # commas belong to the QEMU option values
STATE_DRIVE=(-drive "file=$WORK/state.qcow2,if=none,id=state,format=qcow2,discard=unmap"
    -device virtio-blk-pci,drive=state,serial=gpueue-state)

RESULTS=()
TIMINGS=()
result() { # result PASS|FAIL|SKIP <scenario> [reason]
    RESULTS+=("$1 $DISTRO/$2${3:+: $3}")
}
finish() {
    echo
    echo "=== timings ($DISTRO, $INSTALL)"
    [ "${#TIMINGS[@]}" -eq 0 ] || printf 'TIMING %s\n' "${TIMINGS[@]}"
    echo "=== summary ($DISTRO, $INSTALL)"
    printf 'RESULT %s\n' "${RESULTS[@]}"
    # Not `printf | grep -q`: grep -q exits on the first match, printf may then
    # die of SIGPIPE, and pipefail plus `!` would turn a FAIL into success.
    local summary
    summary="$(printf '%s\n' "${RESULTS[@]}")"
    ! grep -q '^FAIL' <<<"$summary"
}

: >"$SERIAL"
log "booting the $DISTRO VM"
t_boot=$SECONDS
start_vm "$WORK/root.qcow2" "${STATE_DRIVE[@]}"
wait_ssh 300
vm '. /etc/os-release; echo "$PRETTY_NAME"; uname -r; systemctl --version | head -n1; criu --version 2>/dev/null | head -n1 || echo "no criu"'
# The layout of scripts/e2e: /e2e/{scenario.sh,install.sh,q.py,...,vm,pkg}.
tar -cf - -C /stage vm pkg -C /stage/e2e . | vm 'rm -rf /e2e && mkdir /e2e && tar -C /e2e -xf - && chmod -R a+rX /e2e'
if ! vm bash /e2e/vm/guest.sh setup "$INSTALL"; then
    for s in "${scenarios[@]}"; do result FAIL "$s" "setup ($INSTALL) failed"; done
    finish
    exit 1
fi
TIMINGS+=("$DISTRO: boot and setup ($INSTALL) $((SECONDS - t_boot))s")
criu_reason=
criu_reason=$(vm bash /e2e/vm/guest.sh criu-check) || true

for s in "${scenarios[@]}"; do
    log "=== scenario $DISTRO/$s"
    t_s=$SECONDS
    action=
    case "$s" in
    reboot | inflight | desktop | service-install) action=reboot ;;
    poweroff) action=poweroff ;;
    basic | upgrade) ;;
    *) die "unknown scenario $s" ;;
    esac
    # Every scenario but "basic" checkpoints.
    if [ "$s" != basic ] && [ -n "$criu_reason" ]; then
        echo "SKIP $DISTRO/$s: $criu_reason"
        result SKIP "$s" "$criu_reason"
        continue
    fi
    if [ -z "$action" ]; then
        if vm bash /e2e/vm/guest.sh run "$s"; then result PASS "$s"; else result FAIL "$s"; fi
        TIMINGS+=("$DISTRO/$s: $((SECONDS - t_s))s")
        continue
    fi
    if ! vm bash /e2e/vm/guest.sh prepare "$s"; then
        result FAIL "$s" "prepare"
        continue
    fi
    t0=$(date +%s.%N)
    # The trigger starts the shutdown itself (for "inflight" right after
    # `gpueue checkpoint`); the ssh connection is cut by the shutdown.
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
    timing="$DISTRO/$s: shutdown ${down}s (trigger to kernel ${action}), back on ssh after $(python3 -c "print('%.1f' % ($t_up - $t0))")s"
    log "$timing"
    if vm bash /e2e/vm/guest.sh verify "$s"; then
        result PASS "$s"
    else
        result FAIL "$s"
    fi
    TIMINGS+=("$timing; scenario $((SECONDS - t_s))s")
done

if [ "${GPUEUE_VM_KEEP:-}" = 1 ] && printf '%s\n' "${RESULTS[@]}" | grep -q '^FAIL'; then
    log "a scenario failed; keeping the VM: ssh -F $WORK/ssh_config vm"
    sleep infinity
fi
log "shutting the VM down"
vm systemctl poweroff || true
stop_qemu 300
finish
