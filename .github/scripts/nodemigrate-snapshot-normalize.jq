select(
  .kind as $kind
  | (["ComponentStatus", "ControllerRevision", "Endpoints", "EndpointSlice",
      "Event", "Lease", "Node", "NodeMetrics", "Pod", "PodMetrics",
      "ReplicaSet", "VolumeAttachment"]
      | index($kind)) == null
)
| select((.kind != "Secret") or (.type != "kubernetes.io/service-account-token"))
| del(.status, .metadata.uid, .metadata.creationTimestamp,
      .metadata.deletionTimestamp, .metadata.deletionGracePeriodSeconds,
      .metadata.generation, .metadata.managedFields,
      .metadata.resourceVersion, .metadata.selfLink)
| if .metadata.ownerReferences then
    .metadata.ownerReferences |= map(del(.uid))
  else . end
| if .spec.claimRef then .spec.claimRef |= del(.uid) else . end
