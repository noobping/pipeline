#!/usr/bin/env bash
set -euo pipefail

image=${1:?usage: tests/container.sh IMAGE}
root=$(git rev-parse --show-toplevel)

if command -v docker >/dev/null 2>&1; then
    engine=(docker)
elif command -v podman >/dev/null 2>&1; then
    engine=(podman)
elif command -v flatpak-spawn >/dev/null 2>&1 \
    && flatpak-spawn --host podman version >/dev/null 2>&1; then
    engine=(flatpak-spawn --host podman)
else
    echo "docker or podman is required" >&2
    exit 1
fi

just_image=$(sed -n 's/^just-image = "\([^"]*\)"$/\1/p' "$root/Cargo.toml")
dockerfile_image=$(sed -n 's/^FROM \([^ ]*\) AS just$/\1/p' "$root/Dockerfile")
if [[ -z "$just_image" || "$dockerfile_image" != "$just_image" ]]; then
    echo "Dockerfile Just image does not match Cargo.toml metadata" >&2
    exit 1
fi

fixture='tests/action/fixture with spaces'
yaml_fixture=tests/action/yaml
container_name="pipeline-cancel-$$"

cleanup() {
    "${engine[@]}" rm --force "$container_name" >/dev/null 2>&1 || true
    rm -f \
        "$root/$fixture/action-output" \
        "$root/$fixture/action-child.started" \
        "$root/$fixture/action-child.finished" \
        "$root/$yaml_fixture/action-one.started" \
        "$root/$yaml_fixture/action-two.started"
}
trap cleanup EXIT
cleanup

run_action() {
    local directory=$1
    local arguments=$2
    "${engine[@]}" run --rm \
        --security-opt label=disable \
        --env GITHUB_ACTIONS=true \
        --env GITHUB_WORKSPACE=/github/workspace \
        --volume "$root:/github/workspace" \
        --workdir /github/workspace \
        --entrypoint /usr/local/bin/pipeline-action \
        "$image" "$directory" "$arguments"
}

"${engine[@]}" run --rm "$image" --version
minimum_just=$(sed -n 's/^just-min-version = "\([^"]*\)"$/\1/p' "$root/Cargo.toml")
actual_just=$("${engine[@]}" run --rm --entrypoint /usr/local/bin/just "$image" --version)
actual_just=${actual_just#just }
oldest=$(printf '%s\n%s\n' "$minimum_just" "$actual_just" | sort -V | head -n 1)
if [[ -z "$minimum_just" || "$oldest" != "$minimum_just" ]]; then
    echo "container Just $actual_just is older than required $minimum_just" >&2
    exit 1
fi

run_action "$fixture" ""
test "$(cat "$root/$fixture/action-output")" = action-ok

run_action "$fixture" $'argument\nhello world'
run_action "$yaml_fixture" $'--cap-jobs\n2\ncheck'
test -e "$root/$yaml_fixture/action-one.started"
test -e "$root/$yaml_fixture/action-two.started"

set +e
run_action "$fixture" failure
status=$?
set -e
if [[ $status -ne 23 ]]; then
    echo "expected failing recipe status 23, got $status" >&2
    exit 1
fi

"${engine[@]}" run --rm \
    --name "$container_name" \
    --security-opt label=disable \
    --env GITHUB_ACTIONS=true \
    --env GITHUB_WORKSPACE=/github/workspace \
    --volume "$root:/github/workspace" \
    --workdir /github/workspace \
    --entrypoint /usr/local/bin/pipeline-action \
    "$image" "$fixture" cancel &
run_pid=$!

for _ in $(seq 1 100); do
    [[ -e "$root/$fixture/action-child.started" ]] && break
    sleep 0.05
done
test -e "$root/$fixture/action-child.started"
"${engine[@]}" stop --time 2 "$container_name" >/dev/null
set +e
wait "$run_pid"
set -e
test ! -e "$root/$fixture/action-child.finished"
