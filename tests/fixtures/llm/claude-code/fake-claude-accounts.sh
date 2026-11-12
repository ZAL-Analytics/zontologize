#!/bin/sh
dir=$(dirname "$0")
case "$CLAUDE_CONFIG_DIR" in
*-a) cat >/dev/null; cat "$dir/rejected.jsonl" ;;
*) exec "$dir/fake-claude.sh" ;;
esac
