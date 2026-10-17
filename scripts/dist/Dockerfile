# systemd as PID 1 plus what install.sh needs (curl), for scripts/dist-test.sh.
# Like scripts/helper-systemd/Dockerfile, but for any Debian/Ubuntu BASE
# (ubuntu:26.04, 24.04, 22.04, debian:12): criu is optional (older releases
# lack 4.2 or any package), since these tests never checkpoint.
# Where port 80 is blocked but the image has CA certificates (e.g.
# gpueue-poc/systemd259:0.1), apt is switched to https.
ARG BASE=ubuntu:26.04
FROM ${BASE}
RUN set -e; \
    if [ -f /etc/ssl/certs/ca-certificates.crt ]; then \
        sed -i 's|http://|https://|g' /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true; \
    fi; \
    printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\n' >/etc/apt/apt.conf.d/80-gpueue-retries; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus python3 procps curl ca-certificates sudo; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends criu \
        || echo "no criu package for this base"; \
    rm -rf /var/lib/apt/lists/*; \
    if getent passwd 1000 >/dev/null; then \
        old=$(getent passwd 1000 | cut -d: -f1); \
        if [ "$old" != alice ]; then \
            usermod -l alice -d /home/alice -m "$old"; \
            groupmod -n alice "$(getent group 1000 | cut -d: -f1)"; \
        fi; \
    else \
        useradd -m -u 1000 -s /bin/sh alice; \
    fi
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
