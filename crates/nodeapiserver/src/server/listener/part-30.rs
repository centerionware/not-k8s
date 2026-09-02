
/// Pure and unit-tested (unlike `handle`, which needs a live TLS
/// connection to exercise at all): `parts` is the already-split, prefix-
/// intact path (`["api", "v1"]`, `["apis", "apps", "v1"]`, ...) from
/// [`path::split_path`]. `accept_header` is the raw `Accept` header value,
/// if any — its only job here is picking legacy vs. aggregated discovery
/// for the two group-list routes (`/api`, `/apis`); every other route
/// ignores it entirely (it already only serves one shape).
/// `crds` — Group K's own discovery merge: every served, `Established`
/// CRD's resources, only ever non-empty for an `/apis`-prefixed path
/// (the core group at `/api` never has CRDs in it — a CRD's own
/// `spec.group` is never empty, real upstream's own CRD validation
/// requires it). `handle`'s own call site fetches this live (one `LIST`
/// of `customresourcedefinitions`) only when the path actually starts
/// with `apis`, rather than paying that cost on every single discovery
/// request — see that call site's own comment.
/// The pure decision half of Group L Phase 3's live discovery proxy: is
/// `parts` exactly a bare `/apis/{group}/{version}` path (`route_discovery`'s
/// own `NotFound` outcome for it means no local answer exists at all —
/// not statically, not via a CRD), and does `aggregated` (the same
/// pre-flight-gated live list `server::listener::handle`'s own caller
/// already fetched) claim that exact `(group, version)`? `Some` hands
/// back borrowed references into `parts`/`aggregated` themselves — no
/// cloning needed, the caller only ever uses them for one more `resolve`
/// call before either succeeding or falling through to a real `404`.
fn aggregated_discovery_group_version<'a>(parts: &'a [String], aggregated: &'a [(String, String)]) -> Option<(&'a str, &'a str)> {
    include!("body-43-1.rs");
    include!("body-43-2.rs");
}
