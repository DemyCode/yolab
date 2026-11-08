#!/usr/bin/env bash
set -euo pipefail

: "${CHART:?}" "${REGISTRY:?}" "${LIB_VERSION:?}" "${STACK_VERSION:?}"
SUFFIX="${SUFFIX:-}"

retry() {
  for attempt in 1 2 3 4 5; do
    if "$@"; then return 0; fi
    sleep $((attempt * 15))
  done
  return 1
}

mkdir -p /tmp/pkg
name=$(grep '^name:' "$CHART/Chart.yaml" | head -1 | awk '{print $2}')
base=$(grep '^version:' "$CHART/Chart.yaml" | head -1 | awk '{print $2}' | tr -d '"')

sed -i "/- name: yolab-common/{n;s|version: .*|version: \"${LIB_VERSION}\"|}" "$CHART/Chart.yaml"
sed -i "/- name: yolab-stack/{n;s|version: .*|version: \"${STACK_VERSION}\"|}" "$CHART/Chart.yaml"
members=$(awk '/^dependencies:/{d=1;next} d && /^[^ ]/{d=0} d && /- name:/ && $3 != "yolab-common" && $3 != "yolab-stack" {print $3}' "$CHART/Chart.yaml")
for member in $members; do
  version=$(grep '^version:' "apps/catalog/${member}/Chart.yaml" | head -1 | awk '{print $2}' | tr -d '"')
  sed -i "/- name: ${member}\$/{n;s|version: .*|version: \"${version}${SUFFIX}\"|}" "$CHART/Chart.yaml"
  retry helm show chart "${REGISTRY}/${member}" --version "${version}${SUFFIX}" >/dev/null 2>&1 \
    || { echo "::error::${name} is built from ${member} ${version}${SUFFIX}, which the registry does not serve"; exit 1; }
done

retry helm dependency update "$CHART" >/dev/null \
  || { echo "::error::${name}: its dependencies could not be fetched"; exit 1; }

if grep -q -- "- name: yolab-stack" "$CHART/Chart.yaml"; then
  helm lint "$CHART" \
    --set config.subdomain=example \
    --set config.media_folder=media \
    --set config.music_folder=music
else
  grep -rq "yolab-common.uninstallHook" "$CHART" \
    || { echo "::error::${name} has no uninstall hook"; exit 1; }
  helm lint "$CHART" -f apps/catalog/lint-values.yaml
fi

helm package "$CHART" --version "${base}${SUFFIX}" --destination /tmp/pkg
retry helm push "/tmp/pkg/${name}-${base}${SUFFIX}.tgz" "${REGISTRY}" \
  || { echo "::error::${name} ${base}${SUFFIX} could not be pushed"; exit 1; }
