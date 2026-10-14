# systemd as PID 1, for scripts/helper-systemd-test.sh.
# Ubuntu 26.04 ships systemd 259 and the distro criu 4.2 (no CUDA plugin)
# that the Docker+systemd PoC used in manual mode. Its glibc is newer than
# the toolchain image's (Debian bookworm), so the binary built there runs
# here. BASE may be any image derived from it that already has the
# packages (e.g. the PoC's puzuz-poc/systemd259:0.1 where apt is offline).
ARG BASE=ubuntu:26.04
FROM ${BASE}
RUN set -e; \
    if ! command -v criu >/dev/null || ! command -v systemctl >/dev/null \
       || ! command -v python3 >/dev/null; then \
        apt-get update; \
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            systemd systemd-sysv dbus python3 procps criu; \
        rm -rf /var/lib/apt/lists/*; \
    fi; \
    if getent passwd 1000 >/dev/null; then \
        old=$(getent passwd 1000 | cut -d: -f1); \
        if [ "$old" != alice ]; then \
            usermod -l alice -d /home/alice -m "$old"; \
            groupmod -n alice "$(getent group 1000 | cut -d: -f1)"; \
        fi; \
    else \
        useradd -m -u 1000 -s /bin/sh alice; \
    fi; \
    getent passwd 1001 >/dev/null || useradd -m -u 1001 -s /bin/sh bob
# Optional: sshfs and an SSH server for the FUSE mount scenario of
# scripts/e2e/scenario.sh (it prints a SKIP line when they are missing, e.g.
# where apt is offline, or fails with PUZUZ_E2E_REQUIRE_SSHFS=1).
RUN if ! command -v sshfs >/dev/null || ! command -v sshd >/dev/null; then \
        (apt-get update \
         && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            sshfs openssh-server openssh-client \
         && rm -rf /var/lib/apt/lists/*) \
        || echo "sshfs/openssh-server not installed; the FUSE scenario will be skipped"; \
    fi
# Optional: sudo and polkit (pkexec), so that scenario 19 of
# scripts/e2e/scenario.sh can show that jobs cannot use them even when the
# owner may (it skips those checks when they are missing).
RUN if ! command -v sudo >/dev/null || ! command -v pkexec >/dev/null; then \
        (apt-get update \
         && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            sudo polkitd pkexec \
         && rm -rf /var/lib/apt/lists/*) \
        || echo "sudo/polkit not installed; scenario 19 skips their checks"; \
    fi
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
