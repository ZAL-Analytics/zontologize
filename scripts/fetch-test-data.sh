#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
hf download zal-analytics-core/zontologize-data --repo-type dataset --local-dir data \
    --include "README.md" --include "tests/*"
