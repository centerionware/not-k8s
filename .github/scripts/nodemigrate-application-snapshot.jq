[
  .items[]
  # The destination regenerates this trust bundle and its controller-owned
  # description annotation. Its CA contents are checked against the active API
  # CA separately at every migration stage.
  | select(.kind != "ConfigMap" or .metadata.name != "kube-root-ca.crt")
  | {
      apiVersion,
      kind,
      name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      labels: (.metadata.labels // {}),
      annotations: (.metadata.annotations // {}),
      ownerReferences: [(.metadata.ownerReferences // [])[] | {apiVersion, kind, name, controller}],
      spec: ((.spec // {}) | if has("volumeClaimTemplates") then
        .volumeClaimTemplates |= map(del(.apiVersion, .kind))
      else . end)
    }
] | sort_by(.kind, .namespace, .name)
