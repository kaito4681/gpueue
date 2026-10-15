#!/bin/bash
# Approximates an administrator's mount configuration on a real host inside
# the systemd test container (see the mount scenarios in scenario.sh). Runs
# as root after every boot, before install.sh; the filesystems' contents
# live on the volume mounted on $DISK, so they survive the simulated reboot
# while every mount comes back as a new mount (new mount ids and, for the
# loop devices, other device numbers; see below).
#
#   /                rshared, as systemd makes it on a real host
#   /srv/ext4        a loop-mounted ext4 image (a separate filesystem)
#   /home/alice      bind mount of a directory of that filesystem
#   /data            ext4 image, x-systemd.automount (fstab, noauto)
#   /scratch         a tmpfs mounted by the administrator
#   /ovl             overlayfs (lower and upper on /srv/ext4)
#   /ref             read-only bind mount of /srv/ext4/ref
#   /priv            bind mount with private propagation
#   /nfs             sshfs (FUSE) from the SSH server on 127.0.0.1, when
#                    sshfs is installed (kernel NFS needs the nfs module on
#                    the host kernel, which a container cannot load safely)
#
# Mount points listed in $DISK/skip-next-boot (one per line) are not
# mounted at the next boot (a filesystem that does not come back after a
# reboot); the file is removed afterwards. Idempotent: running it again
# mounts what is missing.
#
# Every other boot first mounts a small loop image on /media/gpueue-e2e-shift
# (pruned from job namespaces), so that /srv/ext4 gets another loop device,
# and with it another device number, than at the previous boot.
#
# Usage: mounts.sh setup
set -euo pipefail

DISK=/var/lib/gpueue-e2e-disk

skip=()
if [ -f "$DISK/skip-next-boot" ]; then
    mapfile -t skip <"$DISK/skip-next-boot"
    rm -f "$DISK/skip-next-boot"
fi
skipped() { [[ " ${skip[*]} " == *" $1 "* ]]; }
mounted() { mountpoint -q "$1"; }

image() { # image <file> <size>: create an ext4 image once
    [ -f "$1" ] || { truncate -s "$2" "$1" && mkfs.ext4 -q -F "$1"; }
}

# A container's /dev is a snapshot: loop devices that the host kernel
# allocates later (for `mount -o loop`, also in systemd's automount) have no
# node. Create the nodes up front.
for i in $(seq 0 255); do
    [ -e "/dev/loop$i" ] || mknod -m 0660 "/dev/loop$i" b 7 "$i"
done
# `mount -o loop` sets autoclear: the loop device is released with the
# last mount (at the latest when the container goes away).
mount_loop() { # mount_loop <image> <dir>
    local i
    for i in 1 2 3 4 5; do
        mount -o loop "$1" "$2" && return 0
        sleep "0.$i"
    done
    return 1
}

mount --make-rshared /
mkdir -p "$DISK" /srv/ext4 /data /scratch /ovl /ref /priv /nfs

if [ ! -f "$DISK/booted" ]; then
    boots=0
else
    boots=$(cat "$DISK/booted")
fi
if ! mounted /srv/ext4 && [ $((boots % 2)) -eq 1 ]; then
    image "$DISK/shift.img" 8M
    mkdir -p /media/gpueue-e2e-shift
    mounted /media/gpueue-e2e-shift || mount_loop "$DISK/shift.img" /media/gpueue-e2e-shift
fi
echo $((boots + 1)) >"$DISK/booted"

image "$DISK/ext4.img" 128M
if ! skipped /srv/ext4; then
    mounted /srv/ext4 || mount_loop "$DISK/ext4.img" /srv/ext4
    mkdir -p /srv/ext4/home/alice /srv/ext4/ovl/lower /srv/ext4/ovl/upper \
        /srv/ext4/ovl/work /srv/ext4/ref /srv/ext4/priv /srv/ext4/export
    chown alice: /srv/ext4/home/alice /srv/ext4/priv /srv/ext4/export
    [ -f /srv/ext4/ref/input.txt ] || seq 1 100 >/srv/ext4/ref/input.txt
    [ -f /srv/ext4/ovl/lower/base.txt ] || echo base >/srv/ext4/ovl/lower/base.txt
    chmod 0777 /srv/ext4/ovl/upper
    # /home/alice: a bind mount (as with a separate /home filesystem).
    skipped /home/alice || mounted /home/alice \
        || mount --bind /srv/ext4/home/alice /home/alice
    skipped /ovl || mounted /ovl || mount -t overlay overlay \
        -o lowerdir=/srv/ext4/ovl/lower,upperdir=/srv/ext4/ovl/upper,workdir=/srv/ext4/ovl/work /ovl
    if ! skipped /ref && ! mounted /ref; then
        mount --bind /srv/ext4/ref /ref
        mount -o remount,bind,ro /ref
    fi
    if ! skipped /priv && ! mounted /priv; then
        mount --bind /srv/ext4/priv /priv
        mount --make-private /priv
    fi
fi

# /data: automounted on first access.
if [ ! -f "$DISK/data.img" ]; then
    image "$DISK/data.img" 64M
    tmp=$(mktemp -d)
    mount_loop "$DISK/data.img" "$tmp"
    mkdir -p "$tmp/alice" && chown alice: "$tmp/alice"
    umount "$tmp" && rmdir "$tmp"
fi
if ! skipped /data; then
    grep -q ' /data ' /etc/fstab 2>/dev/null \
        || echo "$DISK/data.img /data ext4 loop,noauto,x-systemd.automount 0 0" >>/etc/fstab
    systemctl daemon-reload
    systemctl start data.automount
fi

if ! skipped /scratch && ! mounted /scratch; then
    mount -t tmpfs -o mode=1777,size=64m tmpfs /scratch
fi

if ! skipped /nfs && ! mounted /nfs && command -v sshfs >/dev/null \
    && command -v sshd >/dev/null; then
    mkdir -p "$DISK/ssh" /root/.ssh /run/sshd
    [ -f "$DISK/ssh/id" ] || ssh-keygen -q -t ed25519 -N '' -f "$DISK/ssh/id"
    cp "$DISK/ssh/id.pub" /root/.ssh/authorized_keys
    chmod 0600 /root/.ssh/authorized_keys
    ssh-keygen -A >/dev/null
    # An sshd of our own (port 2022) without PAM, not the image's
    # ssh.service: where libpam-systemd is installed (polkitd, installed
    # for scenario 19, pulls it in) and logind runs, a PAM login is a
    # logind session, and systemd stops session scopes early in a
    # shutdown. The sftp-server behind /nfs would then be gone before the
    # helper dumps the jobs at the reboot of scenarios 7/15/16, and CRIU
    # fails on every job's (now disconnected) /nfs mount ("failed to dump
    # fs ./nfs (fuse)"). Scenario 15 checks that the sftp-server is in no
    # session.
    [ -e /run/sshd-e2e.pid ] && kill -0 "$(cat /run/sshd-e2e.pid)" 2>/dev/null \
        || /usr/sbin/sshd -o ListenAddress=127.0.0.1 -o Port=2022 -o UsePAM=no \
            -o PermitRootLogin=prohibit-password -o PidFile=/run/sshd-e2e.pid
    for _ in $(seq 50); do
        sshfs -p 2022 -o allow_other,default_permissions,IdentityFile="$DISK/ssh/id",StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR \
            root@127.0.0.1:/srv/ext4/export /nfs 2>/dev/null && break
        sleep 0.2
    done
    mounted /nfs
fi
findmnt -R -o TARGET,SOURCE,FSTYPE,PROPAGATION / | grep -E '/srv|/home|/data|/scratch|/ovl|/ref|/priv|/nfs|/media|^/ ' || true
