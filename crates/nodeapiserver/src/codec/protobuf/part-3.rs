
/// Real upstream Go struct embedding: a field declared as `Foo \`json:",inline"\``
/// has every one of `Foo`'s own fields flattened directly into the
/// *enclosing* JSON object with no wrapper key at all, while the
/// generated proto keeps it as an ordinary named nested-message field
/// (`optional Foo foo = N`) — confirmed directly against the vendored
/// `.proto` (`NamedRuleWithOperations.ruleWithOperations` and
/// `RuleWithOperations.rule`, both in
/// `vendor/protos/k8s.io/api/admissionregistration/v1/generated.proto`,
/// matching real upstream's own `k8s.io/api/admissionregistration/v1/types.go`
/// struct embedding). The core/v1 `Volume.volumeSource`,
/// `PersistentVolumeSpec.persistentVolumeSource`, and
/// `EphemeralContainer.ephemeralContainerCommon` fields have the same
/// flattened JSON shape, as do the core/v1 `LocalObjectReference` fields
/// in config-map and secret sources/selectors. Found live:
/// `ValidatingAdmissionPolicy`'s own
/// `spec.matchConstraints.resourceRules[]` round-tripped through a real
/// `nodestore` as entirely empty objects (every field but
/// `resourceNames` silently dropped) until this was special-cased —
/// every other message type in this codec really is just "recurse with
/// the same field-shaped JSON object", this is the one place two levels
/// of real upstream embedding needed a named exception.
fn is_inline_embedded_field(message: &str, json_name: &str) -> bool {
    // v1alpha1/v1beta1's own `NamedRuleWithOperations.ruleWithOperations`
    // both reference `v1`'s `RuleWithOperations` directly (confirmed in
    // the vendored proto -- neither version has its own copy of that
    // message), so a single `v1` entry for the inner field covers every
    // API version.
    matches!(
        (message, json_name),
        ("io.k8s.api.admissionregistration.v1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1beta1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1alpha1.NamedRuleWithOperations", "ruleWithOperations")
            | ("io.k8s.api.admissionregistration.v1.RuleWithOperations", "rule")
            | ("io.k8s.api.core.v1.Volume", "volumeSource")
            | ("io.k8s.api.core.v1.PersistentVolumeSpec", "persistentVolumeSource")
            | ("io.k8s.api.core.v1.EphemeralContainer", "ephemeralContainerCommon")
            | ("io.k8s.api.core.v1.ConfigMapEnvSource", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapKeySelector", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapProjection", "localObjectReference")
            | ("io.k8s.api.core.v1.ConfigMapVolumeSource", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretEnvSource", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretKeySelector", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretProjection", "localObjectReference")
            | ("io.k8s.api.core.v1.SecretVolumeSource", "localObjectReference")
    )
}
