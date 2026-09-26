#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(git rev-parse --show-toplevel)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT

cat > "$temporary_directory/helm" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
case "$*" in
    'list --all-namespaces --output json')
        cat <<'JSON'
[{"name":"traefik","namespace":"traefik","chart":"traefik-37.1.1","app_version":"3.3.5","revision":2,"status":"deployed"},{"name":"cert-manager","namespace":"cert-manager","chart":"cert-manager-v1.21.2","app_version":"1.21.2","revision":1,"status":"deployed"}]
JSON
        ;;
    'get values traefik -n traefik --all -o json')
        printf '%s\n' '{"service":{"type":"ClusterIP"}}'
        ;;
    'get values cert-manager -n cert-manager --all -o json')
        printf '%s\n' '{"crds":{"enabled":true}}'
        ;;
    'get manifest traefik -n traefik')
        printf '%s\n' 'kind: Deployment' 'metadata:' '  name: traefik'
        ;;
    'get manifest cert-manager -n cert-manager')
        printf '%s\n' 'kind: Deployment' 'metadata:' '  name: cert-manager'
        ;;
    *)
        echo "unexpected helm arguments: $*" >&2
        exit 2
        ;;
esac
SH
chmod +x "$temporary_directory/helm"

PATH="$temporary_directory:$PATH" \
NODEMIGRATE_INTEGRATION_LIBRARY=true \
    bash -c 'source "$1/.github/scripts/nodemigrate-integration.sh"; capture_helm_release_state "$2"' \
    _ "$ROOT" "$temporary_directory/releases.jsonl"

jq -s -e '
  length == 2 and
  ([.[].name] | sort) == ["cert-manager", "traefik"] and
  all(.[];
    .namespace != "" and .chart != "" and .appVersion != "" and
    .revision != "" and .status == "deployed" and
    (.valuesSha256 | test("^[0-9a-f]{64}$")) and
    (.manifestSha256 | test("^[0-9a-f]{64}$"))
  )
' "$temporary_directory/releases.jsonl" >/dev/null

echo "nodemigrate Helm snapshot checks passed"
