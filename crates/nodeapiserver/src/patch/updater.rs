//! Server-Side Apply's `merge.Updater` orchestration — ported from real
//! upstream `sigs.k8s.io/structured-merge-diff/v6/merge.Updater`'s own
//! `update`/`Update` (`merge/update.go`, fetched and read directly, not
//! reconstructed from memory). This is the piece `docs/APISERVER.md` and
//! this crate's `patch` module doc have named as the closing orchestration
//! all three of Group G's real SSA prerequisites (`fieldset::set_from_object`,
//! `typed_merge::merge`, `typed_compare::compare`) exist to feed.
//!
//! # Named, deliberate scope for this slice
//!
//! Real upstream's `update()` caches one `Comparison` **per manager's own
//! recorded API version** (`versions map[fieldpath.APIVersion]*typed.
//! Comparison`), since two managers can have last written the object at
//! different served versions and a `Converter` re-converts old/new into each
//! one before comparing. This build has **no multi-version conversion
//! machinery at all** — confirmed: no CRD conversion webhooks, no
//! `Converter` equivalent anywhere in the crate, `nodeapiserver` serves
//! exactly one storage schema per resource — so every manager's `Set` here
//! is assumed to already be expressed against the one `schema` being
//! compared, collapsing upstream's per-version `Comparison` cache down to
//! one shared `Comparison` computed once. If multi-version storage is ever
//! added, this is where the per-version cache would need to come back.
//!
//! Landed here: `update()` (the shared conflict-detection/bookkeeping core
//! both `Update` and `Apply` build on), `apply_update()` (real upstream's
//! `Updater.Update` — the PATCH/PUT path, which always calls `update()` with
//! `force: true`, since only `Apply` itself ever rejects on conflict), and
//! `prune()` (real upstream's `prune`/`addBackOwnedItems`/
//! `addBackDanglingItems`, using `fieldset::remove_items`/
//! `ensure_named_fields_are_members`) — the step `Apply` runs on the
//! just-merged object to drop whatever the applying manager owned last
//! time but its new config no longer mentions, while adding back anything
//! any manager (including the applier's own new config) still claims.
//!
//! `apply()` now also landed — real `Updater.Apply` itself, the piece
//! this whole arc has been building toward: merges the incoming apply
//! configuration into the live object (`typed_merge::merge`), records the
//! applying manager's own new field set, prunes whatever it stopped
//! claiming, then runs `update()` for real conflict detection against
//! every other manager.
//!
//! **Not yet landed**: the real `managedFields` wire format
//! (`ManagedFieldsEntry`, `metadata.managedFields[]` — this module works
//! entirely in terms of `BTreeMap<String, Set>`, not that wire shape) and
//! `server::rest`/`application/apply-patch+yaml` wiring to actually reach
//! `apply()` from a real request — both real, separate, not-yet-started
//! work. Also not ported: upstream's own `IgnoreFilter`/`IgnoredFields`
//! (server-managed field exclusion, e.g. `status`) and
//! `reconcileManagedFieldsWithSchemaChanges` (schema atomic<->granular
//! migration bookkeeping) — both real, both named as separate
//! not-yet-started work rather than silently dropped. `apply()`'s own doc
//! comment also names one further simplification specific to it: real
//! upstream's `VersionedSet` carries an `Applied` bool per manager this
//! crate's plain `Set`-keyed map has no room for.

use super::fieldset::{ensure_named_fields_are_members, remove_items, set_from_object, Set};
use super::typed_compare::{compare, Comparison};
use super::typed_merge::merge as typed_merge;
use serde_json::Value;
use std::collections::BTreeMap;

/// One other manager's ownership conflicting with what this write would
/// change — real upstream's own `managers[manager] = fieldpath.
/// NewVersionedSet(conflictSet, ...)` entry, surfaced as the caller's error
/// (a real `409 Conflict` at the HTTP layer, not built here).
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub manager: String,
    pub fields: Set,
}

/// Real upstream's `(*Updater).update`, single-schema-version scoped (see
/// module doc). `managers` is every *other* manager's currently-owned
/// `Set`, keyed by manager name — the applying manager must **not** be a key
/// of this map (matches real upstream's own `if manager == workflow {
/// continue }` skip; callers exclude it up front rather than this function
/// filtering it out, since `apply_update` below needs the excluded entry
/// back for its own bookkeeping).
///
/// On success, returns the reconciled map — every other manager's `Set`
/// with the fields this write just changed (real upstream: `compare.
/// Modified.Union(compare.Added)`) taken away from whichever manager used to
/// own them, and any field this write *removed* taken away from every other
/// manager too — plus the `old.Compare(new)` result the caller needs for its
/// own bookkeeping. Manager entries left empty by this are dropped, exactly
/// like real upstream's own trailing cleanup loop.
///
/// On conflict (`force == false` and at least one other manager currently
/// owns a field this write is changing), returns every conflicting manager
/// instead — real upstream's own `ConflictsFromManagers` error.
pub fn update(
    schema: &str,
    old: &Value,
    new: &Value,
    managers: &BTreeMap<String, Set>,
    force: bool,
) -> Result<(BTreeMap<String, Set>, Comparison), Vec<Conflict>> {
    let cmp = compare(schema, old, new);
    let changed = cmp.modified.union(&cmp.added);

    let mut conflicts = Vec::new();
    for (manager, manager_set) in managers {
        let conflict_set = manager_set.intersection(&changed);
        if !conflict_set.is_empty() {
            conflicts.push(Conflict {
                manager: manager.clone(),
                fields: conflict_set,
            });
        }
    }

    if !force && !conflicts.is_empty() {
        return Err(conflicts);
    }

    let mut result = managers.clone();
    for conflict in &conflicts {
        if let Some(set) = result.get(&conflict.manager) {
            result.insert(conflict.manager.clone(), set.difference(&conflict.fields));
        }
    }
    if !cmp.removed.is_empty() {
        for set in result.values_mut() {
            *set = set.difference(&cmp.removed);
        }
    }
    result.retain(|_, set| !set.is_empty());

    Ok((result, cmp))
}

/// Real upstream's `Updater.Update` — the PATCH/PUT write path (not
/// `Apply`). `live` is the object before this write (an empty object for a
/// CREATE, matching upstream's own doc comment: "liveObject must be the
/// original object (empty if this is a CREATE call)"), `new` is the object
/// as it is about to be persisted. `managers` is every manager's *current*
/// `Set` including `manager`'s own prior entry, if any.
///
/// Always calls `update()` with `force: true` — real upstream's own
/// hardcoded `s.update(..., manager, true)` inside `Update`: an ordinary
/// write never rejects on conflict, only `Apply` does — so this never
/// itself returns a conflict.
///
/// Returns the full reconciled manager map, including `manager`'s own new
/// entry: real upstream's `managers[manager].Set().Difference(compare.
/// Removed).Union(compare.Modified).Union(compare.Added)` — the applying
/// manager keeps every field it already owned that this write didn't touch
/// or remove, plus every field this write actually changed or added (not
/// "every field this write's body mentions" — a PUT/PATCH re-sending an
/// unchanged value doesn't newly claim it away from whoever already owned
/// it, since an unchanged field isn't in `compare.Modified`/`Added` at all).
pub fn apply_update(
    schema: &str,
    live: &Value,
    new: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
) -> BTreeMap<String, Set> {
    let others: BTreeMap<String, Set> = managers
        .iter()
        .filter(|(m, _)| m.as_str() != manager)
        .map(|(m, s)| (m.clone(), s.clone()))
        .collect();

    let (mut result, cmp) = update(schema, live, new, &others, true)
        .expect("force: true never returns Err — see update()'s own doc comment");

    let existing = managers.get(manager).cloned().unwrap_or_default();
    let set = existing
        .difference(&cmp.removed)
        .union(&cmp.modified)
        .union(&cmp.added);

    if set.is_empty() {
        result.remove(manager);
    } else {
        result.insert(manager.to_string(), set);
    }
    result
}

/// Real upstream's `(*Updater).prune` (`merge/update.go`, fetched and read
/// directly), single-schema-version scoped like `update`/`apply_update`
/// above (no `Converter` — every conversion step real upstream's own
/// `prune`/`addBackOwnedItems`/`addBackDanglingItems` perform collapses to
/// the identity here). Called by `Updater.Apply` **before** `update()`, on
/// the just-merged object, to drop whatever the applying manager owned
/// last time (`last_set`) but the new apply configuration no longer
/// mentions at all — real Server-Side Apply's own "fields you stop
/// applying get removed" contract, not merely "fields you stop applying
/// stop being *owned*" (`update()` alone only ever adjusts ownership
/// bookkeeping, never removes a value from the object).
///
/// `managers` must already include the applying manager's own **new**
/// `Set` (the field set of the incoming apply configuration) — matches
/// real upstream's own call order inside `Apply`, which stores
/// `managers[manager] = ...` before calling `prune`.
///
/// No pruning at all on a first-ever apply (`last_set` is `None` or
/// empty) — matches real upstream's own `if lastSet == nil ||
/// lastSet.Set().Empty() { return merged, nil }` short-circuit.
pub fn prune(schema: &str, merged: &Value, managers: &BTreeMap<String, Set>, last_set: Option<&Set>) -> Value {
    let Some(last_set) = last_set else {
        return merged.clone();
    };
    if last_set.is_empty() {
        return merged.clone();
    }

    let named_last = ensure_named_fields_are_members(schema, last_set);
    let pruned = remove_items(schema, merged, &named_last);
    let pruned = add_back_owned_items(schema, merged, &pruned, managers);
    add_back_dangling_items(schema, merged, &pruned, last_set)
}

/// Real upstream's `addBackOwnedItems`/`addBackOwnedItemsForVersion`: adds
/// back any field `remove_items` above just dropped that some manager
/// (including the applying manager's own new configuration, already
/// present in `managers` — see `prune`'s own doc comment) still claims to
/// own. Recomputed from scratch against the original `merged`, not
/// incrementally against `pruned` — matches real upstream's own
/// `merged.RemoveItems(mergedSet.Difference(prunedSet.Union(managed)))`.
fn add_back_owned_items(schema: &str, merged: &Value, pruned: &Value, managers: &BTreeMap<String, Set>) -> Value {
    let merged_named = ensure_named_fields_are_members(schema, &set_from_object(schema, merged));
    let pruned_named = ensure_named_fields_are_members(schema, &set_from_object(schema, pruned));
    let mut managed = Set::new();
    for set in managers.values() {
        managed = managed.union(set);
    }
    let managed_named = ensure_named_fields_are_members(schema, &managed);
    let to_remove = merged_named.difference(&pruned_named.union(&managed_named));
    remove_items(schema, merged, &to_remove)
}

/// Real upstream's `addBackDanglingItems`: a defensive final pass, only
/// ever able to matter when something upstream of this crate's own
/// single-version scope diverges the pipeline's intermediate steps (real
/// upstream's own comment: fields "unowned or ... owned by Updaters" that
/// `prune`'s earlier steps shouldn't have dropped) — re-adds anything
/// still missing from `pruned` relative to `merged` that isn't actually
/// part of what the applying manager previously owned (`last_set`).
fn add_back_dangling_items(schema: &str, merged: &Value, pruned: &Value, last_set: &Set) -> Value {
    let merged_named = ensure_named_fields_are_members(schema, &set_from_object(schema, merged));
    let pruned_named = ensure_named_fields_are_members(schema, &set_from_object(schema, pruned));
    let last_named = ensure_named_fields_are_members(schema, last_set);
    let to_remove = merged_named.difference(&pruned_named).intersection(&last_named);
    remove_items(schema, merged, &to_remove)
}

/// The result of a successful [`apply`] — real upstream's own three-tuple
/// return (`*typed.TypedValue, fieldpath.ManagedFields, error`), minus the
/// error (a rejected apply is `Err` instead, see [`apply`]'s own doc
/// comment).
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The object to persist — `None` when the apply was a genuine no-op
    /// (the merged-and-pruned result is byte-for-byte identical to the
    /// live object), matching real upstream's own `returnInputOnNoop`
    /// short-circuit (this crate never opts into keeping the input on a
    /// no-op, so this is always upstream's default behavior, not a
    /// configurable flag).
    pub object: Option<Value>,
    /// Every manager's reconciled `Set`, including the applying manager's
    /// own new one.
    pub managers: BTreeMap<String, Set>,
}

/// Real upstream's `Updater.Apply` (`merge/update.go`, fetched and read
/// directly) — the piece this whole arc has been building toward.
/// Single-schema-version scoped like every other function in this module
/// (see the module doc's own note); `managers` is every manager's
/// *current* `Set`, including `manager`'s own prior entry if it has one
/// (real upstream's own `lastSet := managers[manager]`, read **before**
/// it gets overwritten).
///
/// 1. Merges `config` into `live` (`typed_merge::merge` — real upstream's
///    `liveObject.Merge(configObject)`).
/// 2. Records the applying manager's own new field set as exactly what
///    `config` itself sets (`fieldset::set_from_object` — real upstream's
///    `configObject.ToFieldSet()`), replacing whatever it owned before.
/// 3. Prunes the merged object (`prune()` above) — whatever the applying
///    manager owned last time but `config` no longer mentions, and nobody
///    else claims either, is dropped from the object entirely.
/// 4. Runs `update()` against every *other* manager for real conflict
///    detection: a field this apply actually changed or added that
///    another manager currently owns is a conflict, rejected with
///    `Err(Vec<Conflict>)` unless `force`.
///
/// On success, [`Applied::object`] is `None` exactly when the final
/// result is identical to `live` (a genuine no-op re-apply) — the caller
/// should treat that the same as upstream's own callers do: nothing to
/// write back to storage.
pub fn apply(
    schema: &str,
    live: &Value,
    config: &Value,
    managers: &BTreeMap<String, Set>,
    manager: &str,
    force: bool,
) -> Result<Applied, Vec<Conflict>> {
    let merged = typed_merge(schema, live, config);

    let last_set = managers.get(manager).cloned();
    let new_set = set_from_object(schema, config);
    let mut managers = managers.clone();
    managers.insert(manager.to_string(), new_set);

    let pruned = prune(schema, &merged, &managers, last_set.as_ref());

    let others: BTreeMap<String, Set> = managers
        .iter()
        .filter(|(m, _)| m.as_str() != manager)
        .map(|(m, s)| (m.clone(), s.clone()))
        .collect();
    let (mut result, _cmp) = update(schema, live, &pruned, &others, force)?;
    // `update()`'s own contract (see its doc comment) never touches a
    // manager excluded from the map handed to it -- add the applying
    // manager's own entry (set above, step 2) back in.
    if let Some(set) = managers.get(manager) {
        result.insert(manager.to_string(), set.clone());
    }

    let object = if &pruned == live { None } else { Some(pruned) };
    Ok(Applied { object, managers: result })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::fieldset::PathElement;
    use serde_json::json;

    fn path(fields: &[&str]) -> Vec<PathElement> {
        fields
            .iter()
            .map(|f| PathElement::Field(f.to_string()))
            .collect()
    }

    #[test]
    fn update_no_conflict_disjoint_fields() {
        let old = json!({"replicas": 3});
        let new = json!({"replicas": 5});
        let mut other = Set::new();
        other.insert(&path(&["minReadySeconds"]));
        let managers = BTreeMap::from([("other-controller".to_string(), other.clone())]);

        let (result, cmp) = update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &old,
            &new,
            &managers,
            false,
        )
        .expect("no conflict: other-controller doesn't own replicas");

        assert!(cmp.modified.has(&path(&["replicas"])));
        assert_eq!(result.get("other-controller"), Some(&other));
    }

    #[test]
    fn update_conflict_rejected_without_force() {
        let old = json!({"replicas": 3});
        let new = json!({"replicas": 5});
        let mut other = Set::new();
        other.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("other-controller".to_string(), other)]);

        let err = update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &old,
            &new,
            &managers,
            false,
        )
        .expect_err("other-controller owns replicas, which this write is changing");

        assert_eq!(err.len(), 1);
        assert_eq!(err[0].manager, "other-controller");
        assert!(err[0].fields.has(&path(&["replicas"])));
    }

    #[test]
    fn update_conflict_forced_takes_ownership() {
        let old = json!({"replicas": 3});
        let new = json!({"replicas": 5});
        let mut other = Set::new();
        other.insert(&path(&["replicas"]));
        other.insert(&path(&["minReadySeconds"]));
        let managers = BTreeMap::from([("other-controller".to_string(), other)]);

        let (result, _) = update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &old,
            &new,
            &managers,
            true,
        )
        .expect("force: true never rejects");

        let remaining = result
            .get("other-controller")
            .expect("other-controller keeps minReadySeconds");
        assert!(!remaining.has(&path(&["replicas"])), "taken by this write");
        assert!(remaining.has(&path(&["minReadySeconds"])), "untouched field stays owned");
    }

    #[test]
    fn update_removed_field_dropped_from_every_manager() {
        let old = json!({"replicas": 3, "minReadySeconds": 10});
        let new = json!({"replicas": 3});
        let mut other = Set::new();
        other.insert(&path(&["minReadySeconds"]));
        other.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("other-controller".to_string(), other)]);

        let (result, cmp) = update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &old,
            &new,
            &managers,
            false,
        )
        .expect("removing a field this write doesn't own is not a conflict");

        assert!(cmp.removed.has(&path(&["minReadySeconds"])));
        let remaining = result
            .get("other-controller")
            .expect("other-controller keeps replicas");
        assert!(!remaining.has(&path(&["minReadySeconds"])), "removed everywhere, not just the writer's own set");
        assert!(remaining.has(&path(&["replicas"])));
    }

    #[test]
    fn update_manager_dropped_once_its_set_is_empty() {
        let old = json!({"replicas": 3});
        let new = json!({"replicas": 5});
        let mut other = Set::new();
        other.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("other-controller".to_string(), other)]);

        let (result, _) = update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &old,
            &new,
            &managers,
            true,
        )
        .unwrap();

        assert!(
            !result.contains_key("other-controller"),
            "left with an empty Set, must be dropped entirely, not kept as an empty entry"
        );
    }

    #[test]
    fn apply_update_create_grants_full_set() {
        // liveObject empty (CREATE case, per real upstream's own doc comment).
        let live = json!({});
        let new = json!({"replicas": 3});
        let managers = BTreeMap::new();

        let result = apply_update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &live,
            &new,
            &managers,
            "kubectl-create",
        );

        let mine = result.get("kubectl-create").expect("first write claims replicas");
        assert!(mine.has(&path(&["replicas"])));
    }

    #[test]
    fn apply_update_unchanged_field_stays_owned_by_original_writer() {
        // kubectl-create owns replicas from a prior write; this PUT resends
        // the exact same value for replicas but changes minReadySeconds.
        // real semantics: an unchanged field is neither Modified nor Added,
        // so the second writer must NOT take ownership of it.
        let mut creator_set = Set::new();
        creator_set.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("kubectl-create".to_string(), creator_set)]);

        let live = json!({"replicas": 3});
        let new = json!({"replicas": 3, "minReadySeconds": 10});

        let result = apply_update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &live,
            &new,
            &managers,
            "kubectl-edit",
        );

        assert!(
            result.get("kubectl-create").expect("still owns replicas").has(&path(&["replicas"]))
        );
        assert!(
            result
                .get("kubectl-edit")
                .expect("owns the field it actually changed")
                .has(&path(&["minReadySeconds"]))
        );
        assert!(
            !result.get("kubectl-edit").unwrap().has(&path(&["replicas"])),
            "resending an identical value must not transfer ownership"
        );
    }

    #[test]
    fn apply_update_writer_keeps_prior_fields_this_write_didnt_touch() {
        let mut mine = Set::new();
        mine.insert(&path(&["replicas"]));
        let managers = BTreeMap::from([("kubectl-edit".to_string(), mine)]);

        let live = json!({"replicas": 3});
        let new = json!({"replicas": 3, "minReadySeconds": 10});

        let result = apply_update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &live,
            &new,
            &managers,
            "kubectl-edit",
        );

        let mine = result.get("kubectl-edit").unwrap();
        assert!(mine.has(&path(&["replicas"])), "prior ownership retained");
        assert!(mine.has(&path(&["minReadySeconds"])), "plus the newly-added field");
    }

    #[test]
    fn apply_update_removes_manager_left_with_nothing() {
        let mut mine = Set::new();
        mine.insert(&path(&["minReadySeconds"]));
        let managers = BTreeMap::from([("kubectl-edit".to_string(), mine)]);

        let live = json!({"minReadySeconds": 10});
        let new = json!({});

        let result = apply_update(
            "io.k8s.api.apps.v1.DeploymentSpec",
            &live,
            &new,
            &managers,
            "kubectl-edit",
        );

        assert!(
            !result.contains_key("kubectl-edit"),
            "removed its only owned field and added nothing else — dropped entirely"
        );
    }

    // `prune`

    fn set_of(paths: &[&[&str]]) -> Set {
        let mut s = Set::new();
        for p in paths {
            s.insert(&path(p));
        }
        s
    }

    #[test]
    fn prune_does_nothing_on_a_first_ever_apply() {
        let merged = json!({"replicas": 3});
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &BTreeMap::new(), None);
        assert_eq!(result, merged);
    }

    #[test]
    fn prune_does_nothing_when_last_set_is_empty() {
        let merged = json!({"replicas": 3});
        let empty = Set::new();
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &BTreeMap::new(), Some(&empty));
        assert_eq!(result, merged);
    }

    #[test]
    fn prune_drops_a_field_the_applier_owned_before_but_no_longer_claims() {
        // The applier previously owned "minReadySeconds" but this apply's
        // config -- now merged into `merged` at whatever value it left it
        // at -- no longer mentions it, and nobody else claims it either.
        let merged = json!({"replicas": 5, "minReadySeconds": 10});
        let last_set = set_of(&[&["replicas"], &["minReadySeconds"]]);
        let managers = BTreeMap::from([("kubectl-apply".to_string(), set_of(&[&["replicas"]]))]);
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &managers, Some(&last_set));
        assert_eq!(result, json!({"replicas": 5}));
    }

    #[test]
    fn prune_keeps_a_field_the_applier_still_claims() {
        let merged = json!({"replicas": 5, "minReadySeconds": 10});
        let last_set = set_of(&[&["replicas"], &["minReadySeconds"]]);
        let managers = BTreeMap::from([(
            "kubectl-apply".to_string(),
            set_of(&[&["replicas"], &["minReadySeconds"]]),
        )]);
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &managers, Some(&last_set));
        assert_eq!(result, merged, "still claimed by kubectl-apply's own new config, must survive");
    }

    #[test]
    fn prune_keeps_a_field_dropped_by_the_applier_but_claimed_by_someone_else() {
        let merged = json!({"replicas": 5, "minReadySeconds": 10});
        let last_set = set_of(&[&["replicas"], &["minReadySeconds"]]);
        let managers = BTreeMap::from([
            ("kubectl-apply".to_string(), set_of(&[&["replicas"]])),
            ("hpa-controller".to_string(), set_of(&[&["minReadySeconds"]])),
        ]);
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &managers, Some(&last_set));
        assert_eq!(result, merged, "hpa-controller still owns minReadySeconds, must survive");
    }

    #[test]
    fn prune_never_touches_a_field_the_applier_never_previously_owned() {
        // "replicas" isn't in last_set at all -- prune must leave it
        // alone regardless of who owns what now.
        let merged = json!({"replicas": 5, "minReadySeconds": 10});
        let last_set = set_of(&[&["minReadySeconds"]]);
        let managers = BTreeMap::new();
        let result = prune("io.k8s.api.apps.v1.DeploymentSpec", &merged, &managers, Some(&last_set));
        assert_eq!(result, json!({"replicas": 5}), "minReadySeconds pruned (unclaimed), replicas untouched (never in last_set)");
    }

    // `apply` -- the real Updater.Apply closing orchestration.

    const SCHEMA: &str = "io.k8s.api.apps.v1.DeploymentSpec";

    #[test]
    fn apply_a_first_ever_apply_creates_the_field_and_claims_it() {
        let live = json!({});
        let config = json!({"replicas": 3});
        let result = apply(SCHEMA, &live, &config, &BTreeMap::new(), "kubectl-apply", false).unwrap();
        assert_eq!(result.object, Some(json!({"replicas": 3})));
        assert!(result.managers.get("kubectl-apply").unwrap().has(&path(&["replicas"])));
    }

    #[test]
    fn apply_re_applying_an_identical_config_is_a_real_no_op() {
        let live = json!({"replicas": 3});
        let config = json!({"replicas": 3});
        let managers = BTreeMap::from([("kubectl-apply".to_string(), set_of(&[&["replicas"]]))]);
        let result = apply(SCHEMA, &live, &config, &managers, "kubectl-apply", false).unwrap();
        assert_eq!(result.object, None, "merged-and-pruned result is identical to live -- nothing to write back");
    }

    #[test]
    fn apply_prunes_a_field_the_new_config_stopped_mentioning() {
        let live = json!({"replicas": 3, "minReadySeconds": 10});
        let config = json!({"replicas": 5}); // no longer mentions minReadySeconds
        let managers = BTreeMap::from([(
            "kubectl-apply".to_string(),
            set_of(&[&["replicas"], &["minReadySeconds"]]),
        )]);
        let result = apply(SCHEMA, &live, &config, &managers, "kubectl-apply", false).unwrap();
        assert_eq!(result.object, Some(json!({"replicas": 5})));
        assert!(!result.managers.get("kubectl-apply").unwrap().has(&path(&["minReadySeconds"])));
    }

    #[test]
    fn apply_rejects_a_real_conflict_without_force() {
        let live = json!({"replicas": 3});
        let config = json!({"replicas": 5});
        let managers = BTreeMap::from([("hpa-controller".to_string(), set_of(&[&["replicas"]]))]);
        let err = apply(SCHEMA, &live, &config, &managers, "kubectl-apply", false)
            .expect_err("hpa-controller owns replicas, which this apply is changing");
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].manager, "hpa-controller");
    }

    #[test]
    fn apply_forced_takes_ownership_away_from_the_conflicting_manager() {
        let live = json!({"replicas": 3});
        let config = json!({"replicas": 5});
        let managers = BTreeMap::from([("hpa-controller".to_string(), set_of(&[&["replicas"]]))]);
        let result = apply(SCHEMA, &live, &config, &managers, "kubectl-apply", true).unwrap();
        assert_eq!(result.object, Some(json!({"replicas": 5})));
        assert!(result.managers.get("kubectl-apply").unwrap().has(&path(&["replicas"])));
        assert!(
            !result.managers.get("hpa-controller").is_some_and(|s| s.has(&path(&["replicas"]))),
            "hpa-controller must lose ownership of the field this forced apply just took"
        );
    }

    #[test]
    fn apply_does_not_conflict_over_an_unrelated_field_and_both_managers_survive() {
        let live = json!({"replicas": 3, "minReadySeconds": 10});
        let config = json!({"replicas": 5});
        let managers = BTreeMap::from([("hpa-controller".to_string(), set_of(&[&["minReadySeconds"]]))]);
        let result = apply(SCHEMA, &live, &config, &managers, "kubectl-apply", false).unwrap();
        assert_eq!(result.object, Some(json!({"replicas": 5, "minReadySeconds": 10})));
        assert!(result.managers.get("hpa-controller").unwrap().has(&path(&["minReadySeconds"])));
        assert!(result.managers.get("kubectl-apply").unwrap().has(&path(&["replicas"])));
    }
}
