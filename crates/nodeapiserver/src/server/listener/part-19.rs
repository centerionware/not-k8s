
/// Real upstream's own `Conflict` shape for a Server-Side Apply
/// ownership conflict — `reason: "Conflict"`, `code: 409`. Same "real
/// subset, not the full type" posture every other `Status` builder in
/// this module takes: real upstream's own structured
/// `Status.details.causes` (one `field.ManagedFieldsConflict` entry per
/// conflicting manager) isn't built, `message` joins them into one
/// human-readable string instead.
fn ssa_conflict_status(path_str: &str, conflicts: &[crate::patch::updater::Conflict]) -> serde_json::Value {
    include!("body-27-1.rs");
    include!("body-27-2.rs");
}
