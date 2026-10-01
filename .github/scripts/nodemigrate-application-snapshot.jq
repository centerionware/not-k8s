[
  .items[]
  # The destination regenerates this trust bundle and its controller-owned
  # description annotation. Its CA contents are checked against the active API
  # CA separately at every migration stage.
  | select(.kind != "ConfigMap" or .metadata.name != "kube-root-ca.crt")
  | .kind as $kind
  | {
      apiVersion,
      kind,
      name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      type: (.type // ""),
      labels: (.metadata.labels // {}),
      annotations: (if .kind == "Secret" and .type == "kubernetes.io/service-account-token" then
        ((.metadata.annotations // {}) | del(."kubernetes.io/service-account.uid"))
      else (.metadata.annotations // {}) end),
      ownerReferences: [(.metadata.ownerReferences // [])[] | {apiVersion, kind, name, controller}],
      spec: ((.spec // {})
        | if ($kind == "StatefulSet" or $kind == "ReplicationController") and .minReadySeconds == 0 then
            del(.minReadySeconds)
          else . end
        | if has("volumeClaimTemplates") then
            .volumeClaimTemplates |= map(del(.apiVersion, .kind))
          else . end)
    }
] | sort_by(.kind, .namespace, .name)
