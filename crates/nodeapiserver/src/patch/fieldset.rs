//! Server-Side Apply's own `fieldpath.Set` (`sigs.k8s.io/structured-
//! merge-diff/v6/fieldpath`, fetched and read directly — a genuinely
//! separate GitHub repo from `kubernetes/kubernetes`, the same situation
//! `aggregator`'s own `kube-aggregator` was in) and its real JSON wire
//! shape — the exact bytes stored in a real `ManagedFieldsEntry.
//! fieldsV1` (`metadata.managedFields[].fieldsV1`). **No Rust crate
//! exists to reuse** (confirmed, `docs/APISERVER_PLAN.md` finding 8) —
//! this is a from-scratch, verified-against-real-source port, the same
//! posture `apiextensions`'s schema-walking modules already took for the
//! same reason.
//!
//! # The real wire shape, confirmed directly (`fieldpath/serialize.go`'s
//! `emitContentsV1`/`readIterV1`)
//!
//! `fieldsV1` is a recursive JSON tree keyed by *serialized path
//! elements* (`fieldpath/serialize-pe.go`'s `SerializePathElement`/
//! `DeserializePathElement`), not plain field names:
//! - `"f:<name>"` — a map/struct field, by name.
//! - `"k:{<name>:<value>,...}"` — an associative-list element, by its
//!   own key fields (`x-kubernetes-list-map-keys`), sorted alphabetically
//!   by field name within the object.
//! - `"v:<json value>"` — a set-typed list element (a primitive-typed
//!   associative list, `x-kubernetes-list-type: set`), by its own value.
//! - `"i:<index>"` — an atomic-list element, by numeric index (real
//!   upstream emits these only for a legacy `PatchStrategy` case this
//!   build doesn't model yet — see [`PathElement`]'s own doc comment).
//!
//! Each key maps to a nested object: `{}` (empty) means "this exact path
//! is itself a member of the set, with no further tracked children";
//! a non-empty object means "this path has tracked children" (the
//! entries below the split are `Set.Children`, real upstream's own
//! separate tree from `Set.Members` — a node can be *both*, which is
//! where the real, easy-to-miss `"."` marker comes in); a node that is
//! both a member *and* has children gets a synthetic `"."` : `{}` entry
//! alongside its real children to say "this path is itself a member
//! too" (`emitContentsV1`'s own `includeSelf` parameter) — omitted
//! entirely for the common case (no children) since the empty-object
//! form already says "member, no children" unambiguously.
//!
//! # What this module is, and isn't, yet
//!
//! This is Server-Side Apply's own foundational **data structure**
//! only — the real, verified `PathElement`/`Set` shapes and their exact
//! JSON encoding, faithfully round-tripping any real `fieldsV1` document
//! this build might read from a real object's `managedFields`. **The
//! actual 3-way merge/conflict-detection algorithm
//! (`typed.mergingWalker`, schema-driven: atomic vs. associative
//! lists/maps per the vendored `x-kubernetes-*` annotations Group G's
//! own `strategic_merge` already reads) is a separate, much larger,
//! not-yet-started piece** — this module is the land-the-primitive-first
//! step every other group in this arc has taken, not a claim that
//! `PATCH`/`APPLY` with `application/apply-patch+yaml` works yet (it
//! doesn't — `server::rest::patch_kind_for_content_type` still rejects
//! that media type outright, named honestly in that function's own doc
//! comment).

use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// `fieldpath.PathElement` — exactly one of four real shapes, confirmed
/// directly against `fieldpath/element.go`. `Index` is real upstream's
/// own but is emitted only by its legacy `extractItems`-from-
/// `PatchStrategy`-less atomic list handling; this build's own
/// `x-kubernetes-list-type: atomic` lists are never individually tracked
/// at all (an atomic list is a single leaf, matching real upstream's own
/// "atomic = whole-value ownership" rule) — `Index` is modeled here for
/// a faithful *decode* of any real `fieldsV1` this build might read
/// (defensive: never fabricate data loss on an unrecognized-but-valid
/// shape), even though nothing in this build constructs one yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathElement {
    /// `f:<name>` — a struct/map field, by name.
    Field(String),
    /// `k:{...}` — an associative-list element, by its own key fields
    /// (name, already-encoded JSON value), sorted by name — matching
    /// real upstream's own `value.FieldList`'s sort-by-name invariant.
    Key(Vec<(String, Value)>),
    /// `v:<value>` — a set-typed (primitive-element) list item, by its
    /// own value.
    Value(Value),
    /// `i:<n>` — an atomic-list element, by index. See this enum's own
    /// doc comment for why this build never constructs one.
    Index(i64),
}

/// Real upstream's own `PathElement.Compare` ordering: `Field` <
/// non-`Field`; within the "not-Field" tier, `Key` < `Value` < `Index`
/// — confirmed directly (`element.go`'s own cascading nil-checks, which
/// is exactly a lexicographic tier ordering over which variant is set).
fn variant_rank(pe: &PathElement) -> u8 {
    match pe {
        PathElement::Field(_) => 0,
        PathElement::Key(_) => 1,
        PathElement::Value(_) => 2,
        PathElement::Index(_) => 3,
    }
}

/// A total order over `serde_json::Value` sufficient for this module's
/// own needs (deterministic `BTreeMap` ordering) — **not** claimed to
/// match real upstream's own `value.Compare` byte-for-byte (that
/// function's own real tiered type ordering, `apimachinery`'s
/// `value` package, is separate machinery this build has no reason to
/// reproduce exactly: nothing here depends on cross-implementation
/// ordering agreement, only on this build's own encode/decode round trip
/// being internally consistent, which any total order satisfies).
fn compare_values(a: &Value, b: &Value) -> Ordering {
    // Compare by the value's own compact JSON encoding -- simple, total,
    // and stable regardless of which JSON type is involved (numbers
    // sort lexicographically as text, not numerically, which is fine:
    // this is an ordering key only, never surfaced to a caller).
    let sa = serde_json::to_string(a).unwrap_or_default();
    let sb = serde_json::to_string(b).unwrap_or_default();
    sa.cmp(&sb)
}

impl PartialOrd for PathElement {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PathElement {
    fn cmp(&self, other: &Self) -> Ordering {
        match variant_rank(self).cmp(&variant_rank(other)) {
            Ordering::Equal => {}
            ord => return ord,
        }
        match (self, other) {
            (PathElement::Field(a), PathElement::Field(b)) => a.cmp(b),
            (PathElement::Key(a), PathElement::Key(b)) => {
                // Real upstream's own `value.FieldList.Compare`: pairwise
                // by (name, value), shorter-is-less on a common prefix.
                for ((an, av), (bn, bv)) in a.iter().zip(b.iter()) {
                    match an.cmp(bn) {
                        Ordering::Equal => {}
                        ord => return ord,
                    }
                    match compare_values(av, bv) {
                        Ordering::Equal => {}
                        ord => return ord,
                    }
                }
                a.len().cmp(&b.len())
            }
            (PathElement::Value(a), PathElement::Value(b)) => compare_values(a, b),
            (PathElement::Index(a), PathElement::Index(b)) => a.cmp(b),
            _ => unreachable!("variant_rank already separated differing variants"),
        }
    }
}

/// `fieldpath/serialize-pe.go`'s own `SerializePathElement` — exact
/// prefix bytes (`f:`/`k:`/`v:`/`i:`), `Key`'s own fields written as a
/// compact JSON object in field order (already-sorted, this type's own
/// invariant), confirmed directly.
pub fn serialize_path_element(pe: &PathElement) -> String {
    match pe {
        PathElement::Field(name) => format!("f:{name}"),
        PathElement::Key(fields) => {
            let mut obj = Map::new();
            for (name, value) in fields {
                obj.insert(name.clone(), value.clone());
            }
            format!("k:{}", Value::Object(obj))
        }
        PathElement::Value(value) => format!("v:{value}"),
        PathElement::Index(i) => format!("i:{i}"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeserializeError {
    #[error("key must be at least 2 characters long: {0:?}")]
    TooShort(String),
    #[error("missing colon separator: {0:?}")]
    MissingColon(String),
    #[error("unknown path element type {0:?}")]
    UnknownType(String),
    #[error("invalid JSON in path element {0:?}: {1}")]
    InvalidJson(String, #[source] serde_json::Error),
    #[error("k: path element is not a JSON object: {0:?}")]
    KeyNotAnObject(String),
    #[error("i: path element is not a valid integer: {0:?}")]
    InvalidIndex(String),
}

/// `fieldpath/serialize-pe.go`'s own `DeserializePathElement`, ported
/// faithfully. An unrecognized type prefix is a real, named error here
/// (`UnknownType`) — real upstream instead silently skips such a key
/// during a full `Set` decode (`readIterV1`'s own `ErrUnknownPathElementType`
/// handling: "ignore these -- a future version maybe knows what they
/// are"); [`Set::from_json`] below reproduces that *tree-level* skip
/// behavior itself, calling this function and discarding an
/// `UnknownType` error rather than propagating it, so the end-to-end
/// decode contract matches upstream even though this lower-level
/// function itself reports the error rather than swallowing it.
pub fn deserialize_path_element(s: &str) -> Result<PathElement, DeserializeError> {
    if s.len() < 2 {
        return Err(DeserializeError::TooShort(s.to_string()));
    }
    let (prefix, rest) = s.split_at(2);
    let Some(kind) = prefix.chars().next() else { return Err(DeserializeError::TooShort(s.to_string())) };
    if prefix.chars().nth(1) != Some(':') {
        return Err(DeserializeError::MissingColon(s.to_string()));
    }
    match kind {
        'f' => Ok(PathElement::Field(rest.to_string())),
        'v' => {
            let value: Value = serde_json::from_str(rest).map_err(|e| DeserializeError::InvalidJson(s.to_string(), e))?;
            Ok(PathElement::Value(value))
        }
        'i' => rest.parse::<i64>().map(PathElement::Index).map_err(|_| DeserializeError::InvalidIndex(s.to_string())),
        'k' => {
            let value: Value = serde_json::from_str(rest).map_err(|e| DeserializeError::InvalidJson(s.to_string(), e))?;
            let Value::Object(obj) = value else { return Err(DeserializeError::KeyNotAnObject(s.to_string())) };
            let mut fields: Vec<(String, Value)> = obj.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(PathElement::Key(fields))
        }
        _ => Err(DeserializeError::UnknownType(s.to_string())),
    }
}

/// `fieldpath.Set` — a real tree of owned field paths, faithfully
/// round-tripping `fieldsV1`'s own recursive JSON shape (this module's
/// own doc comment covers the exact wire format). `members`/`children`
/// mirror real upstream's own separate `Members`/`Children` fields on
/// the Go type exactly (not merged into one map) — a path can be a
/// member, have children, or both, and upstream's own `"."` marker only
/// exists to represent that third case, so keeping the two separate
/// here is what makes this module's own encode logic a direct port
/// rather than a reinterpretation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Set {
    pub members: Vec<PathElement>,
    pub children: BTreeMap<PathElement, Set>,
}

impl Set {
    pub fn new() -> Self {
        Set::default()
    }

    /// Inserts `path` (a sequence of `PathElement`s from the root) as a
    /// member of this set, creating intermediate child nodes as needed
    /// — real upstream's own `Set.Insert`.
    pub fn insert(&mut self, path: &[PathElement]) {
        match path.split_first() {
            None => {}
            Some((head, [])) => {
                if !self.members.contains(head) {
                    self.members.push(head.clone());
                    self.members.sort();
                }
            }
            Some((head, rest)) => {
                self.children.entry(head.clone()).or_default().insert(rest);
            }
        }
    }

    /// Whether `path` is a member of this set — real upstream's own
    /// `Set.Has`.
    pub fn has(&self, path: &[PathElement]) -> bool {
        match path.split_first() {
            None => false,
            Some((head, [])) => self.members.contains(head),
            Some((head, rest)) => self.children.get(head).is_some_and(|child| child.has(rest)),
        }
    }

    /// `fieldpath/serialize.go`'s own `ToJSON`/`emitContentsV1`, ported.
    pub fn to_json(&self) -> Value {
        self.emit(false)
    }

    fn emit(&self, include_self: bool) -> Value {
        let mut obj = Map::new();
        if include_self && !(self.members.is_empty() && self.children.is_empty()) {
            obj.insert(".".to_string(), Value::Object(Map::new()));
        }
        for m in &self.members {
            // A member that's also a child-tree root gets its full
            // subtree (with its own `"."` marker) instead of an empty
            // object -- matching real upstream's own merge-by-key walk
            // over Members and Children in lockstep.
            if let Some(child) = self.children.get(m) {
                obj.insert(serialize_path_element(m), child.emit(true));
            } else {
                obj.insert(serialize_path_element(m), Value::Object(Map::new()));
            }
        }
        for (pe, child) in &self.children {
            if self.members.contains(pe) {
                continue; // already emitted above, with its "." marker
            }
            obj.insert(serialize_path_element(pe), child.emit(false));
        }
        Value::Object(obj)
    }

    /// `fieldpath/serialize.go`'s own `FromJSON`/`readIterV1`, ported —
    /// an unrecognized path-element type is silently dropped (matching
    /// upstream's own documented "ignore these" posture, not a decode
    /// failure), everything else propagates as a real error.
    pub fn from_json(value: &Value) -> Result<Set, DeserializeError> {
        let Value::Object(obj) = value else {
            return Ok(Set::default());
        };
        let mut set = Set::default();
        for (key, nested) in obj {
            if key == "." {
                continue; // handled by the caller of this node, not here
            }
            let pe = match deserialize_path_element(key) {
                Ok(pe) => pe,
                Err(DeserializeError::UnknownType(_)) => continue,
                Err(e) => return Err(e),
            };
            let child = Set::from_json(nested)?;
            let is_member = nested.get(".").is_some() || (child.members.is_empty() && child.children.is_empty());
            if is_member {
                set.members.push(pe.clone());
            }
            if !child.members.is_empty() || !child.children.is_empty() {
                set.children.insert(pe, child);
            }
        }
        set.members.sort();
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_serializes_with_the_real_f_prefix() {
        assert_eq!(serialize_path_element(&PathElement::Field("spec".to_string())), "f:spec");
    }

    #[test]
    fn index_serializes_with_the_real_i_prefix() {
        assert_eq!(serialize_path_element(&PathElement::Index(3)), "i:3");
    }

    #[test]
    fn value_serializes_its_own_json_after_the_v_prefix() {
        assert_eq!(serialize_path_element(&PathElement::Value(json!("foo"))), "v:\"foo\"");
    }

    #[test]
    fn key_serializes_as_a_compact_json_object_after_the_k_prefix() {
        let pe = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert_eq!(serialize_path_element(&pe), "k:{\"name\":\"nginx\"}");
    }

    #[test]
    fn deserialize_round_trips_every_real_variant() {
        for pe in [PathElement::Field("spec".to_string()), PathElement::Index(3), PathElement::Value(json!(true)), PathElement::Key(vec![("name".to_string(), json!("nginx"))])] {
            let s = serialize_path_element(&pe);
            assert_eq!(deserialize_path_element(&s).unwrap(), pe, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn an_unknown_type_prefix_is_a_named_error_at_the_element_level() {
        assert!(matches!(deserialize_path_element("x:whatever"), Err(DeserializeError::UnknownType(_))));
    }

    #[test]
    fn a_leaf_member_with_no_children_encodes_as_an_empty_object() {
        let mut set = Set::new();
        set.insert(&[PathElement::Field("spec".to_string()), PathElement::Field("replicas".to_string())]);
        let doc = set.to_json();
        assert_eq!(doc, json!({"f:spec": {"f:replicas": {}}}));
    }

    #[test]
    fn a_member_that_also_has_children_gets_the_real_dot_marker() {
        // metadata.labels is itself owned (the whole map was set) AND
        // metadata.labels.app is separately tracked as a child -- the
        // one real case upstream's own "." marker exists for.
        let mut set = Set::new();
        set.insert(&[PathElement::Field("metadata".to_string()), PathElement::Field("labels".to_string())]);
        set.insert(&[PathElement::Field("metadata".to_string()), PathElement::Field("labels".to_string()), PathElement::Field("app".to_string())]);
        let doc = set.to_json();
        assert_eq!(doc, json!({"f:metadata": {"f:labels": {".": {}, "f:app": {}}}}));
    }

    #[test]
    fn a_real_fieldsv1_document_round_trips_through_from_json_and_to_json() {
        let doc = json!({
            "f:metadata": {
                "f:labels": {".": {}, "f:app": {}},
            },
            "f:spec": {
                "f:replicas": {},
                "f:containers": {"k:{\"name\":\"nginx\"}": {"f:image": {}}},
            },
        });
        let set = Set::from_json(&doc).unwrap();
        assert!(set.has(&[PathElement::Field("metadata".to_string()), PathElement::Field("labels".to_string())]));
        assert!(set.has(&[PathElement::Field("metadata".to_string()), PathElement::Field("labels".to_string()), PathElement::Field("app".to_string())]));
        assert!(set.has(&[PathElement::Field("spec".to_string()), PathElement::Field("replicas".to_string())]));
        assert!(set.has(&[PathElement::Field("spec".to_string()), PathElement::Field("containers".to_string()), PathElement::Key(vec![("name".to_string(), json!("nginx"))]), PathElement::Field("image".to_string())]));
        assert_eq!(set.to_json(), doc, "a real fieldsV1 document must round trip byte-for-byte (modulo JSON key order)");
    }

    #[test]
    fn has_is_false_for_a_path_never_inserted() {
        let mut set = Set::new();
        set.insert(&[PathElement::Field("spec".to_string())]);
        assert!(!set.has(&[PathElement::Field("status".to_string())]));
        assert!(!set.has(&[PathElement::Field("spec".to_string()), PathElement::Field("replicas".to_string())]));
    }

    #[test]
    fn an_empty_path_is_never_a_member() {
        let set = Set::new();
        assert!(!set.has(&[]));
    }
}
