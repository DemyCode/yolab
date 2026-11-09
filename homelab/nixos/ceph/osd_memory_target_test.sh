#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
failed=0

target_for() {
  printf 'MemTotal:       %s kB\nMemFree:          1000 kB\n' "$1" > "$tmp/meminfo"
  MEMINFO="$tmp/meminfo" sh "$here/osd-memory-target.sh"
}

expect() {
  name=$1
  want=$2
  got=$3
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: want $want, got $got"
    failed=1
  fi
}

expect "a 4 GB raspberry pi gets the 1 GiB floor" 1073741824 "$(target_for 3884000)"
expect "an 8 GB laptop also stays on the floor" 1073741824 "$(target_for 7850000)"
expect "a 16 GiB machine gives its OSD an eighth" 2147483648 "$(target_for 16777216)"
expect "a 64 GiB machine keeps the 4 GiB ceph default" 4294967296 "$(target_for 67108864)"

printf 'MemFree: 1000 kB\n' > "$tmp/meminfo"
if MEMINFO="$tmp/meminfo" sh "$here/osd-memory-target.sh" > /dev/null 2>&1; then
  echo "FAIL a meminfo without MemTotal must stop the OSD from starting"
  failed=1
else
  echo "ok   a meminfo without MemTotal stops the OSD from starting"
fi

exit "$failed"
