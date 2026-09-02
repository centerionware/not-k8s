
/// Group K's own well-known-type special case, the same shape
/// `is_time_message` already established and confirmed directly against
/// the vendored proto rather than guessed: `apiextensions.v1.JSON`
/// (`vendor/protos/k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/
/// v1/generated.proto`'s own `message JSON { optional bytes raw = 1; }`)
/// is how a `CustomResourceDefinition`'s own schema represents an
/// arbitrary JSON value — `JSONSchemaProps.default`/`.example`/`.enum`/
/// `.const`, wherever an operator's own schema can name a literal value
/// of any shape. In JSON the field is just that value directly (a
/// `default: "small"` looks exactly like every other scalar field), but
/// on the wire it's this one-field wrapper message whose `raw` holds the
/// value's own JSON encoding as bytes — real upstream's Go type
/// (`apiextensions.JSON`, a `[]byte` with hand-written `MarshalJSON`/
/// `UnmarshalJSON`) does the identical two-faced trick `metav1.Time`
/// does, just wrapping a whole JSON document instead of a timestamp.
/// Found live, the same way `is_time_message` was: nothing exercised a
/// real `CustomResourceDefinition` with a schema `default` through this
/// crate's own protobuf codec until `tests/crd_roundtrip.rs`'s live
/// round trip did.
/// `runtime.RawExtension`
/// (`vendor/protos/k8s.io/apimachinery/pkg/runtime/generated.proto`'s own
/// `message RawExtension { optional bytes raw = 1; }`, confirmed
/// directly) shares the *exact same* `{raw: bytes = 1}` wire shape and
/// "the whole JSON value lives in this one wrapped field" semantics as
/// `apiextensions.v1.JSON` above — real upstream's own `RawExtension.
/// MarshalJSON`/`UnmarshalJSON` store/emit `Raw` as the literal embedded
/// JSON document, not a base64 `bytes` field, identically to `JSON.Raw`.
/// Found via a deliberate audit pass (not a live failure this time):
/// after finding four separate real bugs of this exact class live this
/// session (`Time`, `apiextensions.v1.JSON`, `JSONSchemaPropsOrArray`/
/// `OrBool`, `IntOrString`), checking the vendored protos for
/// `RawExtension` ahead of the next real object that happens to carry
/// one (`Event.regarding`... no — `AdmissionReview.request.object`,
/// `WatchEvent`-adjacent dynamic fields, CRD conversion webhook payloads,
/// anywhere upstream models "an arbitrary embedded object"). Reuses
/// `encode_json_value`/`decode_json_message` directly — no new function
/// needed, since the wire shape and JSON semantics are identical.
fn is_json_message(message: &str) -> bool {
    matches!(
        message,
        "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSON" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSON" | "io.k8s.apimachinery.pkg.runtime.RawExtension"
    )
}
