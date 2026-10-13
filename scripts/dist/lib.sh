# shellcheck shell=bash
# shellcheck disable=SC2034 # the variables are used by the scripts that source this file
# Helpers shared by the distribution scripts (scripts/dist/*.sh,
# scripts/dist-test.sh); source it, do not run it. scripts/install.sh keeps
# its own copy of the architecture mapping, since it is downloaded alone.

# dist_arch: sets DIST_DEB_ARCH (amd64, arm64) and DIST_TARGET (the Rust
# target of the release tarball) for this machine, or fails.
dist_arch() {
    case "$(uname -m)" in
    x86_64)
        DIST_DEB_ARCH=amd64
        DIST_TARGET=x86_64-unknown-linux-gnu
        ;;
    aarch64 | arm64)
        DIST_DEB_ARCH=arm64
        DIST_TARGET=aarch64-unknown-linux-gnu
        ;;
    *)
        echo "unsupported architecture: $(uname -m) (releases exist for x86_64 and aarch64)" >&2
        return 1
        ;;
    esac
}

# workspace_version ROOT: prints the workspace version from ROOT/Cargo.toml.
workspace_version() {
    local version
    version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$1/Cargo.toml")"
    [ -n "$version" ] || { echo "cannot read the workspace version from $1/Cargo.toml" >&2; return 1; }
    printf '%s\n' "$version"
}
