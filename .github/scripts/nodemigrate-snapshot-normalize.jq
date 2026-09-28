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

def deployment_owned_replicaset:
  .kind == "ReplicaSet" and
  any((.metadata.ownerReferences // [])[]?;
    .controller == true and .kind == "Deployment"
  );

def migration_fixture_hostpath_csi_statefulset:
  .kind == "StatefulSet" and .metadata.namespace == "default" and
  (.metadata.name == "csi-hostpathplugin" or .metadata.name == "csi-hostpath-socat");

def normalize_fixture_csi_runtime_string:
  if type == "string" then
    gsub("/var/lib/nodelet"; "/var/lib/kubelet") | gsub("nodelet"; "kubelet")
  else . end;

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
| del(.status, .metadata.uid, .metadata.creationTimestamp,
      .metadata.deletionTimestamp, .metadata.deletionGracePeriodSeconds,
      .metadata.generation, .metadata.managedFields,
      .metadata.resourceVersion, .metadata.selfLink)
| if .metadata.ownerReferences then
    .metadata.ownerReferences |= map(del(.uid))
  else . end
| if .spec.claimRef then .spec.claimRef |= del(.uid) else . end
| if .kind == "PriorityClass" and .globalDefault == false then del(.globalDefault) else . end
| if .kind == "StatefulSet" and .spec.volumeClaimTemplates then
    .spec.volumeClaimTemplates |= map(del(.apiVersion, .kind))
  else . end
| if (.kind == "StatefulSet" or .kind == "ReplicationController") and .spec.minReadySeconds == 0 then
    .spec |= del(.minReadySeconds)
  else . end
| if deployment_owned_replicaset then .spec |= del(.replicas) else . end
| if .kind == "CiliumNode" then
    .metadata.labels = ((.metadata.labels // {})
      | del(."node.kubernetes.io/instance-type", ."nodelet.dev/managed"))
    | if .spec.health then .spec.health |= del(.ipv4) else . end
  else . end
| if .kind == "Secret" and .type == "kubernetes.io/service-account-token" then
    .metadata.annotations |= del(."kubernetes.io/service-account.uid")
    | .data |= del(.token, .namespace, ."ca.crt")
  else . end
| if migration_fixture_hostpath_csi_statefulset then
    # The integration harness reinstalls this test-only CSI driver after each
    # runtime cutover: its socket/plugin roots move from kubelet to nodelet,
    # and a dedicated bind exposes the preserved source CSI global stage. The
    # installer checks those exact runtime paths and stage visibility; keep all
    # other driver fields strict, including its persistent volume catalog.
    .metadata.annotations |= del(."kubectl.kubernetes.io/last-applied-configuration")
    | .spec.template.spec.volumes |= map(
        select(.name != "nodemigrate-source-csi-stage")
        | walk(normalize_fixture_csi_runtime_string)
        | if ((.hostPath.path // "") | startswith("/var/lib/kubelet")) then
            .hostPath |= del(.type)
          else . end
      )
    | .spec.template.spec.containers |= map(
        .volumeMounts |= map(
          select(.name != "nodemigrate-source-csi-stage")
          | walk(normalize_fixture_csi_runtime_string)
        )
        | walk(normalize_fixture_csi_runtime_string)
      )
  else . end
| if .kind == "ClusterTrustBundle" then
    # Kubernetes 1.37 serves certificates.k8s.io/v1 while the current target
    # serves v1beta1. Compare the durable signer and bundle under one identity.
    .apiVersion = "certificates.k8s.io/migration"
  else . end
| if system_coredns_rbac_bootstrap_metadata then
    (if (.metadata.labels | type) == "object" then
       .metadata.labels |= del(."kubernetes.io/bootstrapping")
     else . end)
    | (if (.metadata.annotations | type) == "object" then
         .metadata.annotations |= del(."rbac.authorization.kubernetes.io/autoupdate")
       else . end)
  else . end
