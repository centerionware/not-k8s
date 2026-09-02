
/// Real upstream's own well-known-type special case, confirmed directly
/// against the vendored proto rather than guessed at: `metav1.Time`
/// (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/generated.proto`'s
/// own `message Time`) is represented in JSON as a plain RFC3339 string
/// (`creationTimestamp: "2024-01-01T00:00:00Z"`), but its vendored proto
/// message wraps `{seconds: int64 = 1, nanos: int32 = 2}` — the same
/// shape `google.protobuf.Timestamp` uses — because real upstream's own
/// Go marshaller (`Time.MarshalTo`/`Time.Unmarshal`) hand-converts
/// between the two. `MicroTime` uses the identical wire shape. This is
/// the *only* place this otherwise fully generic, reflection-driven
/// encoder needs a named exception — every other message type really is
/// just "recurse with the same field-shaped JSON object" the rest of
/// this module assumes. Found live: nothing exercised this crate's own
/// `encode_message`/`decode_message` against a real object carrying a
/// real `creationTimestamp` end to end (through an actual protobuf
/// round trip against a real datastore) until
/// `tests/encryption_roundtrip.rs`'s own live round trip did — every
/// prior test either stayed at the JSON/YAML codec layer (which never
/// hits this at all) or unit-tested `encode_message`/`decode_message`
/// with hand-built fixtures that happened never to include a `Time`
/// field.
fn is_time_message(message: &str) -> bool {
    matches!(message, "io.k8s.apimachinery.pkg.apis.meta.v1.Time" | "io.k8s.apimachinery.pkg.apis.meta.v1.MicroTime")
}
