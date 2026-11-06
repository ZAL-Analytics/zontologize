#!/bin/sh
input=$(cat)
dir=$(dirname "$0")
case "$input" in
*"booster pump"*) cat "$dir/booster.jsonl" ;;
*"Shortlisted classes (IRI | label | aliases | definition)"*) cat "$dir/empty.jsonl" ;;
*) echo "fake-claude: no recorded answer for this prompt" >&2; exit 1 ;;
esac
