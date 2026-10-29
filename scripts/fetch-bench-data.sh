#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
python3 -I "$root/tools/fetch-bench-data/fetch.py" --out "${1:-$root/data/benchmarks}" "${@:2}"
