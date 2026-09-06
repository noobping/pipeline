#!/usr/bin/env bash
set -euo pipefail

working_directory=${1:-.}
raw_arguments=${2:-}

workspace=${GITHUB_WORKSPACE:-/github/workspace}
if [[ ! -d "$workspace" && -d /github/workspace ]]; then
    workspace=/github/workspace
fi
if [[ ! -d "$workspace" ]]; then
    workspace=$PWD
fi

workspace=$(cd -- "$workspace" && pwd -P)
case "$working_directory" in
    /*) destination=$working_directory ;;
    *) destination=$workspace/$working_directory ;;
esac
cd -- "$destination"
destination=$(pwd -P)

# GitHub's checkout belongs to the runner user while Docker actions run as root.
# Trust only the mounted workspace and the explicitly selected working directory.
git config --global --add safe.directory "$workspace"
if [[ "$destination" != "$workspace" ]]; then
    git config --global --add safe.directory "$destination"
fi

pipeline_arguments=()
while IFS= read -r argument || [[ -n "$argument" ]]; do
    argument=${argument%$'\r'}
    [[ -z "$argument" ]] && continue
    pipeline_arguments+=("$argument")
done < <(printf '%s' "$raw_arguments")

exec /usr/bin/tini -g -- /usr/local/bin/pipeline "${pipeline_arguments[@]}"
