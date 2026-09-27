def controller_regenerated_pod:
  .kind == "Pod" and (
    ((.metadata.annotations // {}) | has("kubernetes.io/config.mirror")) or
    any((.metadata.ownerReferences // [])[]?; .controller == true)
  );

def regenerated_system_addon_replica_set:
  .kind == "ReplicaSet" and .metadata.namespace == "kube-system" and
  any((.metadata.ownerReferences // [])[]?;
    .controller == true and .kind == "Deployment" and
    (.name == "coredns" or .name == "local-path-provisioner" or
     .name == "metrics-server")
  );

def regenerated_system_lease:
  .kind == "Lease" and .metadata.namespace == "kube-system" and (
    .metadata.name == "kube-controller-manager" or
    .metadata.name == "kube-scheduler" or
    ((.metadata.name // "") | startswith("apiserver-")) or
    .metadata.name == "cert-manager-cainjector-leader-election" or
    .metadata.name == "cert-manager-controller" or
    .metadata.name == "cilium-operator-resource-lock"
  );

def system_coredns_rbac_bootstrap_metadata:
  (.kind == "ClusterRole" or .kind == "ClusterRoleBinding") and
  .metadata.name == "system:coredns";

def default_kubernetes_service_endpoint:
  .metadata.namespace == "default" and (
    .metadata.name == "kubernetes" or
    ((.metadata.labels // {})["kubernetes.io/service-name"] == "kubernetes")
  );

select(
  .kind as $kind
  | (["ComponentStatus", "Event",
      "Node", "NodeMetrics", "PodMetrics", "VolumeAttachment",
      # Cilium recreates these per-node/per-Pod runtime identities from the
      # current CNI and Pod state. They are not durable migration payload.
      "CiliumEndpoint", "CiliumIdentity"]
      | index($kind)) == null
)
| select((.kind != "Endpoints") or (
    (default_kubernetes_service_endpoint | not) and
    ((.metadata.labels // {})["endpoints.kubernetes.io/managed-by"] != "endpoint-controller")
  ))
| select((.kind != "EndpointSlice") or (
    (default_kubernetes_service_endpoint | not) and
    ((.metadata.labels // {})["endpointslice.kubernetes.io/managed-by"] as $managed_by
      | $managed_by != "endpointslice-controller.k8s.io" and
        $managed_by != "endpointslicemirroring-controller.k8s.io")
  ))
| select((.kind != "Lease") or (.metadata.namespace != "kube-node-lease"))
| select((.kind != "ConfigMap") or (.metadata.name != "kube-root-ca.crt"))
# nodebootstrap regenerates request-header trust from the destination PKI;
# the fixture separately requires metrics API discovery to work after cutover.
| select((.kind != "ConfigMap") or
    (.metadata.namespace != "kube-system" or
     .metadata.name != "extension-apiserver-authentication"))
| select(controller_regenerated_pod | not)
| select(regenerated_system_addon_replica_set | not)
| select(regenerated_system_lease | not)
| select(.kind != "CSINode")
| select((.kind != "Secret") or (.type != "kubernetes.io/service-account-token"))
| del(.status, .metadata.uid, .metadata.creationTimestamp,
      .metadata.deletionTimestamp, .metadata.deletionGracePeriodSeconds,
      .metadata.generation, .metadata.managedFields,
      .metadata.resourceVersion, .metadata.selfLink)
| if .metadata.ownerReferences then
    .metadata.ownerReferences |= map(del(.uid))
  else . end
| if .spec.claimRef then .spec.claimRef |= del(.uid) else . end
| if .kind == "PriorityClass" and .globalDefault == false then del(.globalDefault) else . end
| if system_coredns_rbac_bootstrap_metadata then
    (if (.metadata.labels | type) == "object" then
       .metadata.labels |= del(."kubernetes.io/bootstrapping")
     else . end)
    | (if (.metadata.annotations | type) == "object" then
         .metadata.annotations |= del(."rbac.authorization.kubernetes.io/autoupdate")
       else . end)
  else . end
