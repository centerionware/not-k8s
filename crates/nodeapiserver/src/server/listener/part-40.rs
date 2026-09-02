
/// Group L Phase 4's dispatch glue for one already-matched, non-local
/// `APIService`: fetch its backing Service and `EndpointSlice`s, run the
/// same real pre-flight chain `aggregator::availability::preflight_check`
/// would run before a live discovery-endpoint dial, resolve the actual
/// dial target (`aggregator::proxy_target`), build this backend's own
/// TLS trust (`aggregator::client_tls`), and relay the whole request —
/// method, headers minus [`HOP_BY_HOP_HEADERS`], body — unmodified
/// (`proxy::http_client::relay`). A real transparent proxy, matching
/// real upstream's own aggregation posture exactly: nothing about the
/// request or response is inspected or altered beyond what dialing
/// itself requires.
///
/// A cached `Available: False` condition (`aggregator::reconcile`'s own
/// periodic write, `availability::cached_available`) short-circuits
/// straight to `503` before any of the Service/`EndpointSlice` I/O below
/// — a known-broken backend fails fast without paying for a fetch this
/// build already knows the answer to. `Available: True` or no cached
/// condition yet both fall through to the full check unchanged (the
/// backing Service still has to be fetched either way, to resolve the
/// actual dial target — this only ever saves the *negative* path).
async fn aggregate_proxy(req: Request<Incoming>, method: &str, api_service: &serde_json::Value, mut client: StorageClient, path_str: &str, query: &str) -> Response<BoxedBody> {
    include!("body-68-1.rs");
    include!("body-68-2.rs");
}
