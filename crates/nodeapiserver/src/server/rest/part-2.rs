
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
    let buf = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let separator = buf.iter().rposition(|&b| b == 0)?;
    let (key, rest) = buf.split_at(separator);
    let revision = std::str::from_utf8(&rest[1..]).ok()?.parse::<i64>().ok()?;
    Some((key.to_vec(), revision))
}

#[derive(Debug, PartialEq)]
pub enum CreateOutcome {
    /// The stored object, exactly as written (defaults applied,
    /// `creationTimestamp`/`uid`/`resourceVersion` set for real).
    Created(Value),
    UnknownResource,
    /// Neither `metadata.name` nor a usable `metadata.generateName` was
    /// present in the submitted body.
    MissingName,
    /// `metadata.namespace` in the body disagreed with the URL's own
    /// namespace — real upstream rejects this rather than silently
    /// preferring one over the other.
    NamespaceMismatch,
    /// An object already exists at this key — real upstream's own
    /// `AlreadyExists` outcome.
    AlreadyExists,
    /// `scheme::validation`'s own findings, formatted as one message per
    /// violation (`"containers[1].name: Required value"`-shaped) — the
    /// caller's job to turn into a real `422 Unprocessable Entity`.
    Invalid(Vec<String>),
    /// No usable compiled or runtime structural schema was available for
    /// the resolved resource. Established CRDs normally carry the latter;
    /// this remains a defensive outcome for malformed or legacy CRD data.
    UnsupportedForCrd,
}

/// Creates a new object. `namespace: None` for a cluster-scoped resource,
/// same convention as [`get`]/[`list`]. `body` is the client's raw
/// submitted object, decoded but otherwise untouched — this function
/// validates and defaults it, it doesn't trust it.
pub async fn create(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, body: &Value) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(storage, group, version, resource, namespace, body, false, None).await
}

/// [`create`] with the real Kubernetes `dryRun=All` write option. Dry-run
/// still resolves, validates, defaults, and checks for an existing object,
/// but never changes nodestore.
pub async fn create_with_options(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, body: &Value, dry_run: bool) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(storage, group, version, resource, namespace, body, dry_run, None).await
}

/// [`create_with_options`] with the request's field manager. The listener
/// supplies the explicit `fieldManager` or the request's user agent, just as
/// upstream's `managerOrUserAgent` does. Direct REST callers may omit it;
/// their submitted `managedFields` are never trusted or persisted.
pub async fn create_with_options_and_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<CreateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(CreateOutcome::UnknownResource);
    };
    let kind = resolved.kind.as_str();

    let explicit_name = body.pointer("/metadata/name").and_then(Value::as_str).filter(|n| !n.is_empty());
    let generated_prefix = body.pointer("/metadata/generateName").and_then(Value::as_str).filter(|prefix| !prefix.is_empty());
    let Some(name) = explicit_name.map(str::to_string).or_else(|| generated_prefix.map(generate_name)) else {
        return Ok(CreateOutcome::MissingName);
    };
    let mut submitted_body = body.clone();
    if explicit_name.is_none() {
        set_metadata_field(&mut submitted_body, "name", Value::String(name.clone()));
    }
    let body = &submitted_body;

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(CreateOutcome::NamespaceMismatch);
        }
    }

    // Group K: structural-schema pruning runs before validation/defaulting,
    // matching real upstream's own order — a field the schema doesn't
    // declare is silently dropped here rather than surfacing as a
    // validation error, the same way real upstream's own CRD handler
    // behaves (`apiextensions::schema_pruning`'s own doc comment).
    let pruned_body;
    let body: &Value = match &resolved.open_api_schema {
        Some(open_api_schema) => {
            pruned_body = apiextensions::schema_pruning::prune(open_api_schema, body);
            &pruned_body
        }
        None => body,
    };

    let mut violations: Vec<String> = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: real required/type validation against a CRD's own
        // openAPIV3Schema, when it has one.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, body));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, &name).into_iter().map(|e| format!("metadata.name: {e}")));
    // Group K / CEL Phase 3: a CustomResourceDefinition's own
    // `x-kubernetes-validations` rules get their real static cost
    // checked at CRD-acceptance time, real upstream's own posture
    // (`apiextensions::cel_validations`'s own doc comment covers the
    // exact real scope and its one named gap — no `MaxCardinality`
    // multiplication yet).
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
        violations.extend(apiextensions::cel_validations::validate_crd_cel_types(body));
    }
    if !violations.is_empty() {
        return Ok(CreateOutcome::Invalid(violations));
    }

    let mut object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, body),
        (None, None) => body.clone(),
    };
    object = crate::scheme::conversion::to_version(group, version, kind, object);

    // CEL Phase 4: real x-kubernetes-validations rule evaluation against
    // this actual custom resource instance — runs against the
    // fully-defaulted object (real upstream's own ordering: a rule
    // commonly assumes a field already carries its real default, not an
    // absence), `old_value: None` on `CREATE` (real upstream's own
    // `oldSelf` is simply unavailable then, matching
    // `apiextensions::cel_evaluate`'s own doc comment).
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, None);
        if !rule_violations.is_empty() {
            return Ok(CreateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    set_metadata_field(&mut object, "creationTimestamp", Value::String(now_rfc3339()));
    set_metadata_field(&mut object, "uid", Value::String(uuid::Uuid::new_v4().to_string()));
    if let Some(ns) = namespace {
        set_metadata_field(&mut object, "namespace", Value::String(ns.to_string()));
    }

    // Group K: a CustomResourceDefinition's own `status` is entirely
    // server-computed (`apiextensions::conditions`'s own doc comment
    // covers why this build computes it synchronously right here rather
    // than through a separate async establishing controller) — never
    // trusted from whatever the client's submitted body carried under
    // `status`, same "generic status subresource" posture `update_status`
    // already establishes for every other resource's own status.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        let other_crds = list_stored_crds(storage).await?;
        object["status"] = apiextensions::conditions::compute_status(&object, other_crds.iter(), &[], &now_rfc3339());
    }

    object = reconcile_managed_fields(
        resolved.schema,
        resolved.open_api_schema.as_ref(),
        &json!({}),
        object,
        field_manager,
        "Update",
        "",
        group,
        version,
    );

    // Conversion sees the complete object, including the system metadata
    // generated above. This is the same object shape a webhook receives for
    // an object that is about to be persisted, not the pre-create body.
    object = convert_to_storage_version(storage, group, version, resolved.conversion_webhook.as_ref(), object).await?;
    object = match revalidate_storage_object(resolved.storage_open_api_schema.as_ref(), object) {
        Ok(object) => object,
        Err(violations) => return Ok(CreateOutcome::Invalid(violations)),
    };

    let key = keys::object_key(group, resource, namespace, &name);
    if dry_run {
        let existing = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
        if !existing.kvs.is_empty() {
            return Ok(CreateOutcome::AlreadyExists);
        }
        let object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
        return Ok(CreateOutcome::Created(object));
    }
    let stored_version = resolved.conversion_webhook.as_ref().map_or(version, |conversion| conversion.storage_version.as_str());
    let api_version = if group.is_empty() { stored_version.to_string() } else { format!("{group}/{stored_version}") };
    let object_bytes = match resolved.schema {
        Some(schema) => protobuf::encode_message(schema, &object)?,
        None => serde_json::to_vec(&object).map_err(protobuf::Error::Json)?,
    };
    let envelope = protobuf::wrap_unknown(&api_version, kind, &object_bytes);

    // Real upstream's own create-only-if-absent idiom, confirmed against
    // nodestore's own server-side comment naming it exactly this
    // (`crates/nodestore/src/store.rs`): a key with no prior write has
    // ModRevision 0, so a Txn that only Puts when ModRevision == 0 can
    // never silently overwrite an existing object.
    let compare = pb::Compare {
        key: key.clone().into_bytes(),
        result: pb::compare::CompareResult::Equal as i32,
        target: pb::compare::CompareTarget::Mod as i32,
        target_union: Some(pb::compare::TargetUnion::ModRevision(0)),
        range_end: Vec::new(),
    };
    let envelope = encrypt_for_storage(storage, group, resource, key.as_bytes(), &envelope)?;
    let put = pb::PutRequest { key: key.into_bytes(), value: envelope, ..Default::default() };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp { request: Some(pb::request_op::Request::RequestPut(put)) }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        return Ok(CreateOutcome::AlreadyExists);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(&mut object, "resourceVersion", Value::String(revision.to_string()));
    let object = convert_to_requested_version(storage, group, version, kind, resolved.conversion_webhook.as_ref(), object).await?;
    Ok(CreateOutcome::Created(object))
}

#[derive(Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(Value),
    UnknownResource,
    /// No object exists at this key — this build doesn't support
    /// create-on-update (`AllowCreateOnUpdate`, real upstream's own
    /// opt-in a handful of types use), named honestly rather than
    /// silently creating one.
    ObjectNotFound,
    /// The submitted body had no `metadata.resourceVersion` at all —
    /// real upstream's own generic registry requires one for `PUT`
    /// (optimistic concurrency has nothing to compare against
    /// otherwise).
    MissingResourceVersion,
    /// The submitted `resourceVersion` didn't match what's currently
    /// stored — a real conflict, matching real upstream's own
    /// `errors.NewConflict`.
    Conflict,
    NamespaceMismatch,
    Invalid(Vec<String>),
    /// [`patch`] only: the `Content-Type` wasn't one of the three real
    /// patch media types this build understands.
    UnsupportedPatchType,
}

#[derive(Debug, PartialEq)]
pub enum BindOutcome {
    Bound,
    UnknownResource,
    ObjectNotFound,
    Conflict,
    Invalid(Vec<String>),
}

/// Implements the core Pod `binding` subresource used by the scheduler.
/// Real upstream's `BindingREST` validates the binding preconditions, sets
/// `spec.nodeName`, merges binding metadata, and marks the Pod scheduled in
/// one optimistic-concurrency write. Keeping that operation separate from
/// generic `update` matters: a Binding is a small request containing only a
/// target, not a replacement Pod object.
pub async fn bind_pod(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
) -> Result<BindOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(BindOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage
        .range(RangeRequest {
            key: key.clone().into_bytes(),
            ..Default::default()
        })
        .await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(BindOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode(storage, "", "pods", &existing_kv.key, &existing_kv.value)?;

    let mut violations = Vec::new();
    if let Some(binding_name) = body.pointer("/metadata/name").and_then(Value::as_str) {
        if binding_name != name {
            violations.push(format!("metadata.name: must be {name}"));
        }
    }
    if let Some(binding_namespace) = body.pointer("/metadata/namespace").and_then(Value::as_str) {
        if !binding_namespace.is_empty() && binding_namespace != namespace {
            violations.push("metadata.namespace: does not match the request URL".to_string());
        }
    }
    let Some(target_name) = body.pointer("/target/name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
        violations.push("target.name: Required value".to_string());
        return Ok(BindOutcome::Invalid(violations));
    };
    if let Some(uid) = body.pointer("/metadata/uid").and_then(Value::as_str) {
        if existing_object.pointer("/metadata/uid").and_then(Value::as_str) != Some(uid) {
            return Ok(BindOutcome::Conflict);
        }
    }
    if let Some(resource_version) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str) {
        if resource_version.parse::<i64>().ok() != Some(existing_kv.mod_revision) {
            return Ok(BindOutcome::Conflict);
        }
    }
    if existing_object.pointer("/metadata/deletionTimestamp").is_some_and(|timestamp| !timestamp.is_null()) {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object.pointer("/spec/nodeName").and_then(Value::as_str).is_some_and(|node| !node.is_empty()) {
        return Ok(BindOutcome::Conflict);
    }
    if existing_object.pointer("/spec/schedulingGates").and_then(Value::as_array).is_some_and(|gates| !gates.is_empty()) {
        violations.push("spec.schedulingGates: Pod has scheduling gates".to_string());
    }
    if !violations.is_empty() {
        return Ok(BindOutcome::Invalid(violations));
    }

    let mut object = existing_object.clone();
    let Some(object_map) = object.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["Pod must be an object".to_string()]));
    };
    {
        let metadata = object_map.entry("metadata").or_insert_with(|| json!({}));
        let Some(metadata) = metadata.as_object_mut() else {
            return Ok(BindOutcome::Invalid(vec!["metadata must be an object".to_string()]));
        };
        for field in ["annotations", "labels"] {
            if let Some(values) = body.pointer(&format!("/metadata/{field}")) {
                let Some(values) = values.as_object() else {
                    return Ok(BindOutcome::Invalid(vec![format!("metadata.{field} must be an object")]));
                };
                let destination = metadata.entry(field).or_insert_with(|| json!({}));
                let Some(destination) = destination.as_object_mut() else {
                    return Ok(BindOutcome::Invalid(vec![format!("metadata.{field} must be an object")]));
                };
                for (key, value) in values {
                    destination.insert(key.clone(), value.clone());
                }
            }
        }
    }
    let spec = object_map.entry("spec").or_insert_with(|| json!({}));
    let Some(spec) = spec.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["spec must be an object".to_string()]));
    };
    spec.insert("nodeName".to_string(), Value::String(target_name.to_string()));

    let status = object_map.entry("status").or_insert_with(|| json!({}));
    let Some(status) = status.as_object_mut() else {
        return Ok(BindOutcome::Invalid(vec!["status must be an object".to_string()]));
    };
    let conditions = status.entry("conditions").or_insert_with(|| json!([]));
    let Some(conditions) = conditions.as_array_mut() else {
        return Ok(BindOutcome::Invalid(vec!["status.conditions must be an array".to_string()]));
    };
    let message = format!("Successfully assigned {namespace}/{name} to {target_name}");
    if let Some(condition) = conditions.iter_mut().find(|condition| condition.get("type").and_then(Value::as_str) == Some("PodScheduled")) {
        condition["status"] = Value::String("True".to_string());
        condition["reason"] = Value::String("Scheduled".to_string());
        condition["message"] = Value::String(message);
        condition["lastTransitionTime"] = Value::String(now_rfc3339());
    } else {
        conditions.push(json!({
            "type": "PodScheduled",
            "status": "True",
            "reason": "Scheduled",
            "message": message,
            "lastTransitionTime": now_rfc3339(),
        }));
    }

    match persist_update(
        storage,
        resolved.schema,
        None,
        None,
        &resolved.kind,
        "",
        "v1",
        "pods",
        key,
        &existing_kv,
        &existing_object,
        Some(namespace),
        object,
        false,
        None,
        None,
        "",
        false,
    )
    .await?
    {
        UpdateOutcome::Updated(_) => Ok(BindOutcome::Bound),
        UpdateOutcome::Conflict => Ok(BindOutcome::Conflict),
        UpdateOutcome::Invalid(violations) => Ok(BindOutcome::Invalid(violations)),
        UpdateOutcome::UnknownResource | UpdateOutcome::ObjectNotFound => Ok(BindOutcome::ObjectNotFound),
        UpdateOutcome::MissingResourceVersion | UpdateOutcome::NamespaceMismatch | UpdateOutcome::UnsupportedPatchType => Ok(BindOutcome::Invalid(vec!["binding could not be persisted".to_string()])),
    }
}

/// The core Pod `ephemeralcontainers` subresource only exposes the Pod
/// object; its strategy permits changing `spec.ephemeralContainers` and
/// resets every other attempted change back to the stored Pod. Existing
/// ephemeral containers are immutable, so a caller may only append valid
/// new entries. This is the same boundary enforced by upstream's
/// `EphemeralContainersStrategy` before its normal optimistic-concurrency
/// store update.
fn restrict_ephemeral_container_update(existing: &Value, candidate: &Value) -> Result<Value, Vec<String>> {
    let existing_list = ephemeral_container_list(existing)?;
    let candidate_list = ephemeral_container_list(candidate)?;
    let mut violations = Vec::new();

    if candidate_list.len() < existing_list.len() {
        violations.push("spec.ephemeralContainers: existing ephemeral containers may not be removed".to_string());
    }
    for (index, old) in existing_list.iter().enumerate() {
        let old_name = old.get("name").and_then(Value::as_str);
        let replacement = old_name.and_then(|name| {
            candidate_list
                .iter()
                .find(|container| container.get("name").and_then(Value::as_str) == Some(name))
        });
        match replacement {
            None => violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be removed")),
            Some(new) if new != old => {
                violations.push(format!("spec.ephemeralContainers[{index}]: existing ephemeral containers may not be modified"));
            }
            Some(_) => {}
        }
    }

    let ordinary_names: std::collections::BTreeSet<&str> = existing
        .pointer("/spec/containers")
        .into_iter()
        .chain(existing.pointer("/spec/initContainers"))
        .filter_map(Value::as_array)
        .flat_map(|containers| containers.iter())
        .filter_map(|container| container.get("name").and_then(Value::as_str))
        .collect();
    let mut names = std::collections::BTreeSet::new();
    for (index, container) in candidate_list.iter().enumerate() {
        let Some(container) = container.as_object() else {
            violations.push(format!("spec.ephemeralContainers[{index}]: must be an object"));
            continue;
        };
        let Some(name) = container.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
            violations.push(format!("spec.ephemeralContainers[{index}].name: Required value"));
            continue;
        };
        for detail in crate::scheme::name_format::is_dns1123_label(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: {detail}"));
        }
        if !names.insert(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: must be unique"));
        }
        if ordinary_names.contains(name) {
            violations.push(format!("spec.ephemeralContainers[{index}].name: must not duplicate a regular or init container"));
        }
        if let Some(target) = container.get("targetContainerName").and_then(Value::as_str).filter(|target| !target.is_empty()) {
            if !ordinary_names.contains(target) {
                violations.push(format!("spec.ephemeralContainers[{index}].targetContainerName: must name an existing regular or init container"));
            }
        }
        for field in ["ports", "resources", "lifecycle", "livenessProbe", "readinessProbe", "startupProbe"] {
            if container.get(field).is_some_and(|value| !value.is_null()) {
                violations.push(format!("spec.ephemeralContainers[{index}].{field}: field is not allowed for an ephemeral container"));
            }
        }
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let mut object = existing.clone();
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
        return Err(vec!["spec: must be an object".to_string()]);
    };
    if candidate_list.is_empty() {
        spec.remove("ephemeralContainers");
    } else {
        spec.insert("ephemeralContainers".to_string(), Value::Array(candidate_list.to_vec()));
    }
    if candidate_list != existing_list {
        let generation = object.pointer("/metadata/generation").and_then(Value::as_i64).unwrap_or(0);
        set_metadata_field(&mut object, "generation", Value::Number((generation + 1).into()));
    }
    Ok(object)
}

fn ephemeral_container_list(object: &Value) -> Result<&[Value], Vec<String>> {
    match object.pointer("/spec/ephemeralContainers") {
        None => Ok(&[]),
        Some(Value::Array(containers)) => Ok(containers),
        Some(_) => Err(vec!["spec.ephemeralContainers: must be an array".to_string()]),
    }
}

/// Reads a Pod through the `ephemeralcontainers` subresource. Upstream
/// returns the complete Pod because the subresource strategy only narrows
/// writes; the caller still needs the ordinary metadata and status fields
/// to observe the result.
pub async fn get_ephemeral_containers(storage: &mut StorageClient, namespace: &str, name: &str) -> Result<GetOutcome, Error> {
    get(storage, None, "", "v1", "pods", Some(namespace), name).await
}

/// Replaces a Pod through the `ephemeralcontainers` subresource. Only the
/// ephemeral-container list from `body` is retained; spec, status, and
/// ordinary metadata changes are discarded by the subresource strategy.
pub async fn update_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, "", "pods", &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let body = convert_to_requested_version(storage, "", "v1", &resolved.kind, None, body.clone()).await?;
    let object = match restrict_ephemeral_container_update(&existing_object, &body) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
    persist_update(
        storage,
        resolved.schema,
        None,
        None,
        &resolved.kind,
        "",
        "v1",
        "pods",
        key,
        &existing_kv,
        &existing_object,
        Some(namespace),
        object,
        dry_run,
        None,
        field_manager,
        "ephemeralcontainers",
        false,
    )
    .await
}

/// Applies a JSON/merge/strategic patch through the Pod
/// `ephemeralcontainers` subresource. The patch is evaluated against the
/// complete current Pod, then only its resulting ephemeral-container list
/// is retained, matching upstream's reset-fields strategy.
pub async fn patch_ephemeral_containers(
    storage: &mut StorageClient,
    namespace: &str,
    name: &str,
    kind_of_patch: PatchKind,
    patch_doc: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, "", "v1", "pods").await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let key = keys::object_key("", "pods", Some(namespace), name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, "", "pods", &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;
    let patched = match apply_patch(kind_of_patch, resolved.schema, None, &existing_object, patch_doc) {
        Ok(object) => object,
        Err(message) => return Ok(UpdateOutcome::Invalid(vec![message])),
    };
    let object = match restrict_ephemeral_container_update(&existing_object, &patched) {
        Ok(object) => object,
        Err(violations) => return Ok(UpdateOutcome::Invalid(violations)),
    };
    persist_update(
        storage,
        resolved.schema,
        None,
        None,
        &resolved.kind,
        "",
        "v1",
        "pods",
        key,
        &existing_kv,
        &existing_object,
        Some(namespace),
        object,
        dry_run,
        None,
        field_manager,
        "ephemeralcontainers",
        false,
    )
    .await
}

/// Replaces an existing object. `namespace: None` for a cluster-scoped
/// resource, same convention as [`get`]/[`create`]. Real optimistic
/// concurrency: reads the current object first, requires the submitted
/// body's own `metadata.resourceVersion` to match what's actually
/// stored, and writes with a `Txn` compared against that same revision
/// — a concurrent write between the read and this write loses the race
/// and gets a real `Conflict`, not a silent overwrite.
/// `metadata.creationTimestamp`/`uid` are preserved from the existing
/// object regardless of what the client submitted — real upstream
/// treats both as immutable after creation.
pub async fn update(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(storage, group, version, resource, namespace, name, body, false, None).await
}

/// [`update`] with the real Kubernetes `dryRun=All` write option. The
/// candidate is prepared exactly like a normal update, but the final
/// optimistic-concurrency write is omitted.
pub async fn update_with_options(storage: &mut StorageClient, group: &str, version: &str, resource: &str, namespace: Option<&str>, name: &str, body: &Value, dry_run: bool) -> Result<UpdateOutcome, Error> {
    update_with_options_and_manager(storage, group, version, resource, namespace, name, body, dry_run, None).await
}

/// [`update_with_options`] with the request's field manager. Ordinary
/// updates use the same `Update` managed-fields operation as upstream and do
/// not report ownership conflicts; changed fields move to this manager.
pub async fn update_with_options_and_manager(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    dry_run: bool,
    field_manager: Option<&str>,
) -> Result<UpdateOutcome, Error> {
    let Some(resolved) = resolve_resource(storage, group, version, resource).await? else {
        return Ok(UpdateOutcome::UnknownResource);
    };
    let kind = resolved.kind.clone();

    let key = keys::object_key(group, resource, namespace, name);
    let existing_resp = storage.range(RangeRequest { key: key.clone().into_bytes(), ..Default::default() }).await?;
    let Some(existing_kv) = existing_resp.kvs.into_iter().next() else {
        return Ok(UpdateOutcome::ObjectNotFound);
    };
    let existing_object = decrypt_and_decode_with_rotation(storage, group, resource, &existing_kv.key, &existing_kv.value, existing_kv.mod_revision).await?;

    if let (Some(ns), Some(body_ns)) = (namespace, body.pointer("/metadata/namespace").and_then(Value::as_str)) {
        if !body_ns.is_empty() && body_ns != ns {
            return Ok(UpdateOutcome::NamespaceMismatch);
        }
    }

    // Compared numerically, not as strings — resourceVersion is an
    // opaque string to a real client, but this build's own encoding of
    // it is always the decimal MVCC revision, so parsing avoids any
    // formatting-mismatch false negative (leading zeros, etc.).
    let Some(submitted_rv) = body.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(UpdateOutcome::MissingResourceVersion);
    };
    if submitted_rv != existing_kv.mod_revision {
        return Ok(UpdateOutcome::Conflict);
    }

    // Group K: same pruning `create` runs, same order (before validation/
    // defaulting).
    let pruned_body;
    let body: &Value = match &resolved.open_api_schema {
        Some(open_api_schema) => {
            pruned_body = apiextensions::schema_pruning::prune(open_api_schema, body);
            &pruned_body
        }
        None => body,
    };

    let mut violations: Vec<String> = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => {
            let mut v: Vec<String> = validation::validate_required(schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(validation::validate_types(schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v
        }
        // Group K: same scope `create`'s own CRD branch runs.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> = apiextensions::schema_validation::validate_required(open_api_schema, body).into_iter().map(|m| format!("{}: Required value", m.path)).collect();
            v.extend(apiextensions::schema_validation::validate_types(open_api_schema, body).into_iter().map(|t| format!("{}: expected type {}, got {}", t.path, t.expected, t.actual_kind)));
            v.extend(apiextensions::schema_validation::validate_constraints(open_api_schema, body));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(name_format_violations(group, resource, name).into_iter().map(|e| format!("metadata.name: {e}")));
    // Group K / CEL Phase 3: same real static cost check `create`'s own
    // CRD branch runs.
    if group == "apiextensions.k8s.io" && resource == "customresourcedefinitions" {
        violations.extend(apiextensions::cel_validations::validate_crd_cel_costs(body));
        violations.extend(apiextensions::cel_validations::validate_crd_cel_types(body));
    }
    if !violations.is_empty() {
        return Ok(UpdateOutcome::Invalid(violations));
    }

    let object = match (resolved.schema, &resolved.open_api_schema) {
        (Some(schema), _) => defaulting::apply_defaults(schema, body),
        (None, Some(open_api_schema)) => apiextensions::schema_defaults::apply_defaults(open_api_schema, body),
        (None, None) => body.clone(),
    };

    // CEL Phase 4: same real rule evaluation `create`'s own CRD branch
    // runs, `old_value: Some(&existing_object)` this time — real
    // upstream's own `oldSelf` binding is exactly the object as it was
    // immediately before this update.
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations = apiextensions::cel_evaluate::validate_object(open_api_schema, &object, Some(&existing_object));
        if !rule_violations.is_empty() {
            return Ok(UpdateOutcome::Invalid(rule_violations.into_iter().map(|v| v.to_string()).collect()));
        }
    }

    persist_update(storage, resolved.schema, resolved.open_api_schema.as_ref(), resolved.storage_open_api_schema.as_ref(), &kind, group, version, resource, key, &existing_kv, &existing_object, namespace, object, dry_run, resolved.conversion_webhook.clone(), field_manager, "", false).await
}
