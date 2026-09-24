#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
FILTER="$ROOT/.github/scripts/nodemigrate-snapshot-normalize.jq"
command -v jq >/dev/null 2>&1 || {
    echo "jq is required to check migration snapshot normalization" >&2
    exit 2
}

before="$(jq -cn '{apiVersion:"v1",kind:"ConfigMap",metadata:{name:"settings",namespace:"apps",uid:"source-uid",resourceVersion:"4",generation:1,creationTimestamp:"2026-01-01T00:00:00Z",annotations:{"nodemigrate.io/source-uid":"user-value"},ownerReferences:[{apiVersion:"v1",kind:"ConfigMap",name:"parent",uid:"parent-source-uid"}]},data:{marker:"preserved"},status:{ignored:true}}')"
after="$(jq -cn '{apiVersion:"v1",kind:"ConfigMap",metadata:{name:"settings",namespace:"apps",uid:"target-uid",resourceVersion:"19",generation:3,creationTimestamp:"2026-01-02T00:00:00Z",annotations:{"nodemigrate.io/source-uid":"user-value"},ownerReferences:[{apiVersion:"v1",kind:"ConfigMap",name:"parent",uid:"parent-target-uid"}]},data:{marker:"preserved"},status:{ignored:false}}')"
before_normalized="$(jq -cS -f "$FILTER" <<< "$before")"
after_normalized="$(jq -cS -f "$FILTER" <<< "$after")"
[[ "$before_normalized" == "$after_normalized" ]] || {
    echo "source and destination identity changes did not normalize equally" >&2
    exit 1
}
jq -e '.metadata.annotations["nodemigrate.io/source-uid"] == "user-value" and .data.marker == "preserved" and (.metadata.ownerReferences[0] | has("uid") | not)' \
    <<< "$after_normalized" >/dev/null

node_state="$(jq -cn '{items:[{metadata:{labels:{"operator.example/pool":"blue"},annotations:{"nodemigrate.io/source-uid":"operator-node-value"}},spec:{taints:[{key:"operator.example/dedicated",value:"migration",effect:"PreferNoSchedule"}]}}]}')"
jq -e '
  all(.items[];
    .metadata.labels["operator.example/pool"] == "blue" and
    .metadata.annotations["nodemigrate.io/source-uid"] == "operator-node-value" and
    any(.spec.taints[]?; .key == "operator.example/dedicated" and .value == "migration" and .effect == "PreferNoSchedule")
  )
' <<< "$node_state" >/dev/null

pv_before="$(jq -cn '{apiVersion:"v1",kind:"PersistentVolume",metadata:{name:"data"},spec:{claimRef:{namespace:"apps",name:"claim",uid:"claim-source-uid"}}}')"
pv_after="$(jq -cn '{apiVersion:"v1",kind:"PersistentVolume",metadata:{name:"data"},spec:{claimRef:{namespace:"apps",name:"claim",uid:"claim-target-uid"}}}')"
[[ "$(jq -cS -f "$FILTER" <<< "$pv_before")" == "$(jq -cS -f "$FILTER" <<< "$pv_after")" ]] || {
    echo "claim reference identity changes did not normalize equally" >&2
    exit 1
}

for transient in \
    '{"apiVersion":"v1","kind":"Node","metadata":{"name":"node-a"}}' \
    '{"apiVersion":"metrics.k8s.io/v1beta1","kind":"NodeMetrics","metadata":{"name":"node-a"}}' \
    '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"pod-a","namespace":"apps"}}' \
    '{"apiVersion":"metrics.k8s.io/v1beta1","kind":"PodMetrics","metadata":{"name":"pod-a","namespace":"apps"}}' \
    '{"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/service-account-token","metadata":{"name":"token","namespace":"apps"}}'; do
    [[ -z "$(jq -cS -f "$FILTER" <<< "$transient")" ]] || {
        echo "transient object was included in the migratable snapshot" >&2
        exit 1
    }
done

echo "migration snapshot normalization checks passed"
