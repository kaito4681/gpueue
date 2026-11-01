#!/usr/bin/env bash
# Fills the Nix binary cache that CI's nix jobs keep in the GitHub Actions
# cache (.github/workflows/ci.yml, job nix): the closure of the given
# installables, without the paths cache.nixos.org has (those carry its
# signature), so that only what the jobs build themselves is kept (CRIU and
# the vendored crates, about 20 MB). The directory is emptied first, so that
# paths of an older key do not pile up.
#
# Usage: scripts/nix-ci-cache.sh <cache dir> <installable...>
set -euo pipefail

dir="$(realpath -m "$1")"
shift
mapfile -t outputs < <(nix build --no-link --print-out-paths "$@")
# `nix path-info --sigs` prints each path followed by its signatures
# (`ultimate` for a path built here).
mapfile -t own < <(
    nix path-info --recursive --sigs "${outputs[@]}" |
        awk '{ for (i = 2; i <= NF; i++) if ($i ~ /^cache\.nixos\.org-1:/) next; print $1 }'
)
rm -rf "$dir"
# A binary cache only takes a path whose references it has, so the whole
# closure is copied and the paths that cache.nixos.org serves are removed
# again; a job substitutes them from there.
nix copy --to "file://$dir?compression=zstd" "${outputs[@]}"
keep=" ${own[*]##*/} "
for narinfo in "$dir"/*.narinfo; do
    name="$(basename "$narinfo" .narinfo)"
    case "$keep" in
        *" $name-"*) continue ;;
    esac
    nar="$(sed -n 's/^URL: //p' "$narinfo")"
    rm -f "$narinfo" "$dir/$nar"
done
echo "kept ${#own[@]} paths, $(du -sh "$dir" | cut -f1)"
