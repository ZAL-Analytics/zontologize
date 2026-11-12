#!/bin/sh
input=$(cat)
dir=$(dirname "$0")
case "$input" in
*"Zuidwal Technick"*) pair=yes ;;
*) pair=no ;;
esac
case "$pair $input" in
"yes Weigh the evidence"*"Zuidwal Techniek"*) cat "$dir/curate-proposed.jsonl" ;;
"yes "*"Zuidwal Techniek"*) cat "$dir/curate-merge.jsonl" ;;
*"- m1 name matches"*) cat "$dir/curate-not-entity.jsonl" ;;
*"are disjoint"*) cat "$dir/curate-disjoint.jsonl" ;;
*"- m1 atSite needs Project"*) cat "$dir/curate-class.jsonl" ;;
*"Why these members were picked"*) cat "$dir/curate-empty.jsonl" ;;
*) echo "fake-claude: no recorded answer for this prompt" >&2; exit 1 ;;
esac
