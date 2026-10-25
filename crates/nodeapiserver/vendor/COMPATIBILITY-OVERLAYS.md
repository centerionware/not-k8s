# Additive upstream API compatibility overlays

The base built-in API schema and protobuf metadata are vendored from
`kubernetes/kubernetes@release-1.34`. This file records narrowly scoped newer
optional fields that the API server accepts and preserves while its advertised
base version remains 1.34.

## Kubernetes v1.37.1: `CSIDriverSpec.preventPodSchedulingIfMissing`

- Upstream source: `kubernetes/kubernetes@v1.37.1`
- OpenAPI source: `api/openapi-spec/v3/apis__storage.k8s.io__v1_openapi.json`,
  schema `io.k8s.api.storage.v1.CSIDriverSpec`
- Protobuf source: `staging/src/k8s.io/api/storage/v1/generated.proto`,
  message `CSIDriverSpec`, field number 11
- Purpose: retain the opt-in CSI node-registration scheduling policy when a
  cluster migrates from Kubernetes 1.37 to the current not-k8s API server.
- Runtime behavior: `nodescheduler` rejects CSI-backed Pods on nodes that have
  not registered a driver whose CSIDriver opts into the field.

The vendor refresh script replaces the base upstream directories. After a
refresh, re-apply and re-verify this overlay against the selected upstream
release, or remove it only after the base schema contains the field.
