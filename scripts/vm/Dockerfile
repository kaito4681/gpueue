# Runner for scripts/vm-reboot-test.sh: QEMU/KVM and the tools to build a
# cloud-init seed and talk to the guest over ssh. Needs no privileges apart
# from --device /dev/kvm.
#
# apt goes over HTTPS (plain HTTP may be blocked). The base image has no CA
# certificates yet, so the build context carries the host's bundle as
# ca.crt; it is only used to install the ca-certificates package.
FROM ubuntu:26.04
COPY ca.crt /tmp/host-ca.crt
RUN set -e; \
    sed -i 's|http://archive.ubuntu.com|https://archive.ubuntu.com|; s|http://security.ubuntu.com|https://security.ubuntu.com|' \
        /etc/apt/sources.list.d/ubuntu.sources; \
    apt-get -o Acquire::https::CAInfo=/tmp/host-ca.crt update; \
    DEBIAN_FRONTEND=noninteractive apt-get -o Acquire::https::CAInfo=/tmp/host-ca.crt \
        install -y --no-install-recommends ca-certificates; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        qemu-system-x86 qemu-utils xorriso openssh-client curl python3; \
    rm -rf /var/lib/apt/lists/* /tmp/host-ca.crt
