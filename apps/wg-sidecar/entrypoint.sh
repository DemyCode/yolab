#!/bin/sh
set -e

WG_CONF="${WG_CONF:-/etc/wireguard/wg0.conf}"

if [ ! -f "$WG_CONF" ]; then
  echo "ERROR: WireGuard config not found at $WG_CONF"
  exit 1
fi

wg-quick up "$WG_CONF"

cleanup() {
  wg-quick down "$WG_CONF"
}
trap cleanup INT TERM

tail -f /dev/null &
wait $!
