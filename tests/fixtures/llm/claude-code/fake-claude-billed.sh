#!/bin/sh
cat >/dev/null
sed 's/"apiKeySource": "none"/"apiKeySource": "user"/' "$(dirname "$0")/booster.jsonl"
