# shellcheck shell=bash
# shellcheck disable=SC2034 # the variables are used by the scripts that source this file
# The distributions of scripts/vm-reboot-test.sh: their cloud images and what
# provisioning installs. Sourced by scripts/vm-reboot-test.sh (the package
# format) and scripts/vm/run.sh (the image); do not run it.
#
# Each distribution is an x86_64 cloud image with cloud-init. CRIU 4.2 or
# newer comes from the distribution where it has it (Ubuntu 26.04, Fedora);
# on the others the scenarios that checkpoint print SKIP (see guest.sh).

VM_DISTROS="ubuntu-26.04 ubuntu-24.04 debian-13 debian-12 fedora-44"
# The version of the prepared images (<distro>-prepared-v<N>.qcow2 in the
# cache): bump it when the provisioning (user-data.yaml, the packages below)
# changes. CI keys its cache of the images on this file and user-data.yaml.
VM_IMAGE_VERSION=2

# distro_info <name>: sets
#   DISTRO_PKG       deb or rpm (the package that is installed)
#   DISTRO_IMG_URL   the cloud image
#   DISTRO_SUMS_URL  its checksum file, DISTRO_SUMS_KIND sha256|sha512|fedora
#   DISTRO_USER_DATA cloud-config lines appended to user-data.yaml (package
#                    sources and packages)
# or fails for an unknown name.
distro_info() {
    local ubuntu_apt debian_apt
    # Plain HTTP (port 80) may be blocked; the images have CA certificates.
    # apt retries transient mirror failures.
    ubuntu_apt='apt:
  preserve_sources_list: false
  conf: |
    Acquire::Retries "5";
    Acquire::http::Timeout "30";
    Acquire::https::Timeout "30";
  primary:
    - arches: [default]
      uri: https://archive.ubuntu.com/ubuntu
  security:
    - arches: [default]
      uri: https://security.ubuntu.com/ubuntu
package_update: true
package_upgrade: false'
    debian_apt='apt:
  preserve_sources_list: false
  conf: |
    Acquire::Retries "5";
    Acquire::http::Timeout "30";
    Acquire::https::Timeout "30";
  primary:
    - arches: [default]
      uri: https://deb.debian.org/debian
  security:
    - arches: [default]
      uri: https://deb.debian.org/debian-security
package_update: true
package_upgrade: false'
    case "$1" in
    ubuntu-26.04 | ubuntu-24.04)
        local code=resolute
        [ "$1" = ubuntu-24.04 ] && code=noble
        DISTRO_PKG=deb
        DISTRO_IMG_URL="https://cloud-images.ubuntu.com/minimal/releases/$code/release/$1-minimal-cloudimg-amd64.img"
        DISTRO_SUMS_URL="${DISTRO_IMG_URL%/*}/SHA256SUMS"
        DISTRO_SUMS_KIND=sha256
        # Ubuntu 24.04 has no criu package (see docs/install.md, CRIU).
        local criu='  - criu'
        [ "$1" = ubuntu-24.04 ] && criu=
        DISTRO_USER_DATA="$ubuntu_apt
packages:
$criu
  - python3
  - e2fsprogs
  - sshfs"
        ;;
    debian-13 | debian-12)
        local code=trixie
        [ "$1" = debian-12 ] && code=bookworm
        DISTRO_PKG=deb
        DISTRO_IMG_URL="https://cloud.debian.org/images/cloud/$code/latest/$1-genericcloud-amd64.qcow2"
        DISTRO_SUMS_URL="https://cloud.debian.org/images/cloud/$code/latest/SHA512SUMS"
        DISTRO_SUMS_KIND=sha512
        # criu is too old here (4.1, 3.17): installed anyway, so that the
        # test sees what users get. systemd-resolved is a package of its own.
        DISTRO_USER_DATA="$debian_apt
packages:
  - criu
  - python3
  - e2fsprogs
  - sshfs
  - systemd-resolved"
        ;;
    fedora-*)
        local release=${1#fedora-}
        DISTRO_PKG=rpm
        # The image name has a build number (Fedora-Cloud-Base-Generic-44-1.7);
        # run.sh finds it in the checksum file of the directory.
        DISTRO_IMG_URL="https://dl.fedoraproject.org/pub/fedora/linux/releases/$release/Cloud/x86_64/images/"
        DISTRO_SUMS_URL=
        DISTRO_SUMS_KIND=fedora
        # No weak dependencies (fuse-sshfs would pull in a desktop's worth),
        # and not the openh264 repository, which is plain HTTP only.
        DISTRO_USER_DATA="bootcmd:
  - [sh, -c, 'echo install_weak_deps=False >>/etc/dnf/dnf.conf; rm -f /etc/yum.repos.d/fedora-cisco-openh264.repo']
packages:
  - criu
  - python3
  - e2fsprogs
  - fuse-sshfs"
        ;;
    *)
        echo "unknown distribution: $1 (known: $VM_DISTROS)" >&2
        return 1
        ;;
    esac
}
