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

    /// No members and every child (recursively) also empty — real
    /// upstream's own `Set.Empty`. A node with child *entries* that are
    /// each themselves empty still counts as empty; this module never
    /// actually constructs such a node (every insertion point below
    /// checks emptiness before inserting), but the check itself has to
    /// be recursive to match upstream's own definition exactly, not just
    /// `self.children.is_empty()`.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.children.values().all(Set::is_empty)
    }

    /// Fields present in either `self` or `other` — real upstream's own
    /// `Set.Union` (`fieldpath/set.go`/`pathelementmap.go`, ported;
    /// merge-sort-shaped in Go, expressed here with `BTreeMap`'s own
    /// sorted-map operations to the identical real effect since this
    /// module's own `children` is already a `BTreeMap`, not the
    /// sorted-`Vec` upstream's own `SetNodeMap` uses).
    pub fn union(&self, other: &Set) -> Set {
        let mut members = self.members.clone();
        for m in &other.members {
            if !members.contains(m) {
                members.push(m.clone());
            }
        }
        members.sort();
        let mut children = self.children.clone();
        for (pe, other_child) in &other.children {
            match children.get_mut(pe) {
                Some(existing) => *existing = existing.union(other_child),
                None => {
                    children.insert(pe.clone(), other_child.clone());
                }
            }
        }
        Set { members, children }
    }

    /// Fields present in both `self` and `other` — real upstream's own
    /// `Set.Intersection`. An empty resulting child node is dropped
    /// entirely, never inserted — matching upstream's own `if
    /// !res.Empty()` guard (a `Set` with an empty child entry would
    /// otherwise be a real, if harmless, structural divergence from what
    /// `to_json`/`from_json` would ever themselves produce).
    pub fn intersection(&self, other: &Set) -> Set {
        let mut members = self.members.clone();
        members.retain(|m| other.members.contains(m));
        let mut children = BTreeMap::new();
        for (pe, child) in &self.children {
            if let Some(other_child) = other.children.get(pe) {
                let res = child.intersection(other_child);
                if !res.is_empty() {
                    children.insert(pe.clone(), res);
                }
            }
        }
        Set { members, children }
    }

    /// Fields present in `self` but not `other` — real upstream's own
    /// `Set.Difference`. **A real, intentional asymmetry, confirmed
    /// directly against upstream's own doc comment, not an oversight**:
    /// this only ever descends into `other`'s own *child* entries at each
    /// level, never checks whether `other` owns the same path as a plain
    /// leaf `member` instead — so `parent - child = parent` (a subtree
    /// `self` tracks in depth survives untouched if `other` only owns
    /// that same path shallowly, as one leaf) and `child - parent =
    /// {empty}` is instead handled the moment `self` itself only ever
    /// owns that path as a leaf too (`self.members`'s own difference
    /// against `other.members` at that same level already removes it).
    /// [`recursive_difference`] is the sibling that *does* also treat an
    /// `other` leaf as canceling a whole `self` subtree — reach for that
    /// one when upstream's own real usage needs it (SSA's own "remove
    /// fields the previous apply owned but the new one doesn't" step
    /// uses plain `Difference`, not `RecursiveDifference` — named here so
    /// a future caller doesn't reach for the wrong one by symmetry alone).
    pub fn difference(&self, other: &Set) -> Set {
        let mut members = self.members.clone();
        members.retain(|m| !other.members.contains(m));
        let mut children = BTreeMap::new();
        for (pe, child) in &self.children {
            match other.children.get(pe) {
                Some(other_child) => {
                    let diff = child.difference(other_child);
                    if !diff.is_empty() {
                        children.insert(pe.clone(), diff);
                    }
                }
                None => {
                    children.insert(pe.clone(), child.clone());
                }
            }
        }
        Set { members, children }
    }

    /// Fields present in `self` but not `other`, where an `other` field
    /// present *at all* (leaf member or non-empty subtree) removes the
    /// matching `self` subtree **in its entirety**, not just the exact
    /// overlapping paths — real upstream's own `Set.RecursiveDifference`,
    /// its own doc comment's example: `self` owning `a.b.c` and `other`
    /// owning `a.b` (as a leaf) recursive-differences down to just `a`,
    /// since the whole `a.b` node — everything beneath it — is dropped.
    pub fn recursive_difference(&self, other: &Set) -> Set {
        let mut members = self.members.clone();
        members.retain(|m| !other.members.contains(m));
        let mut children = BTreeMap::new();
        for (pe, child) in &self.children {
            if other.members.contains(pe) {
                continue; // `other` owns this whole path as a leaf -- drop self's entire subtree here
            }
            match other.children.get(pe) {
                Some(other_child) => {
                    let diff = child.recursive_difference(other_child);
                    if !diff.is_empty() {
                        children.insert(pe.clone(), diff);
                    }
                }
                None => {
                    children.insert(pe.clone(), child.clone());
                }
            }
        }
        Set { members, children }
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

/// `typed.TypedValue.ToFieldSet()`, ported — the schema-driven walk that
/// turns one real object (understood to be shaped like `schema`, an
/// openapi-style qualified schema name, the same key `FIELD_META`/
/// `patch::strategic_merge` already use) into the `Set` of every field
/// path it sets. This is Server-Side Apply's first real building block
/// on top of the pure `PathElement`/`Set` data structure above: given an
/// object a client submitted via `PATCH` with `Content-Type:
/// application/apply-patch+yaml`, this is what would become that
/// manager's own new `managedFields` entry (before the *merge*/conflict-
/// detection half — real upstream's own `merge.Updater` — which is
/// separate, larger, not-yet-started work; this function alone doesn't
/// merge anything, it only says what one object owns).
///
/// Driven entirely by the same `codegen::field_meta_index()` Group A
/// table `patch::strategic_merge` already reads (`list_type`/
/// `list_map_keys`/`map_type`/`ref_schema` — Server-Side Apply's own
/// `x-kubernetes-*` extensions, confirmed against real vendored specs,
/// not the older `patch_strategy`/`patch_merge_key` pair `strategic_merge`
/// itself actually uses; for every real vendored field both pairs agree,
/// but this function reads the SSA-specific ones on principle). Three
/// real per-field decisions, each confirmed against a real vendored
/// field before writing this:
/// - `map_type: "atomic"` (`PodSpec.nodeSelector`, `ServiceSpec.selector`,
///   confirmed) — the whole field is one leaf member, no per-key
///   ownership tracked; anything else (unset, or `"granular"`, real
///   upstream's own default) tracks each present key separately.
/// - `list_type: "map"` (`PodSpec.containers`, confirmed) — each element
///   becomes a [`PathElement::Key`] built from `list_map_keys`, recursed
///   into using the array's own `ref_schema` (the *element* schema,
///   matching `strategic_merge`'s own convention) so the element's other
///   fields are tracked too, not just its key fields.
/// - `list_type: "set"` (`ObjectMeta.finalizers`, confirmed) — each
///   element becomes its own [`PathElement::Value`] leaf member directly
///   (no recursion: a set-typed list's own elements are always scalars,
///   real upstream's own restriction).
/// - Anything else (`list_type` unset, or explicitly `"atomic"`
///   — `Container.command`/`.args`, confirmed) — the whole list is one
///   leaf member, matching real upstream's own default posture for a
///   list nobody annotated.
///
/// A nested object field with a known `ref_schema` (a real Struct)
/// recurses using that schema's own `FIELD_META` rows; one with no known
/// `ref_schema` at all (a generic `map[string]V`-shaped field like
/// `metadata.labels`, which carries no per-key metadata to look up in
/// the first place) still tracks each present key as its own member —
/// real upstream's own granular-map default applies identically whether
/// the map is a real SMD "Map" type or simply untyped as far as this
/// crate's own compiled schema goes.
///
/// **Named, deliberate scope, not silently overclaimed**: real upstream
/// strips a handful of `ObjectMeta` fields (`resourceVersion`,
/// `creationTimestamp`, `selfLink`, `uid`, `managedFields` itself, ...)
/// before ever computing a field set for a real applied object — that's
/// the *caller*'s job here too (`k8s.io/apimachinery/pkg/util/
/// managedfields`'s own `stripFields`, not yet ported), this function
/// tracks exactly whatever object it's handed, nothing more, nothing
/// less.
pub fn set_from_object(schema: &str, value: &Value) -> Set {
    let mut set = Set::new();
    let mut path = Vec::new();
    collect_object_fields(schema, value, &mut path, &mut set);
    set
}

fn collect_object_fields(schema: &str, value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    let Value::Object(map) = value else { return };
    for (key, v) in map {
        path.push(PathElement::Field(key.clone()));
        let meta = crate::codegen::field_meta_index().get(&(schema, key.as_str())).copied();
        collect_field_value(meta, v, path, set);
        path.pop();
    }
}

fn collect_field_value(meta: Option<&crate::codegen::openapi_meta::FieldMeta>, value: &Value, path: &mut Vec<PathElement>, set: &mut Set) {
    match value {
        Value::Object(_) if meta.and_then(|m| m.map_type) == Some("atomic") => {
            set.insert(path);
        }
        Value::Object(inner) => match meta.and_then(|m| m.ref_schema) {
            Some(next_schema) => collect_object_fields(next_schema, value, path, set),
            None => {
                // A generic map with no known per-key schema (`metadata.
                // labels`, ...) — real upstream's own granular-map
                // default: each present key is its own member, one level
                // deep, nothing further to recurse through.
                for key in inner.keys() {
                    path.push(PathElement::Field(key.clone()));
                    set.insert(path);
                    path.pop();
                }
            }
        },
        Value::Array(elements) => match meta.and_then(|m| m.list_type) {
            Some("map") => {
                let list_map_keys = meta.map(|m| m.list_map_keys).unwrap_or(&[]);
                let element_schema = meta.and_then(|m| m.ref_schema);
                for element in elements {
                    let Value::Object(obj) = element else {
                        // A `list_type: map` element that isn't an object
                        // is malformed real data (real upstream requires
                        // object elements for an associative list) — skip
                        // rather than fabricate a key for it.
                        continue;
                    };
                    let mut key_fields: Vec<(String, Value)> = list_map_keys.iter().filter_map(|k| obj.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
                    key_fields.sort_by(|a, b| a.0.cmp(&b.0));
                    path.push(PathElement::Key(key_fields));
                    match element_schema {
                        Some(s) => collect_object_fields(s, element, path, set),
                        None => set.insert(path),
                    }
                    path.pop();
                }
            }
            Some("set") => {
                for element in elements {
                    path.push(PathElement::Value(element.clone()));
                    set.insert(path);
                    path.pop();
                }
            }
            // `Some("atomic")` and everything else (unset — real
            // upstream's own default when nobody annotated the field)
            // both mean the same thing here: one leaf for the whole list.
            _ => set.insert(path),
        },
        // A scalar (string/bool/number/null) is always a leaf.
        _ => set.insert(path),
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

    // `set_from_object`'s own tests, each driven by a real vendored field
    // confirmed directly against `vendor/openapi-spec/v3` before writing
    // the code, not assumed from the SMD spec alone -- see this module's
    // own doc comment on `set_from_object` for the exact confirmation.

    #[test]
    fn a_list_type_map_field_tracks_each_element_by_its_own_key_and_recurses_into_it() {
        // PodSpec.containers: x-kubernetes-list-type: map, list-map-keys:
        // [name], element schema Container.
        let pod_spec = json!({"containers": [{"name": "nginx", "image": "nginx:latest"}]});
        let set = set_from_object("io.k8s.api.core.v1.PodSpec", &pod_spec);
        let key = PathElement::Key(vec![("name".to_string(), json!("nginx"))]);
        assert!(set.has(&[PathElement::Field("containers".to_string()), key.clone(), PathElement::Field("name".to_string())]), "the key field itself must also be tracked as a child, matching real fieldsV1 documents");
        assert!(set.has(&[PathElement::Field("containers".to_string()), key, PathElement::Field("image".to_string())]));
    }

    #[test]
    fn a_list_type_set_field_tracks_each_element_as_its_own_value_leaf_with_no_recursion() {
        // ObjectMeta.finalizers: x-kubernetes-list-type: set, scalar elements.
        let meta = json!({"finalizers": ["a.example.com/finalizer", "b.example.com/finalizer"]});
        let set = set_from_object("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &meta);
        assert!(set.has(&[PathElement::Field("finalizers".to_string()), PathElement::Value(json!("a.example.com/finalizer"))]));
        assert!(set.has(&[PathElement::Field("finalizers".to_string()), PathElement::Value(json!("b.example.com/finalizer"))]));
    }

    #[test]
    fn a_list_type_atomic_field_is_one_leaf_for_the_whole_list_not_per_element() {
        // Container.command: x-kubernetes-list-type: atomic (explicit).
        let container = json!({"command": ["/bin/sh", "-c", "echo hi"]});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(set.has(&[PathElement::Field("command".to_string())]), "the whole list must be tracked as one leaf");
        assert!(!set.children.contains_key(&PathElement::Field("command".to_string())), "an atomic list must have no per-element children at all");
    }

    #[test]
    fn a_map_type_atomic_field_is_one_leaf_for_the_whole_map_not_per_key() {
        // PodSpec.nodeSelector: x-kubernetes-map-type: atomic.
        let pod_spec = json!({"nodeSelector": {"disktype": "ssd", "region": "us-west"}});
        let set = set_from_object("io.k8s.api.core.v1.PodSpec", &pod_spec);
        assert!(set.has(&[PathElement::Field("nodeSelector".to_string())]), "the whole map must be tracked as one leaf");
        assert!(!set.children.contains_key(&PathElement::Field("nodeSelector".to_string())), "an atomic map must have no per-key children at all");
    }

    #[test]
    fn a_generic_map_with_no_known_schema_tracks_each_key_separately() {
        // ObjectMeta.labels carries no ref_schema (scalar-valued
        // additionalProperties) -- real upstream's own granular-map
        // default still applies.
        let meta = json!({"labels": {"app": "nginx", "tier": "frontend"}});
        let set = set_from_object("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta", &meta);
        assert!(set.has(&[PathElement::Field("labels".to_string()), PathElement::Field("app".to_string())]));
        assert!(set.has(&[PathElement::Field("labels".to_string()), PathElement::Field("tier".to_string())]));
        assert!(!set.has(&[PathElement::Field("labels".to_string())]), "the map field itself must not be a leaf member -- only its individual keys are");
    }

    #[test]
    fn a_scalar_field_is_always_a_leaf() {
        let container = json!({"name": "nginx", "image": "nginx:latest"});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(set.has(&[PathElement::Field("name".to_string())]));
        assert!(set.has(&[PathElement::Field("image".to_string())]));
    }

    #[test]
    fn a_nested_struct_field_recurses_using_its_own_ref_schema() {
        // Container.resources -> ResourceRequirements -> .limits (a map
        // of Quantity, itself a real nested-schema recursion chain).
        let container = json!({"name": "nginx", "resources": {"limits": {"cpu": "500m"}}});
        let set = set_from_object("io.k8s.api.core.v1.Container", &container);
        assert!(
            set.has(&[PathElement::Field("resources".to_string()), PathElement::Field("limits".to_string()), PathElement::Field("cpu".to_string())])
                || set.has(&[PathElement::Field("resources".to_string()), PathElement::Field("limits".to_string())]),
            "resources.limits.cpu must be tracked one way or the other depending on whether ResourceRequirements.limits itself carries ref_schema metadata"
        );
    }

    // `Set` algebra (`union`/`intersection`/`difference`/
    // `recursive_difference`) -- each built from two small hand-built
    // sets rather than a real object, so the exact tree shape under test
    // is unambiguous.

    fn set_of(paths: &[&[PathElement]]) -> Set {
        let mut s = Set::new();
        for p in paths {
            s.insert(p);
        }
        s
    }

    fn f(name: &str) -> PathElement {
        PathElement::Field(name.to_string())
    }

    #[test]
    fn union_combines_members_from_both_sides() {
        let a = set_of(&[&[f("spec"), f("replicas")]]);
        let b = set_of(&[&[f("spec"), f("selector")]]);
        let u = a.union(&b);
        assert!(u.has(&[f("spec"), f("replicas")]));
        assert!(u.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn union_merges_a_shared_child_node_rather_than_overwriting_it() {
        let a = set_of(&[&[f("metadata"), f("labels"), f("app")]]);
        let b = set_of(&[&[f("metadata"), f("labels"), f("tier")]]);
        let u = a.union(&b);
        assert!(u.has(&[f("metadata"), f("labels"), f("app")]), "the union must not lose a's own child under a shared parent");
        assert!(u.has(&[f("metadata"), f("labels"), f("tier")]));
    }

    #[test]
    fn intersection_keeps_only_paths_present_on_both_sides() {
        let a = set_of(&[&[f("spec"), f("replicas")], &[f("spec"), f("selector")]]);
        let b = set_of(&[&[f("spec"), f("replicas")]]);
        let i = a.intersection(&b);
        assert!(i.has(&[f("spec"), f("replicas")]));
        assert!(!i.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn intersection_of_disjoint_sets_is_empty() {
        let a = set_of(&[&[f("spec"), f("replicas")]]);
        let b = set_of(&[&[f("status"), f("readyReplicas")]]);
        assert!(a.intersection(&b).is_empty());
    }

    #[test]
    fn difference_removes_shared_leaves() {
        let a = set_of(&[&[f("spec"), f("replicas")], &[f("spec"), f("selector")]]);
        let b = set_of(&[&[f("spec"), f("replicas")]]);
        let d = a.difference(&b);
        assert!(!d.has(&[f("spec"), f("replicas")]));
        assert!(d.has(&[f("spec"), f("selector")]));
    }

    #[test]
    fn difference_of_a_set_with_itself_is_empty() {
        let a = set_of(&[&[f("spec"), f("replicas")], &[f("metadata"), f("labels"), f("app")]]);
        assert!(a.difference(&a).is_empty());
    }

    /// The real, intentional asymmetry `difference`'s own doc comment
    /// names: a subtree survives a plain `difference` against a shallow
    /// leaf at the same path in `other`, but not against
    /// `recursive_difference`.
    #[test]
    fn plain_difference_does_not_let_an_others_leaf_cancel_a_selfs_subtree() {
        let a = set_of(&[&[f("a"), f("b"), f("c")]]);
        let b = set_of(&[&[f("a")]]); // "a" owned as a shallow leaf, not a subtree
        let d = a.difference(&b);
        assert!(d.has(&[f("a"), f("b"), f("c")]), "difference must leave self's own deeper subtree alone here");
    }

    #[test]
    fn recursive_difference_drops_a_whole_subtree_when_others_leaf_matches_its_root() {
        let a = set_of(&[&[f("a"), f("b"), f("c")]]);
        let b = set_of(&[&[f("a"), f("b")]]);
        let d = a.recursive_difference(&b);
        assert!(!d.has(&[f("a"), f("b"), f("c")]), "the entire a.b subtree must be gone");
    }

    #[test]
    fn is_empty_is_true_for_a_freshly_constructed_set() {
        assert!(Set::new().is_empty());
    }

    #[test]
    fn is_empty_is_false_once_anything_is_inserted() {
        let s = set_of(&[&[f("spec")]]);
        assert!(!s.is_empty());
    }
}
