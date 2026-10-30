#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
repo=zal-analytics-core/zontologize-data

usage() {
    echo "usage: $0 --gen-test [out] [args...] | --fetch-bench [out] [args...] | --download | --upload [message]" >&2
    exit 1
}

[ $# -ge 1 ] || usage
cmd="$1"
shift

case "$cmd" in
    --gen-test)
        tool="$root/tools/gen-test-data"
        out="${1:-$root/data/tests}"
        shift || true
        mkdir -p "$out"
        out="$(cd "$out" && pwd)"
        docker build -q -t zontologize-gen-test-data "$tool" >/dev/null
        docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
            -v "$tool:/gen:ro" -v "$(dirname "$out"):/work" \
            zontologize-gen-test-data --out "/work/$(basename "$out")" "$@"
        ;;
    --fetch-bench)
        python3 -I "$root/tools/fetch-bench-data/fetch.py" --out "${1:-$root/data/benchmarks}" "${@:2}"
        ;;
    --download)
        hf download "$repo" --repo-type dataset --local-dir "$root/data"
        ;;
    --upload)
        hf upload "$repo" "$root/data" . --repo-type dataset --exclude "benchmarks/.cache/*" --commit-message "${1:-Update data}"
        ;;
    *)
        usage
        ;;
esac
