//! Sets a `CustomResourceDefinition`'s own `status` on write.
//!
//! **Named, deliberate simplification against real upstream**: real
//! `kube-apiserver` computes `NamesAccepted`/`Established` from a
//! separate, asynchronous in-process controller
//! (`pkg/controller/establish`) that only flips `Established` true once
//! it has confirmed the CRD's storage/discovery were actually installed
//! successfully, and `NamesAccepted` is computed by walking every *other*
//! `Established` CRD in the same group looking for a name conflict
//! (`pkg/apiserver/validation`). This build has no separate
//! controller-manager loop that could own that async reconciliation
//! in-process (`nodecontroller` is a genuinely different component, and
//! real upstream's own controller lives inside kube-apiserver for
//! exactly the reason this one would have to as well — it needs
//! synchronous access to the in-process REST storage it's deciding
//! whether to trust), so both conditions are computed synchronously,
//! right on `CREATE`/`UPDATE` of the CRD object itself
//! (`server::rest::create`/`persist_update`'s own CRD special case) —
//! matching the user's own framing of this build's real job here:
//! *"it's up to operators to WATCH/LIST CRDs and react to them,
//! apiserver just has to track them."* A CRD this build accepts is
//! marked `Established`/`NamesAccepted` immediately; the naming-conflict
//! check below is a real, synchronous one (no controller needed to make
//! it correct), it just isn't re-checked against a CRD that's deleted or
//! renamed later without another write to trigger recomputation.

use serde_json::{json, Value};

/// Real upstream's own naming-conflict rule
/// (`pkg/apiserver/validation/validation.go`'s `validateCustomResource
/// DefinitionSpec` conflict half — fetched and read directly): within one
/// `spec.group`, no two CRDs may share a `plural`, `singular`, `kind`,
/// `listKind`, or any `shortName` — checked here against every *other*
/// already-`Established` CRD (a CRD in the middle of first being created
/// has no established rivals of its own to conflict with).
fn names_conflict(candidate: &Value, other: &Value) -> bool {
    if other.pointer("/spec/group").and_then(Value::as_str) != candidate.pointer("/spec/group").and_then(Value::as_str) {
        return false;
    }
    let candidate_names = candidate.pointer("/spec/names");
    let other_names = other.pointer("/spec/names");
    let (Some(a), Some(b)) = (candidate_names, other_names) else { return false };

    let scalar_conflict = ["plural", "singular", "kind", "listKind"]
        .iter()
        .any(|field| matches!((a.get(field).and_then(Value::as_str), b.get(field).and_then(Value::as_str)), (Some(x), Some(y)) if x == y));
    if scalar_conflict {
        return true;
    }
    let a_short: Vec<&str> = a.get("shortNames").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let b_short: Vec<&str> = b.get("shortNames").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    a_short.iter().any(|x| b_short.contains(x))
}

/// True when `candidate`'s own names don't collide with any other
/// already-`Established` CRD in `others` — see [`names_conflict`]'s own
/// doc comment for the exact rule. `others` should already exclude
/// `candidate` itself (its own prior stored revision, on an `UPDATE`) —
/// callers get this for free since `others` comes from a fresh `LIST`
/// taken before this write lands.
pub fn names_accepted<'a>(candidate: &Value, others: impl IntoIterator<Item = &'a Value>) -> bool {
    !others.into_iter().any(|other| names_conflict(candidate, other))
}

/// The CRD's own storage version — the one `spec.versions[]` entry with
/// `storage: true`. Real upstream's own CRD validation requires exactly
/// one; this build trusts that invariant rather than re-deriving it
/// (defensive `None` for a malformed document, not a real case a
/// validated CRD produces).
fn storage_version_name(crd: &Value) -> Option<String> {
    crd.pointer("/spec/versions")?
        .as_array()?
        .iter()
        .find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
        .and_then(|v| v.get("name").and_then(Value::as_str))
        .map(str::to_string)
}

/// Computes the full `status` object to persist for a
/// `CustomResourceDefinition` write — `server::rest`'s own CRD special
/// case calls this instead of running the generic write path's ordinary
/// defaulting for `status` (a CRD's `status` is server-computed, never
/// client-settable, the same "generic status subresource" posture
/// `server::rest::update_status` already establishes for every other
/// resource).
///
/// `existing_stored_versions` carries forward real upstream's own
/// monotonic-union rule for `status.storedVersions` (never silently
/// drops a version a prior revision already recorded — that list is what
/// a real storage-migration tool reads to know what to migrate) even
/// though this build has no migration tooling of its own yet to read it.
pub fn compute_status<'a>(candidate: &Value, other_crds: impl IntoIterator<Item = &'a Value>, existing_stored_versions: &[String], now: &str) -> Value {
    let accepted = names_accepted(candidate, other_crds);
    let names = candidate.pointer("/spec/names").cloned().unwrap_or_else(|| json!({}));

    let mut stored_versions: Vec<String> = existing_stored_versions.to_vec();
    if let Some(v) = storage_version_name(candidate) {
        if !stored_versions.contains(&v) {
            stored_versions.push(v);
        }
    }

    let (names_status, names_reason, names_message) = if accepted {
        ("True", "NoConflicts", "no conflicts found")
    } else {
        ("False", "NamesConflict", "names conflict with an existing established CustomResourceDefinition in the same group")
    };
    // Real upstream only ever marks `Established` once `NamesAccepted`
    // has been observed true (`pkg/controller/establish`'s own
    // `crdEstablishingController`) — a name conflict means neither
    // condition is true, not just the naming one.
    let (established_status, established_reason, established_message) =
        if accepted { ("True", "InitialNamesAccepted", "the initial names have been accepted") } else { ("False", "NotAccepted", "not all names are accepted") };

    json!({
        "acceptedNames": if accepted { names } else { json!({}) },
        "storedVersions": stored_versions,
        "conditions": [
            {"type": "NamesAccepted", "status": names_status, "reason": names_reason, "message": names_message, "lastTransitionTime": now},
            {"type": "Established", "status": established_status, "reason": established_reason, "message": established_message, "lastTransitionTime": now},
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crd(group: &str, plural: &str, kind: &str) -> Value {
        json!({
            "spec": {
                "group": group,
                "names": {"plural": plural, "kind": kind, "listKind": format!("{kind}List")},
                "versions": [{"name": "v1", "served": true, "storage": true}],
            },
        })
    }

    #[test]
    fn a_crd_with_no_rivals_is_accepted_and_established() {
        let candidate = crd("example.com", "widgets", "Widget");
        let status = compute_status(&candidate, std::iter::empty(), &[], "2026-08-21T00:00:00Z");
        assert_eq!(status["conditions"][0]["status"], "True");
        assert_eq!(status["conditions"][1]["status"], "True");
        assert_eq!(status["storedVersions"], json!(["v1"]));
    }

    #[test]
    fn a_plural_collision_in_the_same_group_is_rejected() {
        let candidate = crd("example.com", "widgets", "Widget");
        let other = crd("example.com", "widgets", "OtherWidget");
        let status = compute_status(&candidate, [&other], &[], "2026-08-21T00:00:00Z");
        assert_eq!(status["conditions"][0]["status"], "False");
        assert_eq!(status["conditions"][1]["status"], "False");
    }

    #[test]
    fn the_same_plural_in_a_different_group_does_not_conflict() {
        let candidate = crd("example.com", "widgets", "Widget");
        let other = crd("other.com", "widgets", "Widget");
        assert!(names_accepted(&candidate, [&other]));
    }

    #[test]
    fn a_short_name_collision_is_rejected() {
        let mut candidate = crd("example.com", "widgets", "Widget");
        candidate["spec"]["names"]["shortNames"] = json!(["wd"]);
        let mut other = crd("example.com", "gizmos", "Gizmo");
        other["spec"]["names"]["shortNames"] = json!(["wd"]);
        assert!(!names_accepted(&candidate, [&other]));
    }

    #[test]
    fn stored_versions_accumulate_rather_than_being_overwritten() {
        let candidate = crd("example.com", "widgets", "Widget");
        let status = compute_status(&candidate, std::iter::empty(), &["v1beta1".to_string()], "2026-08-21T00:00:00Z");
        assert_eq!(status["storedVersions"], json!(["v1beta1", "v1"]));
    }

    #[test]
    fn an_already_recorded_stored_version_is_not_duplicated() {
        let candidate = crd("example.com", "widgets", "Widget");
        let status = compute_status(&candidate, std::iter::empty(), &["v1".to_string()], "2026-08-21T00:00:00Z");
        assert_eq!(status["storedVersions"], json!(["v1"]));
    }
}
