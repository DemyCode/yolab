#!/bin/sh
set -eu
meminfo=${MEMINFO:-/proc/meminfo}
kib=$(awk '/^MemTotal:/ { print $2 }' "$meminfo")
if [ -z "$kib" ]; then
  echo "no MemTotal in $meminfo" >&2
  exit 1
fi
floor=1073741824
ceiling=4294967296
target=$((kib * 1024 / 8))
if [ "$target" -lt "$floor" ]; then
  target=$floor
fi
if [ "$target" -gt "$ceiling" ]; then
  target=$ceiling
fi
echo "$target"
