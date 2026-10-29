#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
tool="$root/tools/gen-test-data"
out="${1:-$root/data/tests}"
shift || true
mkdir -p "$out"
out="$(cd "$out" && pwd)"
docker build -q -t zontologize-gen-test-data "$tool" >/dev/null
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$tool:/gen:ro" -v "$(dirname "$out"):/work" \
    zontologize-gen-test-data --out "/work/$(basename "$out")" "$@"
