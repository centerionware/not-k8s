//! The real `metadata.managedFields[]` wire shape (`io.k8s.apimachinery.
//! pkg.apis.meta.v1.ManagedFieldsEntry`, confirmed directly against the
//! vendored OpenAPI spec) and the two conversions `updater::apply`/
//! `updater::update`/`apply_update` actually need: the stored array in,
//! a `BTreeMap<String, Set>` out (what those functions operate on); a
//! reconciled `BTreeMap<String, Set>` plus the applying manager's own new
//! entry in, the array to write back to storage out.
//!
//! `fieldsV1`'s own JSON shape is exactly `fieldset::Set::to_json()`/
//! `from_json()`'s shape (confirmed directly: the vendored `FieldsV1`
//! schema's own description names the same `f:`/`k:`/`v:`/`i:`/`"."`
//! grammar `fieldset`'s own doc comment already ports) — no separate
//! parser needed here.
//!
//! Not yet wired into `server::rest` — this module is the storage-shape
//! primitive; the request-handling glue (parsing `metadata.managedFields`
//! off a stored object, calling `updater::apply`/`update`, writing the
//! rebuilt array back, `application/apply-patch+yaml` content-type
//! routing) is separate, not-yet-started work.

use super::fieldset::{DeserializeError, Set};
use std::collections::BTreeMap;
use serde_json::{Map, Value};

/// One real `ManagedFieldsEntry`. `time` is kept as whatever RFC3339
/// string was already stored (or `None` for a brand-new entry a caller
/// hasn't stamped yet) — this module reads no clock itself, matching
/// `nodestore`'s own determinism discipline (`command.rs`'s own rule,
/// carried here as a matter of good practice even though this crate has
/// no raft log of its own): a wall-clock read belongs at the call site
/// that actually knows "now", not buried in a reusable primitive.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedFieldsEntry {
    pub manager: String,
    /// Real upstream's only two valid values: `"Apply"` or `"Update"`.
    pub operation: String,
    pub api_version: String,
    pub time: Option<String>,
    pub fields: Set,
    /// Empty string (not `None`) for the main resource — matches real
    /// upstream's own `Subresource string` (never optional on the wire).
    pub subresource: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("metadata.managedFields must be an array")]
    NotAnArray,
    #[error("a managedFields entry must be a JSON object")]
    EntryNotAnObject,
    #[error("a managedFields entry's fieldsV1 could not be parsed: {0}")]
    Fields(#[from] DeserializeError),
}

/// Real upstream's own decode of `metadata.managedFields` — entries whose
/// `fieldsType` isn't `"FieldsV1"` are skipped rather than rejected
/// (real upstream's own forward-compatibility posture: `fieldsType` is
/// explicitly documented as a discriminator for a format that could grow
/// new variants; a genuinely unrecognized one is data this crate simply
/// can't interpret yet, not malformed data worth failing the whole
/// decode over).
pub fn parse_managed_fields(value: &Value) -> Result<Vec<ManagedFieldsEntry>, Error> {
    let Value::Array(entries) = value else {
        return Err(Error::NotAnArray);
    };
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Object(obj) = entry else {
            return Err(Error::EntryNotAnObject);
        };
        let fields_type = obj.get("fieldsType").and_then(Value::as_str).unwrap_or("");
        if fields_type != "FieldsV1" {
            continue;
        }
        let manager = obj.get("manager").and_then(Value::as_str).unwrap_or("").to_string();
        let operation = obj.get("operation").and_then(Value::as_str).unwrap_or("").to_string();
        let api_version = obj.get("apiVersion").and_then(Value::as_str).unwrap_or("").to_string();
        let time = obj.get("time").and_then(Value::as_str).map(str::to_string);
        let subresource = obj.get("subresource").and_then(Value::as_str).unwrap_or("").to_string();
        let fields = match obj.get("fieldsV1") {
            Some(f) => Set::from_json(f)?,
            None => Set::new(),
        };
        result.push(ManagedFieldsEntry { manager, operation, api_version, time, fields, subresource });
    }
    Ok(result)
}

/// The inverse of [`parse_managed_fields`] — real upstream's own array
/// shape, one object per entry, `fieldsType` always `"FieldsV1"` (the
/// only value this crate ever writes, matching every real entry it could
/// have read in the first place).
pub fn render_managed_fields(entries: &[ManagedFieldsEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|e| {
                let mut obj = Map::new();
                obj.insert("manager".to_string(), Value::String(e.manager.clone()));
                obj.insert("operation".to_string(), Value::String(e.operation.clone()));
                obj.insert("apiVersion".to_string(), Value::String(e.api_version.clone()));
                if let Some(t) = &e.time {
                    obj.insert("time".to_string(), Value::String(t.clone()));
                }
                obj.insert("fieldsType".to_string(), Value::String("FieldsV1".to_string()));
                obj.insert("fieldsV1".to_string(), e.fields.to_json());
                obj.insert("subresource".to_string(), Value::String(e.subresource.clone()));
                Value::Object(obj)
            })
            .collect(),
    )
}

/// Reduces the real entry list down to exactly what `updater::update`/
/// `apply_update`/`apply` operate on — a manager's *own* `Set`, keyed by
/// name. A manager with entries under more than one `subresource` (a
/// separate `status` update, say) has each kept distinct by real
/// upstream's own documented rule ("the value of this field is used to
/// distinguish between managers, even if they share the same name") —
/// modeled here by folding `subresource` into the map key whenever it's
/// non-empty, so `"kubectl-edit"` (main resource) and
/// `"kubectl-edit/status"` never collide.
pub fn to_managers_map(entries: &[ManagedFieldsEntry]) -> BTreeMap<String, Set> {
    let mut map = BTreeMap::new();
    for e in entries {
        let key = manager_key(&e.manager, &e.subresource);
        map.insert(key, e.fields.clone());
    }
    map
}

fn manager_key(manager: &str, subresource: &str) -> String {
    if subresource.is_empty() {
        manager.to_string()
    } else {
        format!("{manager}/{subresource}")
    }
}

/// Rebuilds the real entry list after a successful `updater::apply`/
/// `update`/`apply_update` call: every manager in `managers` (the
/// reconciled map those functions returned) becomes one entry, reusing
/// the previous entry's own `time`/`apiVersion`/`operation`/`subresource`
/// when one already existed for that manager (a manager `update()`/
/// `prune()` only trimmed, not the one that just wrote, keeps its own
/// prior bookkeeping — matches real upstream's own doc comment: "The
/// timestamp does not update when a field is removed from the entry
/// because another manager took it over"), and stamping the applying
/// manager's own entry fresh with `operation`/`api_version`/`time`
/// supplied by the caller (the one piece of "now" this module doesn't
/// invent itself — see this module's own top doc comment).
pub fn rebuild_managed_fields(
    previous: &[ManagedFieldsEntry],
    managers: &BTreeMap<String, Set>,
    applying_manager: &str,
    applying_subresource: &str,
    operation: &str,
    api_version: &str,
    time: Option<&str>,
) -> Vec<ManagedFieldsEntry> {
    let mut previous_by_key: BTreeMap<String, &ManagedFieldsEntry> = BTreeMap::new();
    for e in previous {
        previous_by_key.insert(manager_key(&e.manager, &e.subresource), e);
    }
    let applying_key = manager_key(applying_manager, applying_subresource);

    let mut result = Vec::with_capacity(managers.len());
    for (key, fields) in managers {
        if *key == applying_key {
            result.push(ManagedFieldsEntry {
                manager: applying_manager.to_string(),
                operation: operation.to_string(),
                api_version: api_version.to_string(),
                time: time.map(str::to_string),
                fields: fields.clone(),
                subresource: applying_subresource.to_string(),
            });
            continue;
        }
        match previous_by_key.get(key) {
            Some(prior) => result.push(ManagedFieldsEntry {
                manager: prior.manager.clone(),
                operation: prior.operation.clone(),
                api_version: prior.api_version.clone(),
                time: prior.time.clone(),
                fields: fields.clone(),
                subresource: prior.subresource.clone(),
            }),
            // A manager `managers` names but `previous` never recorded an
            // entry for at all is genuinely unreachable through this
            // module's own two entry points (`to_managers_map` only ever
            // produces keys `previous` already has) -- defensively kept
            // rather than panicking, using the key itself as the best
            // available manager name.
            None => result.push(ManagedFieldsEntry {
                manager: key.clone(),
                operation: operation.to_string(),
                api_version: api_version.to_string(),
                time: time.map(str::to_string),
                fields: fields.clone(),
                subresource: String::new(),
            }),
        }
    }
    result.sort_by(|a, b| (a.manager.as_str(), a.subresource.as_str()).cmp(&(b.manager.as_str(), b.subresource.as_str())));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::fieldset::PathElement;
    use serde_json::json;

    fn path(fields: &[&str]) -> Vec<PathElement> {
        fields.iter().map(|f| PathElement::Field(f.to_string())).collect()
    }

    fn entry(manager: &str, fields: &[&[&str]]) -> ManagedFieldsEntry {
        let mut set = Set::new();
        for p in fields {
            set.insert(&path(p));
        }
        ManagedFieldsEntry {
            manager: manager.to_string(),
            operation: "Apply".to_string(),
            api_version: "apps/v1".to_string(),
            time: Some("2026-08-21T00:00:00Z".to_string()),
            fields: set,
            subresource: String::new(),
        }
    }

    #[test]
    fn parse_reads_a_real_wire_shaped_entry() {
        let doc = json!([{
            "manager": "kubectl-apply",
            "operation": "Apply",
            "apiVersion": "apps/v1",
            "time": "2026-08-21T00:00:00Z",
            "fieldsType": "FieldsV1",
            "fieldsV1": {"f:replicas": {}},
            "subresource": "",
        }]);
        let entries = parse_managed_fields(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].manager, "kubectl-apply");
        assert_eq!(entries[0].operation, "Apply");
        assert!(entries[0].fields.has(&path(&["replicas"])));
    }

    #[test]
    fn parse_skips_an_entry_with_an_unrecognized_fields_type() {
        let doc = json!([{
            "manager": "some-future-client",
            "operation": "Apply",
            "apiVersion": "apps/v1",
            "fieldsType": "FieldsV2",
            "fieldsV1": {},
            "subresource": "",
        }]);
        let entries = parse_managed_fields(&doc).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_rejects_a_non_array_document() {
        assert!(matches!(parse_managed_fields(&json!({})), Err(Error::NotAnArray)));
    }

    #[test]
    fn render_and_parse_round_trip() {
        let entries = vec![entry("kubectl-apply", &[&["replicas"]])];
        let rendered = render_managed_fields(&entries);
        let parsed = parse_managed_fields(&rendered).unwrap();
        assert_eq!(parsed, entries);
    }

    #[test]
    fn to_managers_map_reduces_to_manager_and_fields_only() {
        let entries = vec![
            entry("kubectl-apply", &[&["replicas"]]),
            entry("hpa-controller", &[&["minReadySeconds"]]),
        ];
        let map = to_managers_map(&entries);
        assert_eq!(map.len(), 2);
        assert!(map["kubectl-apply"].has(&path(&["replicas"])));
        assert!(map["hpa-controller"].has(&path(&["minReadySeconds"])));
    }

    #[test]
    fn to_managers_map_keeps_a_subresource_manager_distinct_from_the_main_one() {
        let mut main = entry("kubectl-edit", &[&["replicas"]]);
        let mut status = entry("kubectl-edit", &[&["status"]]);
        status.subresource = "status".to_string();
        main.subresource = String::new();
        let map = to_managers_map(&[main, status]);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("kubectl-edit"));
        assert!(map.contains_key("kubectl-edit/status"));
    }

    #[test]
    fn rebuild_preserves_a_trimmed_managers_own_prior_bookkeeping() {
        // hpa-controller's set got trimmed by a prune/conflict elsewhere,
        // but it's still present -- its own time/operation/apiVersion
        // must survive untouched, only its fields update.
        let previous = vec![entry("hpa-controller", &[&["replicas"], &["minReadySeconds"]])];
        let mut trimmed = Set::new();
        trimmed.insert(&path(&["minReadySeconds"]));
        let managers = BTreeMap::from([("hpa-controller".to_string(), trimmed)]);

        let rebuilt = rebuild_managed_fields(&previous, &managers, "kubectl-apply", "", "Apply", "apps/v1", Some("2026-08-21T01:00:00Z"));
        let hpa = rebuilt.iter().find(|e| e.manager == "hpa-controller").unwrap();
        assert_eq!(hpa.time, previous[0].time, "an unrelated manager's own bookkeeping must not be touched");
        assert!(!hpa.fields.has(&path(&["replicas"])), "the trimmed field is gone");
        assert!(hpa.fields.has(&path(&["minReadySeconds"])));
    }

    #[test]
    fn rebuild_stamps_the_applying_managers_own_fresh_entry() {
        let previous = vec![];
        let mut fields = Set::new();
        fields.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("kubectl-apply".to_string(), fields)]);

        let rebuilt = rebuild_managed_fields(&previous, &managers, "kubectl-apply", "", "Apply", "apps/v1", Some("2026-08-21T00:00:00Z"));
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].manager, "kubectl-apply");
        assert_eq!(rebuilt[0].operation, "Apply");
        assert_eq!(rebuilt[0].time.as_deref(), Some("2026-08-21T00:00:00Z"));
    }

    #[test]
    fn rebuild_drops_a_manager_whose_set_became_empty() {
        // updater::apply/update already drop an empty-set manager from
        // the map they return -- rebuild must not resurrect it.
        let previous = vec![entry("hpa-controller", &[&["replicas"]])];
        let managers: BTreeMap<String, Set> = BTreeMap::new();
        let rebuilt = rebuild_managed_fields(&previous, &managers, "kubectl-apply", "", "Apply", "apps/v1", None);
        assert!(rebuilt.is_empty());
    }
}
