def controller_regenerated_pod:
  .kind == "Pod" and (
    ((.metadata.annotations // {}) | has("kubernetes.io/config.mirror")) or
    any((.metadata.ownerReferences // [])[]?; .controller == true)
  );

def default_kubernetes_service_endpoint:
  .metadata.namespace == "default" and (
    .metadata.name == "kubernetes" or
    ((.metadata.labels // {})["kubernetes.io/service-name"] == "kubernetes")
  );

select(
  .kind as $kind
  | (["ComponentStatus", "Event",
      "Node", "NodeMetrics", "PodMetrics", "VolumeAttachment"]
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
| select(controller_regenerated_pod | not)
| select((.kind != "Secret") or (.type != "kubernetes.io/service-account-token"))
| del(.status, .metadata.uid, .metadata.creationTimestamp,
      .metadata.deletionTimestamp, .metadata.deletionGracePeriodSeconds,
      .metadata.generation, .metadata.managedFields,
      .metadata.resourceVersion, .metadata.selfLink)
| if .metadata.ownerReferences then
    .metadata.ownerReferences |= map(del(.uid))
  else . end
| if .spec.claimRef then .spec.claimRef |= del(.uid) else . end
