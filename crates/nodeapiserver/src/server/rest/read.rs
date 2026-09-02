/// Fetches and decodes a single object. `namespace` is `None` for a
/// cluster-scoped resource (matches `storage::keys::object_key`'s own
/// convention) — the caller (`server::listener`) is responsible for
/// turning `path::RequestInfo`'s always-`String` `namespace` field into
/// this `Option` (empty string -> `None`).
///
/// `cache`, if given, is consulted first (`cacher::store::SharedCache::get`,
/// Group D) — a hit skips the `Range` round trip to nodestore entirely.
/// A **miss always falls through to nodestore**, unconditionally, rather
/// than trusting the cache to say "not found": a `cache: Some(_)` that
/// hasn't finished its first `LIST` yet (or isn't registered for this
/// exact resource at all, if a caller ever passed the wrong one) is
/// indistinguishable from "genuinely empty" using only what `SharedCache`
/// exposes today, so treating a miss as authoritative would risk a false
/// `404` during that window. This makes cache consultation a pure
/// latency optimization on the hit path, never a correctness risk on the
/// miss path — real upstream's own watch cache takes the same
/// "consistent read falls through" posture for exactly this reason.
/// `None` behaves exactly as before this parameter existed; callers outside
/// the listener's cache path can still pass `None`.
pub async fn get(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
) -> Result<GetOutcome, Error> {
    get_at_revision(storage, cache, group, version, resource, namespace, name, 0).await
}

/// [`get`] with an optional etcd MVCC snapshot revision. A non-positive
/// revision retains the normal current-state behavior.
pub async fn get_at_revision(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    resource_version: i64,
) -> Result<GetOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(GetOutcome::UnknownResource);
    };
    let kind = resolved.kind;
    let key = keys::object_key(group, resource, namespace, name);

    if resource_version <= 0 {
        if let Some(cache) = cache {
            if let Some(entry) = cache.get(key.as_bytes()) {
                let mut object =
                    decrypt_and_decode(storage, group, resource, key.as_bytes(), &entry.value)?;
                set_metadata_field(
                    &mut object,
                    "resourceVersion",
                    Value::String(entry.mod_revision.to_string()),
                );
                return Ok(GetOutcome::Found(crate::scheme::conversion::to_version(
                    group, version, &kind, object,
                )));
            }
        }
    }

    let resp = storage
        .range(RangeRequest {
            key: key.into_bytes(),
            revision: resource_version.max(0),
            ..Default::default()
        })
        .await?;
    let Some(kv) = resp.kvs.into_iter().next() else {
        return Ok(GetOutcome::ObjectNotFound);
    };
    let mut object = decrypt_and_decode(storage, group, resource, &kv.key, &kv.value)?;
    // Real, load-bearing fix, found live (`tests/apiservice_roundtrip.rs`'s
    // own get-then-update round trip): `resourceVersion` is never
    // actually *persisted* into the stored object bytes (`create`/
    // `persist_update` both stamp it onto their own return value only
    // *after* the write that produced it — the revision doesn't exist
    // yet while the bytes being written are still being built, so there
    // is nothing earlier to persist it into either) — matching real
    // upstream's own posture, where `resourceVersion` is always etcd's
    // own `mod_revision` read back at serve time, never object content.
    // A plain read has to do the same real-time stamping every write
    // path already does, from this exact `Range`'s own `kv.mod_revision`
    // — every prior write-then-read-back test in this crate happened to
    // use a `create`/`update` call's own return value directly, which
    // already carried a real `resourceVersion`, so nothing exercised a
    // genuine `GET` followed by an `UPDATE` until this one did.
    set_metadata_field(
        &mut object,
        "resourceVersion",
        Value::String(kv.mod_revision.to_string()),
    );
    Ok(GetOutcome::Found(crate::scheme::conversion::to_version(
        group, version, &kind, object,
    )))
}

#[derive(Debug, PartialEq)]
pub enum ListOutcome {
    /// The real `<Kind>List` document, ready to serialize.
    Found(Value),
    UnknownResource,
    /// The submitted `continue` token didn't decode — not valid base64,
    /// no `0x00` key/revision separator, or a non-numeric revision.
    /// Real upstream's own `errors.NewBadRequest("continue token is not
    /// valid")` shape, not a `500`.
    InvalidContinueToken,
}

/// The real `<Kind>List` `kind` value for a resource this build serves —
/// standard Kubernetes convention, verified against real vendored data:
/// every List type in the vendored OpenAPI specs is named exactly
/// `<Kind>List` (`PodList`, `DeploymentList`, ...), never a separate
/// hand-assigned name.
fn list_kind(kind: &str) -> String {
    format!("{kind}List")
}

/// Lists every object of a resource — the whole resource, or scoped to
/// one namespace (`namespace: None` for a cluster-scoped resource, same
/// convention as [`get`]). Items are decoded and filtered
/// (`cacher::selector::object_matches`) the same way regardless of source.
/// Items are returned in whatever order the source hands them back in
/// (key order, for both a real `Range` and the cache's own `BTreeMap`) —
/// real upstream doesn't guarantee list ordering either.
/// `label_selector`/`field_selector` are the raw query-string values
/// `path::RequestInfo` already captures for `list` (empty means "no
/// constraint from that half," matching upstream's own `Everything()`
/// selector semantics). `limit`/`continue_token` are real pagination —
/// `limit <= 0` means "no limit" (matching real upstream's own `0`
/// convention), and a non-empty `continue_token` resumes an earlier
/// paginated listing (real upstream's own contract: opaque to the
/// client, only ever handed back verbatim from a prior page's own
/// `metadata.continue`). A paginated request always bypasses the watch
/// cache (see below) and reads directly from nodestore, since real
/// pagination is a genuine ordered range-scan-with-resume-point, which
/// the cache's own unordered in-memory store doesn't support. Real
/// upstream's own documented caveat applies here too: label/field
/// selector filtering happens *after* the limited range fetch, so a
/// page can come back with fewer than `limit` items (even zero) despite
/// more matching items existing on later pages.
///
/// `cache`, if given, is consulted first — but only once
/// [`crate::cacher::store::SharedCache::has_synced`] is true. Unlike
/// [`get`]'s "a miss always falls through" trick, `list` can't use that
/// same safety net: a cache that hasn't finished its first `LIST` yet
/// would report zero items, and zero items is itself a fully valid `LIST`
/// answer (a real `200`, not a `404`) — there is no way to tell "empty
/// because unsynced" from "empty because genuinely empty" after the fact,
/// so this checks `has_synced()` up front instead (see that method's own
/// doc comment for why it's a real flag, not inferred from the revision).
/// An unsynced cache falls through to nodestore exactly as `cache: None`
/// would. `None` behaves exactly as before this parameter existed; callers
/// outside the listener's cache path still pass `None` (same scope as
/// `get`'s own cache parameter).
pub async fn list(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
) -> Result<ListOutcome, Error> {
    list_at_revision(
        storage,
        cache,
        group,
        version,
        resource,
        namespace,
        label_selector,
        field_selector,
        limit,
        continue_token,
        0,
    )
    .await
}

/// [`list`] with an optional etcd MVCC snapshot revision. A positive
/// revision bypasses the live watch cache and returns a consistent snapshot
/// from nodestore.
pub async fn list_at_revision(
    storage: &mut StorageClient,
    cache: Option<&crate::cacher::store::SharedCache>,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    label_selector: &str,
    field_selector: &str,
    limit: i64,
    continue_token: &str,
    resource_version: i64,
) -> Result<ListOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(ListOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();
    let label_reqs = if label_selector.is_empty() {
        Vec::new()
    } else {
        selector::parse_label_selector(label_selector)?
    };
    let field_reqs = if field_selector.is_empty() {
        Vec::new()
    } else {
        selector::parse_field_selector(field_selector)?
    };
    selector::validate_field_selector(group, resource, &field_reqs)?;

    let group_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    // Shared by both the cache path and the direct-nodestore path below —
    // the cache registers one entry per whole `(group, version, resource)`
    // (`cacher::registry`'s own doc comment: "every namespace at once, not
    // one cache per namespace"), so a namespaced request still needs this
    // same prefix to scope the cache's own entries down to one namespace,
    // exactly as it already scopes the `Range` request on the fallback path.
    let prefix = keys::list_prefix(group, resource, namespace).into_bytes();

    // Real upstream itself doesn't serve a paginated request from its own
    // watch cache either — a consistent ordered range-scan-with-resume-point
    // is what the underlying store gives for free and an in-memory
    // unordered cache doesn't. A paginated request (real `limit`/`continue`,
    // not the default "everything") always goes straight to nodestore
    // below, same as an unsynced cache would.
    let paginated = limit > 0 || !continue_token.is_empty() || resource_version > 0;
    if let Some(cache) = cache {
        if cache.has_synced() && !paginated {
            let (entries, revision) = cache.list();
            let items = entries
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(key, entry)| {
                    // Same real fix `get`'s own doc comment covers: a
                    // stored object never carries `resourceVersion` as
                    // persisted content, so every item in a `LIST`
                    // response needs it stamped from its own live
                    // revision, the same way real upstream does.
                    let mut object =
                        decrypt_and_decode(storage, group, resource, key, &entry.value)?;
                    set_metadata_field(
                        &mut object,
                        "resourceVersion",
                        Value::String(entry.mod_revision.to_string()),
                    );
                    object = crate::scheme::conversion::to_version(group, version, kind, object);
                    Ok(object)
                })
                .collect::<Result<Vec<Value>, Error>>()?
                .into_iter()
                .filter(|item| selector::object_matches(item, &label_reqs, &field_reqs))
                .collect::<Vec<Value>>();
            return Ok(ListOutcome::Found(json!({
                "kind": list_kind(kind),
                "apiVersion": group_version,
                "metadata": {"resourceVersion": revision.to_string()},
                "items": items,
            })));
        }
    }

    let range_end = prefix_range_end(&prefix);
    // A `continue` token resumes from the exact key its own page left off
    // at (`encode_continue_token`'s own doc comment covers the "append a
    // single 0x00 byte" idiom that makes this the correct etcd range
    // start), at the same revision the listing began at — every page of
    // one listing sees a consistent snapshot, matching real upstream's
    // own pagination contract.
    let (start_key, at_revision) = if continue_token.is_empty() {
        (prefix, resource_version.max(0))
    } else {
        match decode_continue_token(continue_token) {
            Some((key, revision)) => (key, revision),
            None => return Ok(ListOutcome::InvalidContinueToken),
        }
    };
    let resp = storage
        .range(RangeRequest {
            key: start_key,
            range_end,
            limit: limit.max(0),
            revision: at_revision,
            ..Default::default()
        })
        .await?;
    let revision = resp.header.map(|h| h.revision).unwrap_or(at_revision);
    // Real upstream's own documented caveat applies here too: filtering by
    // label/field selector happens *after* the limited range fetch, so a
    // page can legitimately come back with fewer than `limit` items (or
    // even zero) despite there being more matching items on later pages —
    // this isn't a bug, it's the same trade-off a selector combined with
    // `limit` has against a real etcd-backed apiserver.
    let more = resp.more;
    // The successor marker `encode_continue_token`'s own doc comment
    // expects — appended *here*, not inside that function, so its own
    // internal `0x00` push stays purely about the encoding's key/revision
    // separator (see that function's doc comment for why the two
    // 0x00 bytes this produces when they land back to back is
    // deliberate, not a bug).
    let resume_key = resp.kvs.last().map(|kv| {
        let mut k = kv.key.clone();
        k.push(0);
        k
    });
    let items = resp
        .kvs
        .iter()
        .map(|kv| {
            let mut object = decrypt_and_decode(storage, group, resource, &kv.key, &kv.value)?;
            set_metadata_field(
                &mut object,
                "resourceVersion",
                Value::String(kv.mod_revision.to_string()),
            );
            object = crate::scheme::conversion::to_version(group, version, kind, object);
            Ok(object)
        })
        .collect::<Result<Vec<Value>, Error>>()?
        .into_iter()
        .filter(|item| selector::object_matches(item, &label_reqs, &field_reqs))
        .collect::<Vec<Value>>();

    let mut metadata = json!({"resourceVersion": revision.to_string()});
    if more {
        if let Some(resume_key) = resume_key {
            metadata["continue"] = json!(encode_continue_token(&resume_key, revision));
        }
    }

    Ok(ListOutcome::Found(json!({
        "kind": list_kind(kind),
        "apiVersion": group_version,
        "metadata": metadata,
        "items": items,
    })))
}

/// Real upstream's own continuation-token contract: a client must treat
/// this as fully opaque, never construct or parse one itself. This
/// build's own encoding (base64 of `<resume-key>\0<revision>`) has no
/// compatibility requirement with real upstream's own token format,
/// since nothing outside this crate's own client/server pair ever reads
/// one.
///
/// `resume_key` must already be `list`'s own last-returned key with a
/// single `0x00` byte appended by the caller (the standard etcd idiom
/// for "the immediate lexicographic successor of this key" — exactly
/// the correct next `Range` start to exclude everything already
/// returned while including everything after it: byte-string
/// comparison guarantees any real key strictly greater than `last_key`
/// is always >= `last_key + 0x00`, since `0x00` is the smallest
/// possible byte). This function then appends *its own* `0x00` as the
/// key/revision separator — so a real encoded buffer ends up with two
/// consecutive `0x00` bytes where the successor marker meets the
/// separator, which is deliberate, not a bug: [`decode_continue_token`]
/// finds the *last* one to split on, so the successor marker correctly
/// stays part of the decoded key.
fn encode_continue_token(resume_key: &[u8], revision: i64) -> String {
    use base64::Engine;
    let mut buf = resume_key.to_vec();
    buf.push(0);
    buf.extend_from_slice(revision.to_string().as_bytes());
    base64::engine::general_purpose::STANDARD.encode(buf)
}

/// The inverse of [`encode_continue_token`]. `None` for anything
/// malformed (not valid base64, no `0x00` separator, a non-numeric
/// revision) — surfaced by `list` as a real `ListOutcome::
/// InvalidContinueToken`, not a panic or a silently-wrong resume point.
/// Splits on the *last* `0x00` byte rather than the first, defensively:
/// a resume key built from real object names should never itself
/// contain one (`DNS-1123` names have no room for a null byte), but
/// searching from the end costs nothing and removes even that
/// assumption.
fn decode_continue_token(token: &str) -> Option<(Vec<u8>, i64)> {
    use base64::Engine;
    let buf = base64::engine::general_purpose::STANDARD
        .decode(token)
        .ok()?;
    let separator = buf.iter().rposition(|&b| b == 0)?;
    let (key, rest) = buf.split_at(separator);
    let revision = std::str::from_utf8(&rest[1..]).ok()?.parse::<i64>().ok()?;
    Some((key.to_vec(), revision))
}
