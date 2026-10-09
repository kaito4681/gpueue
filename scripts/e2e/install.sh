#!/bin/bash
# Installs puzuz inside the systemd container the way an administrator
# would (`puzuz service install`), with the test's /etc/puzuz/config.toml
# (which the installer keeps), and creates the admin user. Idempotent: it
# runs again in the second container of the reboot test, on top of the
# persisted /var/lib/puzuz.
#   alice (1000), bob (1001): regular users; carol (1002): puzuz-admin.
set -euo pipefail

mkdir -p /etc/puzuz
if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
    install -m 0644 /e2e/config-gpu.toml /etc/puzuz/config.toml
else
    install -m 0644 /e2e/config.toml /etc/puzuz/config.toml
fi
# Exits non-zero when its final checks warn (e.g. no GPUs); the state of
# the services is checked below.
/e2e/bin/puzuz service install --bin-dir /usr/local/bin || true

getent passwd 1002 >/dev/null || useradd -m -u 1002 -s /bin/sh carol
usermod -aG puzuz-admin carol

for _ in $(seq 100); do
    [ -S /run/puzuz/puzuz.sock ] && break
    sleep 0.1
done
systemctl is-active puzuz-helper.service puzuz.service
