#!/bin/sh
input=$(cat)
dir=$(dirname "$0")
case "$input" in
*"booster pump"*) cat "$dir/booster.jsonl" ;;
*) cat "$dir/empty.jsonl" ;;
esac
