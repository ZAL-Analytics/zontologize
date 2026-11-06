#!/bin/sh
input=$(cat)
dir=$(dirname "$0")
case "$input" in
*"Allowed properties (IRI | label | kind"*"Rated power"*) cat "$dir/attributes-sheet.jsonl" ;;
*"Allowed properties (IRI | label | kind"*) cat "$dir/attributes-empty.jsonl" ;;
*"Document types"*"Work order"*) cat "$dir/classify-work-order.jsonl" ;;
*"Document types"*) cat "$dir/classify-none.jsonl" ;;
*"booster pump"*) cat "$dir/extract-booster.jsonl" ;;
*"Known things (ref | class | name)"*) cat "$dir/extract-empty.jsonl" ;;
*) echo "fake-claude: no recorded answer for this prompt" >&2; exit 1 ;;
esac
