#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
FILTER="$ROOT/.github/scripts/nodemigrate-snapshot-normalize.jq"
APPLICATION_FILTER="$ROOT/.github/scripts/nodemigrate-application-snapshot.jq"
command -v jq >/dev/null 2>&1 || {
    echo "jq is required to check migration snapshot normalization" >&2
    exit 2
}

source_application="$(jq -cn '{items:[
  {apiVersion:"v1",kind:"ConfigMap",metadata:{name:"kube-root-ca.crt",namespace:"migration-apps",annotations:{"kubernetes.io/description":"source CA"}},data:{"ca.crt":"source-ca"}},
  {apiVersion:"apps/v1",kind:"StatefulSet",metadata:{name:"database",namespace:"migration-apps"},spec:{volumeClaimTemplates:[{apiVersion:"v1",kind:"PersistentVolumeClaim",metadata:{name:"data"},spec:{accessModes:["ReadWriteOnce"],resources:{requests:{storage:"1Gi"}}}}]}}
]}')"
target_application="$(jq -cn '{items:[
  {apiVersion:"v1",kind:"ConfigMap",metadata:{name:"kube-root-ca.crt",namespace:"migration-apps",annotations:{"kubernetes.io/description":"target CA"}},data:{"ca.crt":"target-ca"}},
  {apiVersion:"apps/v1",kind:"StatefulSet",metadata:{name:"database",namespace:"migration-apps"},spec:{volumeClaimTemplates:[{metadata:{name:"data"},spec:{accessModes:["ReadWriteOnce"],resources:{requests:{storage:"1Gi"}}}}]}}
]}')"
[[ "$(jq -cS -f "$APPLICATION_FILTER" <<< "$source_application")" == "$(jq -cS -f "$APPLICATION_FILTER" <<< "$target_application")" ]] || {
    echo "generated CA metadata or optional PVC template type metadata changed the semantic fixture snapshot" >&2
    exit 1
}
jq -e 'all(.[]; .kind != "ConfigMap" or .name != "kube-root-ca.crt") and .[0].spec.volumeClaimTemplates[0].spec.resources.requests.storage == "1Gi"' \
    <<< "$(jq -cS -f "$APPLICATION_FILTER" <<< "$target_application")" >/dev/null

legacy_token_source='{"items":[{"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/service-account-token","metadata":{"name":"legacy-token","namespace":"apps","annotations":{"kubernetes.io/service-account.name":"builder","kubernetes.io/service-account.uid":"source-uid","custom.example/preserve":"yes"}}}]}'
legacy_token_target="${legacy_token_source/source-uid/target-uid}"
[[ "$(jq -cS -f "$APPLICATION_FILTER" <<< "$legacy_token_source")" == "$(jq -cS -f "$APPLICATION_FILTER" <<< "$legacy_token_target")" ]] || {
    echo "destination ServiceAccount UID reissuance changed legacy token Secret identity" >&2
    exit 1
}
jq -e '.[0].annotations["kubernetes.io/service-account.name"] == "builder" and .[0].annotations["custom.example/preserve"] == "yes"' \
    <<< "$(jq -cS -f "$APPLICATION_FILTER" <<< "$legacy_token_target")" >/dev/null || {
    echo "legacy token snapshot normalization hid user annotations" >&2
    exit 1
}
legacy_token_api_source='{"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/service-account-token","metadata":{"name":"legacy-token","namespace":"apps","annotations":{"kubernetes.io/service-account.name":"builder","kubernetes.io/service-account.uid":"source-uid","custom.example/preserve":"yes"}},"data":{"token":"c291cmNlLXRva2Vu","namespace":"YXBwcw==","ca.crt":"c291cmNlLWNh","fixture":"cHJlc2VydmVk"}}'
legacy_token_api_target='{"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/service-account-token","metadata":{"name":"legacy-token","namespace":"apps","annotations":{"kubernetes.io/service-account.name":"builder","kubernetes.io/service-account.uid":"target-uid","custom.example/preserve":"yes"}},"data":{"token":"dGFyZ2V0LXRva2Vu","namespace":"YXBwcw==","ca.crt":"dGFyZ2V0LWNh","fixture":"cHJlc2VydmVk"}}'
[[ "$(jq -cS -f "$FILTER" <<< "$legacy_token_api_source")" == "$(jq -cS -f "$FILTER" <<< "$legacy_token_api_target")" ]] || {
    echo "destination-bound token fields changed legacy Secret semantics" >&2
    exit 1
}
legacy_token_api_changed="$(jq -c '.data.fixture = "Y2hhbmdlZA=="' <<< "$legacy_token_api_target")"
[[ "$(jq -cS -f "$FILTER" <<< "$legacy_token_api_source")" != "$(jq -cS -f "$FILTER" <<< "$legacy_token_api_changed")" ]] || {
    echo "legacy ServiceAccount Secret's unrelated data was normalized away" >&2
    exit 1
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

priority_source='{"apiVersion":"scheduling.k8s.io/v1","kind":"PriorityClass","metadata":{"name":"system-node-critical"},"value":2000001000}'
priority_target='{"apiVersion":"scheduling.k8s.io/v1","kind":"PriorityClass","metadata":{"name":"system-node-critical"},"value":2000001000,"globalDefault":false}'
[[ "$(jq -cS -f "$FILTER" <<< "$priority_source")" == "$(jq -cS -f "$FILTER" <<< "$priority_target")" ]] || {
    echo "absent and false PriorityClass globalDefault values should have identical scheduling semantics" >&2
    exit 1
}
priority_true="$(jq -cS -f "$FILTER" <<< "${priority_target/false/true}")"
[[ "$priority_true" != "$(jq -cS -f "$FILTER" <<< "$priority_source")" ]] || {
    echo "PriorityClass globalDefault=true was normalized away" >&2
    exit 1
}

coredns_rbac_source='{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"system:coredns","labels":{"kubernetes.io/bootstrapping":"rbac-defaults","custom.example/preserve":"yes"},"annotations":{"rbac.authorization.kubernetes.io/autoupdate":"true","custom.example/preserve":"yes"}},"rules":[{"apiGroups":[""],"resources":["services"],"verbs":["list","watch"]}]}'
coredns_rbac_target='{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"system:coredns","labels":{"custom.example/preserve":"yes"},"annotations":{"custom.example/preserve":"yes"}},"rules":[{"apiGroups":[""],"resources":["services"],"verbs":["list","watch"]}]}'
[[ "$(jq -cS -f "$FILTER" <<< "$coredns_rbac_source")" == "$(jq -cS -f "$FILTER" <<< "$coredns_rbac_target")" ]] || {
    echo "Kubernetes CoreDNS bootstrap metadata did not normalize across source and target defaults" >&2
    exit 1
}
coredns_rbac_changed="${coredns_rbac_target/\"watch\"/\"get\"}"
[[ "$(jq -cS -f "$FILTER" <<< "$coredns_rbac_source")" != "$(jq -cS -f "$FILTER" <<< "$coredns_rbac_changed")" ]] || {
    echo "CoreDNS ClusterRole rules were normalized away with bootstrap metadata" >&2
    exit 1
}

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
    '{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"web","namespace":"apps","labels":{"endpoints.kubernetes.io/managed-by":"endpoint-controller"}}}' \
    '{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"kubernetes","namespace":"default"}}' \
    '{"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"web-abc","namespace":"apps","labels":{"endpointslice.kubernetes.io/managed-by":"endpointslice-controller.k8s.io"}}}' \
    '{"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"web-mirror-abc","namespace":"apps","labels":{"endpointslice.kubernetes.io/managed-by":"endpointslicemirroring-controller.k8s.io"}}}' \
    '{"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"kubernetes","namespace":"default","labels":{"kubernetes.io/service-name":"kubernetes"}}}' \
    '{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"node-a","namespace":"kube-node-lease"}}' \
    '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"kube-root-ca.crt","namespace":"apps"},"data":{"ca.crt":"source-ca"}}' \
    '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"pod-a","namespace":"apps","ownerReferences":[{"kind":"ReplicaSet","name":"web","uid":"source-uid","controller":true}]}}' \
    '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"mirror-pod","namespace":"kube-system","annotations":{"kubernetes.io/config.mirror":"mirror-uid"}}}' \
    '{"apiVersion":"metrics.k8s.io/v1beta1","kind":"PodMetrics","metadata":{"name":"pod-a","namespace":"apps"}}' \
    '{"apiVersion":"cilium.io/v2","kind":"CiliumEndpoint","metadata":{"name":"pod-a","namespace":"apps"}}' \
    '{"apiVersion":"cilium.io/v2","kind":"CiliumIdentity","metadata":{"name":"12345"}}' \
    '{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"coredns-54bf7cdff9","namespace":"kube-system","ownerReferences":[{"kind":"Deployment","name":"coredns","controller":true}]}}' \
    '{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"local-path-provisioner-69879d7dd7","namespace":"kube-system","ownerReferences":[{"kind":"Deployment","name":"local-path-provisioner","controller":true}]}}' \
    '{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"metrics-server-77dbbf84b","namespace":"kube-system","ownerReferences":[{"kind":"Deployment","name":"metrics-server","controller":true}]}}' \
    '{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"cilium-operator-resource-lock","namespace":"kube-system"},"spec":{"holderIdentity":"source","renewTime":"2026-09-27T00:00:00Z"}}' \
    '{"apiVersion":"storage.k8s.io/v1","kind":"CSINode","metadata":{"name":"node-a","ownerReferences":[{"apiVersion":"v1","kind":"Node","name":"node-a","uid":"source-node-uid"}]},"spec":{"drivers":[{"name":"hostpath.csi.k8s.io","nodeID":"node-a"}]}}'; do
    [[ -z "$(jq -cS -f "$FILTER" <<< "$transient")" ]] || {
        echo "transient object was included in the migratable snapshot" >&2
        exit 1
    }
done

for durable in \
    '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"standalone","namespace":"apps"}}' \
    '{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"external-db","namespace":"migration-apps"},"subsets":[{"addresses":[{"ip":"192.0.2.20"}],"ports":[{"port":5432}]}]}' \
    '{"apiVersion":"v1","kind":"Endpoints","metadata":{"name":"custom-web","namespace":"apps","labels":{"endpoints.kubernetes.io/managed-by":"custom-endpoint-controller"}}}' \
    '{"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"external-db-v4","namespace":"migration-apps","labels":{"kubernetes.io/service-name":"external-db","endpointslice.kubernetes.io/managed-by":"migration-operator"}},"addressType":"IPv4","endpoints":[{"addresses":["192.0.2.20"]}],"ports":[{"port":5432}]}' \
    '{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"migration-lock","namespace":"migration-apps"},"spec":{"holderIdentity":"migration-controller"}}' \
    '{"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"application-lock","namespace":"kube-system"},"spec":{"holderIdentity":"application-controller"}}' \
    '{"apiVersion":"cilium.io/v2","kind":"CiliumNode","metadata":{"name":"node-a"},"spec":{"addresses":[{"ip":"192.0.2.10","type":"InternalIP"}]}}' \
    '{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"metrics-server-old","namespace":"kube-system"}}' \
    '{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"web-old","namespace":"apps"}}' \
    '{"apiVersion":"apps/v1","kind":"ControllerRevision","metadata":{"name":"db-old","namespace":"apps"}}'; do
    [[ -n "$(jq -cS -f "$FILTER" <<< "$durable")" ]] || {
        echo "durable workload history was omitted from the migratable snapshot" >&2
        exit 1
    }
done

cluster_trust_bundle_v1='{"apiVersion":"certificates.k8s.io/v1","kind":"ClusterTrustBundle","metadata":{"name":"custom.example:bundle"},"spec":{"signerName":"custom.example/signer","trustBundle":"-----BEGIN CERTIFICATE-----\\nsource\\n-----END CERTIFICATE-----"}}'
cluster_trust_bundle_beta='{"apiVersion":"certificates.k8s.io/v1beta1","kind":"ClusterTrustBundle","metadata":{"name":"custom.example:bundle"},"spec":{"signerName":"custom.example/signer","trustBundle":"-----BEGIN CERTIFICATE-----\\nsource\\n-----END CERTIFICATE-----"}}'
ctb_v1_identity="$(jq -cS '{apiGroup: ((.apiVersion | split("/")) | if length > 1 then .[0] else "" end), kind, namespace: (.metadata.namespace // ""), name: .metadata.name}' <<< "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_v1")")"
ctb_beta_identity="$(jq -cS '{apiGroup: ((.apiVersion | split("/")) | if length > 1 then .[0] else "" end), kind, namespace: (.metadata.namespace // ""), name: .metadata.name}' <<< "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_beta")")"
[[ "$ctb_v1_identity" == "$ctb_beta_identity" ]] || {
    echo "served API versions did not retain the same Kubernetes object identity" >&2
    exit 1
}
[[ "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_v1")" == "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_beta")" ]] || {
    echo "served ClusterTrustBundle API version changed its durable trust data" >&2
    exit 1
}
cluster_trust_bundle_changed="${cluster_trust_bundle_beta/source/source-changed}"
[[ "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_v1")" != "$(jq -cS -f "$FILTER" <<< "$cluster_trust_bundle_changed")" ]] || {
    echo "ClusterTrustBundle trust contents were normalized away with its served API version" >&2
    exit 1
}

stateful_source='{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"database","namespace":"apps"},"spec":{"minReadySeconds":0,"volumeClaimTemplates":[{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"data"},"spec":{"accessModes":["ReadWriteOnce"]}}]}}'
stateful_target='{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"database","namespace":"apps"},"spec":{"volumeClaimTemplates":[{"metadata":{"name":"data"},"spec":{"accessModes":["ReadWriteOnce"]}}]}}'
[[ "$(jq -cS -f "$FILTER" <<< "$stateful_source")" == "$(jq -cS -f "$FILTER" <<< "$stateful_target")" ]] || {
    echo "default minReadySeconds or nested PVC template type metadata changed StatefulSet semantics" >&2
    exit 1
}
stateful_changed='{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"database","namespace":"apps"},"spec":{"minReadySeconds":1,"volumeClaimTemplates":[{"metadata":{"name":"data"},"spec":{"accessModes":["ReadWriteOnce"]}}]}}'
[[ "$(jq -cS -f "$FILTER" <<< "$stateful_source")" != "$(jq -cS -f "$FILTER" <<< "$stateful_changed")" ]] || {
    echo "non-default StatefulSet minReadySeconds was normalized away" >&2
    exit 1
}

owned_rs_source='{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"web-old","namespace":"apps","ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"web","controller":true}]},"spec":{"replicas":2,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"web","image":"web:v1"}]}}}}'
owned_rs_target="${owned_rs_source/\"replicas\":2/\"replicas\":0}"
[[ "$(jq -cS -f "$FILTER" <<< "$owned_rs_source")" == "$(jq -cS -f "$FILTER" <<< "$owned_rs_target")" ]] || {
    echo "Deployment-owned ReplicaSet scale was not treated as controller-managed state" >&2
    exit 1
}
standalone_rs_source='{"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"standalone","namespace":"apps"},"spec":{"replicas":2}}'
standalone_rs_target="${standalone_rs_source/\"replicas\":2/\"replicas\":0}"
[[ "$(jq -cS -f "$FILTER" <<< "$standalone_rs_source")" != "$(jq -cS -f "$FILTER" <<< "$standalone_rs_target")" ]] || {
    echo "standalone ReplicaSet desired replica count was normalized away" >&2
    exit 1
}

cilium_node_source='{"apiVersion":"cilium.io/v2","kind":"CiliumNode","metadata":{"name":"node-a","labels":{"node.kubernetes.io/instance-type":"k3s","nodelet.dev/managed":"true","operator.example/pool":"blue"}},"spec":{"addresses":[{"ip":"192.0.2.10","type":"InternalIP"}],"health":{"ipv4":"10.0.0.10"}}}'
cilium_node_target="${cilium_node_source/k3s/nodelet}"
cilium_node_target="${cilium_node_target/true/false}"
cilium_node_target="${cilium_node_target/10.0.0.10/10.0.0.11}"
[[ "$(jq -cS -f "$FILTER" <<< "$cilium_node_source")" == "$(jq -cS -f "$FILTER" <<< "$cilium_node_target")" ]] || {
    echo "nodelet-generated CiliumNode labels or regenerated health IP changed normalized state" >&2
    exit 1
}
cilium_node_target="${cilium_node_target/192.0.2.10/192.0.2.11}"
[[ "$(jq -cS -f "$FILTER" <<< "$cilium_node_source")" != "$(jq -cS -f "$FILTER" <<< "$cilium_node_target")" ]] || {
    echo "CiliumNode spec was normalized away with runtime labels" >&2
    exit 1
}

hostpath_csi_source='{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"csi-hostpathplugin","namespace":"default","annotations":{"kubectl.kubernetes.io/last-applied-configuration":"source"}},"spec":{"template":{"spec":{"containers":[{"name":"hostpath","image":"hostpath:v1","args":["--kubelet-registration-path=/var/lib/kubelet/plugins/registry"],"volumeMounts":[{"name":"socket","mountPath":"/var/lib/kubelet/plugins"}]}],"volumes":[{"name":"socket","hostPath":{"path":"/var/lib/kubelet/plugins","type":"Directory"}},{"name":"csi-data","hostPath":{"path":"/var/lib/nodemigrate-csi-hostpath-data","type":"DirectoryOrCreate"}}]}}}}'
hostpath_csi_target='{"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"csi-hostpathplugin","namespace":"default","annotations":{"kubectl.kubernetes.io/last-applied-configuration":"target"}},"spec":{"template":{"spec":{"containers":[{"name":"hostpath","image":"hostpath:v1","args":["--kubelet-registration-path=/var/lib/nodelet/plugins/registry"],"volumeMounts":[{"name":"socket","mountPath":"/var/lib/nodelet/plugins"},{"name":"nodemigrate-source-csi-stage","mountPath":"/var/lib/kubelet/plugins/kubernetes.io/csi","mountPropagation":"Bidirectional"}]}],"volumes":[{"name":"socket","hostPath":{"path":"/var/lib/nodelet/plugins","type":"DirectoryOrCreate"}},{"name":"csi-data","hostPath":{"path":"/var/lib/nodemigrate-csi-hostpath-data","type":"DirectoryOrCreate"}},{"name":"nodemigrate-source-csi-stage","hostPath":{"path":"/var/lib/kubelet/plugins/kubernetes.io/csi","type":"DirectoryOrCreate"}}]}}}}'
[[ "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_source")" == "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_target")" ]] || {
    echo "fixture CSI runtime root and verified source-stage mount were not normalized narrowly" >&2
    exit 1
}
hostpath_csi_changed="$(jq -c '.spec.template.spec.containers[0].image = "hostpath:v2"' <<< "$hostpath_csi_target")"
[[ "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_source")" != "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_changed")" ]] || {
    echo "fixture CSI image changes were normalized away with runtime root adaptation" >&2
    exit 1
}
hostpath_csi_changed="$(jq -c '.spec.template.spec.volumes[1].hostPath.path = "/var/lib/changed-csi-data"' <<< "$hostpath_csi_target")"
[[ "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_source")" != "$(jq -cS -f "$FILTER" <<< "$hostpath_csi_changed")" ]] || {
    echo "fixture CSI catalog/data path changes were normalized away" >&2
    exit 1
}

echo "migration snapshot normalization checks passed"
