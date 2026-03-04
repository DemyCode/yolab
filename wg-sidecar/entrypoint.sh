#!/bin/sh
set -e

WG_CONF="${WG_CONF:-/etc/wireguard/wg0.conf}"

if [ ! -f "$WG_CONF" ]; then
  echo "ERROR: WireGuard config not found at $WG_CONF"
  echo "Mount your wg0.conf at $WG_CONF"
  exit 1
fi

wg-quick up "$WG_CONF"

# Keep container alive — WireGuard runs in kernel, not as a process
exec tail -f /dev/null
