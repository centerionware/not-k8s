.items as $items
| [
  $items[] as $item
  # The destination regenerates this trust bundle and its controller-owned
  # description annotation. Its CA contents are checked against the active API
  # CA separately at every migration stage.
  | select($item.kind != "ConfigMap" or $item.metadata.name != "kube-root-ca.crt")
  | $item.kind as $kind
  | {
      apiVersion: $item.apiVersion,
      kind: $item.kind,
      name: $item.metadata.name,
      namespace: ($item.metadata.namespace // ""),
      type: ($item.type // ""),
      labels: ($item.metadata.labels // {}),
      annotations: (if $item.kind == "Secret" and $item.type == "kubernetes.io/service-account-token" then
        (($item.metadata.annotations // {}) | del(."kubernetes.io/service-account.uid"))
      elif $item.kind == "ReplicaSet" and any($items[];
          .kind == "HorizontalPodAutoscaler" and
          .metadata.namespace == $item.metadata.namespace and
          .spec.scaleTargetRef.kind == "Deployment" and
          (.spec.scaleTargetRef.name as $target_name | any($item.metadata.ownerReferences[]?;
            .kind == "Deployment" and .name == $target_name))) then
        (($item.metadata.annotations // {}) | del(."deployment.kubernetes.io/desired-replicas", ."deployment.kubernetes.io/max-replicas"))
      else ($item.metadata.annotations // {}) end),
      ownerReferences: [($item.metadata.ownerReferences // [])[] | {apiVersion, kind, name, controller}],
      spec: (($item.spec // {})
        # An HPA owns Deployment.spec.replicas. Keep the autoscaling policy
        # itself in this snapshot, while allowing its controller-managed
        # replica count to change during the behavioral migration checks.
        | if $kind == "Deployment" and any($items[];
            .kind == "HorizontalPodAutoscaler" and
            .spec.scaleTargetRef.kind == "Deployment" and
            .spec.scaleTargetRef.name == $item.metadata.name)
          then del(.replicas) else . end
        | if ($kind == "StatefulSet" or $kind == "ReplicationController") and .minReadySeconds == 0 then
            del(.minReadySeconds)
          else . end
        | if has("volumeClaimTemplates") then
            .volumeClaimTemplates |= map(del(.apiVersion, .kind))
          else . end)
    }
] | sort_by(.kind, .namespace, .name)
