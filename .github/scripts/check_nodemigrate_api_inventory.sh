#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(git rev-parse --show-toplevel)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT

cat > "$temporary_directory/expected.txt" <<'RESOURCES'
deployments.apps
widgets.example.io
RESOURCES
cat > "$temporary_directory/actual.txt" <<'RESOURCES'
deployments.apps
nodes
widgets.example.io
RESOURCES

NODEMIGRATE_INTEGRATION_LIBRARY=true bash -c '
  source "$1/.github/scripts/nodemigrate-integration.sh"
  assert_discovered_api_resources_preserved "$2/expected.txt" "$2/actual.txt" target
' _ "$ROOT" "$temporary_directory"

cat > "$temporary_directory/missing.txt" <<'RESOURCES'
deployments.apps
RESOURCES
if output="$(NODEMIGRATE_INTEGRATION_LIBRARY=true bash -c '
  source "$1/.github/scripts/nodemigrate-integration.sh"
  assert_discovered_api_resources_preserved "$2/expected.txt" "$2/missing.txt" target
' _ "$ROOT" "$temporary_directory" 2>&1)"; then
    echo "API inventory comparison accepted a missing source resource" >&2
    exit 1
fi
grep -Fqx 'widgets.example.io' <<< "$output" || {
    echo "API inventory comparison did not identify the missing source resource" >&2
    printf '%s\n' "$output" >&2
    exit 1
}

cat > "$temporary_directory/source-objects.jsonl" <<'OBJECTS'
{"identity":{"apiGroup":"apps","kind":"Deployment","name":"sample","namespace":"test"},"sha256":"same","fields":{"/spec/replicas":"same"}}
OBJECTS
cp "$temporary_directory/source-objects.jsonl" "$temporary_directory/target-objects.jsonl"
cat >> "$temporary_directory/target-objects.jsonl" <<'OBJECTS'
{"identity":{"apiGroup":"apps","kind":"Deployment","name":"target-runtime","namespace":"kube-system"},"sha256":"target-only"}
OBJECTS
NODEMIGRATE_INTEGRATION_LIBRARY=true bash -c '
  source "$1/.github/scripts/nodemigrate-integration.sh"
  assert_migratable_api_objects_unchanged "$2/source-objects.jsonl" "$2/target-objects.jsonl" source target
' _ "$ROOT" "$temporary_directory"
output="$(NODEMIGRATE_INTEGRATION_LIBRARY=true bash -c '
  source "$1/.github/scripts/nodemigrate-integration.sh"
  assert_migratable_api_objects_unchanged "$2/source-objects.jsonl" "$2/target-objects.jsonl" source target
' _ "$ROOT" "$temporary_directory")"
grep -Fq 'target-only objects: 1' <<< "$output" || {
    echo "API object comparison did not report target-only runtime objects" >&2
    printf '%s\n' "$output" >&2
    exit 1
}

cat > "$temporary_directory/target-objects.jsonl" <<'OBJECTS'
{"identity":{"apiGroup":"apps","kind":"Deployment","name":"sample","namespace":"test"},"sha256":"changed","fields":{"/spec/replicas":"changed"}}
{"identity":{"apiGroup":"apps","kind":"Deployment","name":"target-runtime","namespace":"kube-system"},"sha256":"target-only"}
OBJECTS
if output="$(NODEMIGRATE_INTEGRATION_LIBRARY=true bash -c '
  source "$1/.github/scripts/nodemigrate-integration.sh"
  assert_migratable_api_objects_unchanged "$2/source-objects.jsonl" "$2/target-objects.jsonl" source target
' _ "$ROOT" "$temporary_directory" 2>&1)"; then
    echo "API object comparison accepted a changed normalized resource" >&2
    exit 1
fi
grep -Fq '"Deployment"' <<< "$output" || {
    echo "API object comparison did not identify the changed resource" >&2
    printf '%s\n' "$output" >&2
    exit 1
}
grep -Fq '/spec/replicas' <<< "$output" || {
    echo "API object comparison did not report the changed normalized field path" >&2
    printf '%s\n' "$output" >&2
    exit 1
}
grep -Fq 'target-runtime' <<< "$output" || {
    echo "API object comparison hid target-only identities when source data changed" >&2
    printf '%s\n' "$output" >&2
    exit 1
}

echo "nodemigrate API inventory checks passed"
