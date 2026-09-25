def controller_regenerated_pod:
  .kind == "Pod" and (
    ((.metadata.annotations // {}) | has("kubernetes.io/config.mirror")) or
    any((.metadata.ownerReferences // [])[]?; .controller == true)
  );

select(
  .kind as $kind
  | (["ComponentStatus", "Endpoints", "EndpointSlice", "Event", "Lease",
      "Node", "NodeMetrics", "PodMetrics", "VolumeAttachment"]
      | index($kind)) == null
)
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
