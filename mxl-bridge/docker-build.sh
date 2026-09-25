#!/usr/bin/env bash
# Builds the mxl-bridge image from a small staging context (this crate + MXL's headers and Rust
# crates, never any target/ dir), then optionally loads it into the kind cluster.
#
# Usage: ./docker-build.sh [--kind-load <cluster>]      e.g. --kind-load headroom-test
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MXL="$(cd "$HERE/../../mxl" && pwd)"
TAG="${TAG:-mxl-bridge:latest}"
CTX="$(mktemp -d)"
trap 'rm -rf "$CTX"' EXIT

rsync -a --exclude target --exclude .git "$HERE/" "$CTX/mxl-bridge/"
mkdir -p "$CTX/mxl/lib"
rsync -a "$MXL/lib/include" "$CTX/mxl/lib/"
rsync -a --exclude target "$MXL/rust" "$CTX/mxl/"

docker build -f "$HERE/Dockerfile" -t "$TAG" "$CTX"

if [[ "${1:-}" == "--kind-load" ]]; then
    kind load docker-image "$TAG" --name "${2:?cluster name}"
fi
