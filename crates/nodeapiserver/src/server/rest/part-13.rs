
/// A raw `Range` over every stored `CustomResourceDefinition`, decoded —
/// deliberately *not* [`list`] itself: [`list`] calls [`resolve_resource`]
/// to find out what it's listing, and [`resolve_resource`]'s own CRD
/// fallback needs this same data, so calling back into `list` here would
/// be a real `async fn` recursion cycle (rejected outright by rustc,
/// `E0733` — infinitely-sized future, not merely a style objection) even
/// though it would never actually recurse more than once at runtime (the
/// CRD group is always resolved by the static table, never this
/// fallback). `customresourcedefinitions` is always cluster-scoped and
/// its own resource is never itself encrypted-at-rest-configurable in a
/// way this function needs to special-case — `decrypt_and_decode`
/// already handles "no transformer configured for this group/resource"
/// as a plain pass-through.
async fn list_stored_crds(storage: &mut StorageClient) -> Result<Vec<Value>, Error> {
    include!("body-17-1.rs");
    include!("body-17-2.rs");
}
