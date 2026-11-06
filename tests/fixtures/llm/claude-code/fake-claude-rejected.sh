#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1000000000}}'
