#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
hf upload zal-analytics-core/zontologize-data data . --repo-type dataset \
    --include "README.md" --include "tests/*" --commit-message "${1:-Update test data}"
