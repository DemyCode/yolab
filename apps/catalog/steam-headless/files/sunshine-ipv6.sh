#!/bin/sh
set -eu

conf="${SUNSHINE_CONF:-/home/steamheadless/.config/sunshine/sunshine.conf}"
mkdir -p "${conf%/*}"
if [ ! -s "$conf" ]; then
    printf 'sunshine_name = %s\nupnp = disabled\n' "${SUNSHINE_NAME:-$HOSTNAME}" >"$conf"
fi
sed -i '/^[[:space:]]*address_family[[:space:]]*=/d' "$conf"
echo 'address_family = both' >>"$conf"
