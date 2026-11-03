#!/usr/bin/env bash
set -euo pipefail

[ "$#" -gt 0 ] || {
    echo "usage: check-cache.sh <store path>..." >&2
    exit 1
}

root=$(cd "$(dirname "$0")/.." && pwd)
setting() {
    nix eval --raw --file "$root/flake.nix" "nixConfig.$1" --apply 'xs: builtins.concatStringsSep " " xs'
}
cache=$(setting extra-substituters)
keys=$(setting extra-trusted-public-keys)
echo "cache: $cache"
echo "trusted key: $keys"

if ! info=$(curl -fsS --retry 5 --retry-connrefused --retry-delay 5 "$cache/nix-cache-info"); then
    echo "::error::$cache/nix-cache-info refuses an anonymous read, so every install and update builds from source"
    exit 1
fi
printf '%s\n' "$info"

if ! nix store verify --store "$cache" --no-contents \
    --option netrc-file /dev/null \
    --option trusted-public-keys "$keys" \
    "$@"; then
    echo "::error::$cache does not serve these paths signed by $keys, the key every machine trusts (flake.nix, installer/nixos/iso-config.nix, homelab/nixos/common.nix)"
    exit 1
fi
echo "all $# paths are served anonymously and signed by $keys"
