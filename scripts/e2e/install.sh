#!/bin/bash
# Installs gpueue inside the systemd container the way an administrator
# would (`gpueue service install`), with the test's /etc/gpueue/config.toml
# (which the installer keeps), and creates the admin user. Idempotent: it
# runs again in the second container of the reboot test, on top of the
# persisted /var/lib/gpueue.
#   alice (1000), bob (1001): regular users; carol (1002): gpueue-admin.
set -euo pipefail

mkdir -p /etc/gpueue
if [ "${GPUEUE_E2E_GPU:-}" = 1 ]; then
    install -m 0644 /e2e/config-gpu.toml /etc/gpueue/config.toml
else
    install -m 0644 /e2e/config.toml /etc/gpueue/config.toml
fi
if [ -n "${GPUEUE_E2E_CUDA_MODE:-}" ]; then
    # Top-level key: prepend it, before any [[gpus]] table.
    { echo "cuda_checkpoint_mode = \"$GPUEUE_E2E_CUDA_MODE\""; cat /etc/gpueue/config.toml; } >/etc/gpueue/config.toml.new
    mv /etc/gpueue/config.toml.new /etc/gpueue/config.toml
    chmod 0644 /etc/gpueue/config.toml
fi
# Exits non-zero when its final checks warn (e.g. no GPUs); the state of
# the services is checked below.
# /e2e is a mount owned by the invoking user; `service install` only copies a
# helper that nobody but root could have replaced, so install from a root-owned copy.
install -d -m 0755 /opt/gpueue-src
install -m 0755 /e2e/bin/gpueue /e2e/bin/gpueue-helper /opt/gpueue-src/
/opt/gpueue-src/gpueue service install --bin-dir /usr/local/bin || true
rm -rf /opt/gpueue-src

getent passwd 1002 >/dev/null || useradd -m -u 1002 -s /bin/sh carol
usermod -aG gpueue-admin carol

for _ in $(seq 100); do
    [ -S /run/gpueue/gpueue.sock ] && break
    sleep 0.1
done
systemctl is-active gpueue-helper.service gpueue.service
# The simulated reboot stops the container through the shutdown targets, whose
# JobTimeoutSec= (30 min by default) the installed drop-ins raise.
for target in reboot poweroff halt kexec; do
    timeout="$(systemctl show -p JobTimeoutUSec --value "$target.target")"
    [ "$timeout" = 2h ] || { echo "FAIL: $target.target has JobTimeoutUSec=$timeout" >&2; exit 1; }
done
