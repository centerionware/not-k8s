#!/usr/bin/env bash
# Exact tagged assets, never latest/VERSION. SHA256SUMS is published alongside
# the binaries; verify before executing anything downloaded.
set -euo pipefail
tag=${1:?release tag}
profile=${2:-release}
dest=${3:?destination}
: "${GH_TOKEN:?}" "${GITHUB_REPOSITORY:?}"
[[ "$tag" =~ ^v[0-9]+(\.[0-9]+){2,3}$ ]] || exit 2
[[ "$profile" == release || "$profile" == profiling ]] || exit 2
mkdir -p "$dest"
work=$(mktemp -d)
asset="notk8s-${tag#v}-linux-x86_64-$profile"
gh release download "$tag" --repo "$GITHUB_REPOSITORY" --pattern "$asset" --pattern SHA256SUMS --dir "$work"
awk -v asset="$asset" '$2 == asset {print}' "$work/SHA256SUMS" > "$work/selected.sha256"
[[ $(wc -l < "$work/selected.sha256") == 1 ]] || { echo "missing/duplicate checksum for $asset" >&2; exit 1; }
(cd "$work" && sha256sum -c selected.sha256)
install -m0755 "$work/$asset" "$dest/notk8s"
cp "$work/selected.sha256" "$dest/SHA256SUMS"
