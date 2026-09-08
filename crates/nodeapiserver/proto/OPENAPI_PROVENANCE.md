# OpenAPI document wire schema

`openapi/v2/OpenAPIv2.proto` is unmodified from google/gnostic-models **v0.6.9**:
https://github.com/google/gnostic-models/blob/v0.6.9/openapiv2/OpenAPIv2.proto

Its Apache-2.0 notice is retained. This is the schema used for kubectl's
`application/com.github.proto-openapi.spec.v2@v1.0+protobuf` negotiation, not
Kubernetes resource protobuf. Build tooling compiles its descriptor; the server
converts the same vendored Swagger document served as JSON, preserving named
maps, references and YAML-backed vendor extensions. Refresh this file from
upstream when changing that wire model, and rerun the typed decode and real
kubectl apply regressions. No Go toolchain is required to build or run not-k8s.
