
/// Parses the write-only `dryRun` query option. Kubernetes currently defines
/// one value, `All`; accepting anything else would make a misspelled option
/// look like a successful persisted write.
fn dry_run_query(query: &str) -> Result<bool, &'static str> {
    include!("body-23-1.rs");
    include!("body-23-2.rs");
}
