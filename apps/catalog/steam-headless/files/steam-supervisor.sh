#!/usr/bin/env bash
set -euo pipefail

original="${STEAM_SUPERVISOR:-/usr/local/bin/steam-headless-steam}"
check=$'    if pgrep -x steam >/dev/null 2>&1; then\n        missing_steam_checks=0'
alive=$'    if pgrep -x steam >/dev/null 2>&1 || steam_launcher_running; then\n        missing_steam_checks=0'

script="$(<"${original}")"
after="${script#*"${check}"}"
if [[ "${after}" == "${script}" || "${after}" == *"${check}"* ]]; then
    echo "${original} no longer has exactly one Steam liveness loop;" \
        "update steam-headless/files/steam-supervisor.sh for this image" >&2
    exit 1
fi

steam_launcher_running() {
    [[ -n "${steam_launcher_pid}" ]] || return 1
    local state
    state="$(awk '{print $3}' "/proc/${steam_launcher_pid}/stat" 2>/dev/null)" || return 1
    [[ -n "${state}" && "${state}" != Z ]]
}
export -f steam_launcher_running

patched="$(mktemp)"
printf '%s\n' "${script/"${check}"/"${alive}"}" >"${patched}"
exec bash "${patched}"
