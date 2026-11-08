#!/bin/sh
set -eu

tree="$1"
n="$2"
shift 2
doomed=""
while [ "$n" -gt 0 ]; do
    d="$tree/$1"
    doomed="$doomed${d%/}
"
    shift
    n=$((n - 1))
done
kept=""
for k in "$@"; do
    kept="$kept$tree/$k
"
done

verdict() {
    v=delete
    while IFS= read -r k; do
        [ -n "$k" ] || continue
        if [ "$1" = "$k" ]; then
            v=keep
            break
        fi
        case "$k" in "$1"/*) v=descend ;; esac
    done <<KEPT
$kept
KEPT
    echo "$v"
}

prune() {
    for e in "$1"/* "$1"/.[!.]* "$1"/..?*; do
        [ -e "$e" ] || [ -L "$e" ] || continue
        case "$(verdict "$e")" in
        delete) rm -rf -- "$e" ;;
        descend)
            if [ -L "$e" ]; then
                rm -f -- "$e"
            else
                prune "$e"
            fi
            ;;
        esac
    done
}

while IFS= read -r d; do
    [ -n "$d" ] || continue
    case "$(verdict "$d")" in
    delete)
        if [ "$d" = "$tree" ]; then
            prune "$d"
        else
            rm -rf -- "$d"
        fi
        ;;
    descend) prune "$d" ;;
    esac
done <<DOOMED
$doomed
DOOMED
