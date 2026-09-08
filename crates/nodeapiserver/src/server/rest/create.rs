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
pub async fn create(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(
        storage, group, version, resource, namespace, body, false, None,
    )
    .await
}

/// [`create`] with the real Kubernetes `dryRun=All` write option. Dry-run
/// still resolves, validates, defaults, and checks for an existing object,
/// but never changes nodestore.
pub async fn create_with_options(
    storage: &mut StorageClient,
    group: &str,
    version: &str,
    resource: &str,
    namespace: Option<&str>,
    body: &Value,
    dry_run: bool,
) -> Result<CreateOutcome, Error> {
    create_with_options_and_manager(
        storage, group, version, resource, namespace, body, dry_run, None,
    )
    .await
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

    let explicit_name = body
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty());
    let generated_prefix = body
        .pointer("/metadata/generateName")
        .and_then(Value::as_str)
        .filter(|prefix| !prefix.is_empty());
    let Some(name) = explicit_name
        .map(str::to_string)
        .or_else(|| generated_prefix.map(generate_name))
    else {
        return Ok(CreateOutcome::MissingName);
    };
    let mut submitted_body = body.clone();
    if explicit_name.is_none() {
        set_metadata_field(&mut submitted_body, "name", Value::String(name.clone()));
    }
    let body = &submitted_body;

    if let (Some(ns), Some(body_ns)) = (
        namespace,
        body.pointer("/metadata/namespace").and_then(Value::as_str),
    ) {
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
            let mut v: Vec<String> = validation::validate_required(schema, body)
                .into_iter()
                .map(|m| format!("{}: Required value", m.path))
                .collect();
            v.extend(
                validation::validate_types(schema, body)
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    }),
            );
            v.extend(validation::validate_openapi_constraints(
                group, version, kind, body,
            ));
            v
        }
        // Group K: real required/type validation against a CRD's own
        // openAPIV3Schema, when it has one.
        (None, Some(open_api_schema)) => {
            let mut v: Vec<String> =
                apiextensions::schema_validation::validate_required(open_api_schema, body)
                    .into_iter()
                    .map(|m| format!("{}: Required value", m.path))
                    .collect();
            v.extend(
                apiextensions::schema_validation::validate_types(open_api_schema, body)
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{}: expected type {}, got {}",
                            t.path, t.expected, t.actual_kind
                        )
                    }),
            );
            v.extend(apiextensions::schema_validation::validate_constraints(
                open_api_schema,
                body,
            ));
            v
        }
        (None, None) => Vec::new(),
    };
    violations.extend(
        name_format_violations(group, resource, &name)
            .into_iter()
            .map(|e| format!("metadata.name: {e}")),
    );
    violations.extend(metadata_format_violations(body));
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
        (None, Some(open_api_schema)) => {
            apiextensions::schema_defaults::apply_defaults(open_api_schema, body)
        }
        (None, None) => body.clone(),
    };
    object = defaulting::apply_builtin_defaults(group, version, kind, object);
    object = crate::scheme::conversion::to_version(group, version, kind, object);
    if group.is_empty() && resource == "services" {
        object = allocate_cluster_ip(storage, object).await?;
    }

    // CEL Phase 4: real x-kubernetes-validations rule evaluation against
    // this actual custom resource instance — runs against the
    // fully-defaulted object (real upstream's own ordering: a rule
    // commonly assumes a field already carries its real default, not an
    // absence), `old_value: None` on `CREATE` (real upstream's own
    // `oldSelf` is simply unavailable then, matching
    // `apiextensions::cel_evaluate`'s own doc comment).
    if let Some(open_api_schema) = &resolved.open_api_schema {
        let rule_violations =
            apiextensions::cel_evaluate::validate_object(open_api_schema, &object, None);
        if !rule_violations.is_empty() {
            return Ok(CreateOutcome::Invalid(
                rule_violations.into_iter().map(|v| v.to_string()).collect(),
            ));
        }
    }

    set_metadata_field(
        &mut object,
        "creationTimestamp",
        Value::String(now_rfc3339()),
    );
    set_metadata_field(
        &mut object,
        "uid",
        Value::String(uuid::Uuid::new_v4().to_string()),
    );
    // Real upstream's `rest.BeforeCreate` stamps every object's
    // `metadata.generation` to 1 unconditionally, regardless of resource
    // type — not just ones this crate happens to bump later (`scale.rs`,
    // `subresources.rs`'s ephemeralcontainers path). Without this, a
    // freshly created object's `generation` stays absent forever unless
    // one of those two narrow subresource paths happens to touch it,
    // which left every other resource's `PodCondition.observedGeneration`
    // (and any other consumer keying off generation) with nothing to
    // observe.
    set_metadata_field(&mut object, "generation", Value::Number(1.into()));
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
        object["status"] = apiextensions::conditions::compute_status(
            &object,
            other_crds.iter(),
            &[],
            &now_rfc3339(),
        );
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
        resolved.has_status_subresource,
    );

    // Conversion sees the complete object, including the system metadata
    // generated above. This is the same object shape a webhook receives for
    // an object that is about to be persisted, not the pre-create body.
    object = convert_to_storage_version(
        storage,
        group,
        version,
        resolved.conversion_webhook.as_ref(),
        object,
    )
    .await?;
    object = match revalidate_storage_object(resolved.storage_open_api_schema.as_ref(), object) {
        Ok(object) => object,
        Err(violations) => return Ok(CreateOutcome::Invalid(violations)),
    };

    let key = keys::object_key(group, resource, namespace, &name);
    if dry_run {
        let existing = storage
            .range(RangeRequest {
                key: key.clone().into_bytes(),
                ..Default::default()
            })
            .await?;
        if !existing.kvs.is_empty() {
            return Ok(CreateOutcome::AlreadyExists);
        }
        let object = convert_to_requested_version(
            storage,
            group,
            version,
            kind,
            resolved.conversion_webhook.as_ref(),
            object,
        )
        .await?;
        return Ok(CreateOutcome::Created(object));
    }
    let stored_version = resolved
        .conversion_webhook
        .as_ref()
        .map_or(version, |conversion| conversion.storage_version.as_str());
    let api_version = if group.is_empty() {
        stored_version.to_string()
    } else {
        format!("{group}/{stored_version}")
    };
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
    let put = pb::PutRequest {
        key: key.into_bytes(),
        value: envelope,
        ..Default::default()
    };
    let txn = pb::TxnRequest {
        compare: vec![compare],
        success: vec![pb::RequestOp {
            request: Some(pb::request_op::Request::RequestPut(put)),
        }],
        failure: vec![],
    };
    let resp = storage.txn(txn).await?;
    if !resp.succeeded {
        return Ok(CreateOutcome::AlreadyExists);
    }

    let revision = resp.header.map(|h| h.revision).unwrap_or(0);
    set_metadata_field(
        &mut object,
        "resourceVersion",
        Value::String(revision.to_string()),
    );
    let object = convert_to_requested_version(
        storage,
        group,
        version,
        kind,
        resolved.conversion_webhook.as_ref(),
        object,
    )
    .await?;
    Ok(CreateOutcome::Created(object))
}

/// The env var real upstream's own `--service-cluster-ip-range` flag maps
/// to. Read directly here rather than threaded through [`Config`] — every
/// `create` call site (`server::listener`'s dispatch table, every direct
/// REST test) would otherwise need a new parameter just to plumb one
/// string a handful of call sites deep, and this crate already has
/// precedent elsewhere for a narrowly-scoped env read (`config.rs`'s own
/// `from_env` is itself just this, generalized). Matches
/// `nodebootstrap::config::DEFAULT_SERVICE_CIDR` and
/// `deploy/lib/upstream-kube-apiserver.sh`'s own `SERVICE_CIDR` default —
/// this crate has no way to import that constant (`nodebootstrap` depends
/// on `nodeapiserver`, not the reverse), so the literal is duplicated
/// rather than shared.
///
/// [`Config`]: crate::config::Config
fn service_cluster_ip_range() -> String {
    std::env::var("NODEAPISERVER_SERVICE_CLUSTER_IP_RANGE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "10.43.0.0/16".to_string())
}

/// Assigns `spec.clusterIP`/`spec.clusterIPs` on a Service create — the one
/// piece of real upstream's REST create path this build never implemented
/// at all. `scheme::defaulting` is pure and stateless (no storage access),
/// but ClusterIP assignment is inherently stateful: it must not hand out an
/// address any other live Service already holds, so it cannot live there.
/// Real upstream tracks this with a persistent `ipallocator.Interface` (a
/// bitmap, itself a real object in etcd); this build instead does a
/// linear scan of every already-stored Service's own
/// `spec.clusterIP`/`spec.clusterIPs` for the next free address in
/// [`service_cluster_ip_range`] — correct, if not O(1), and there is no
/// other persistent "allocator" object in this build's storage a Service
/// create could consult instead. `default/kubernetes` (nodebootstrap's
/// `service_reconciler.rs`) is always created with its own explicit
/// `spec.clusterIP` before any ordinary Service create happens, so this
/// function's "respect an explicitly requested clusterIP" branch — the
/// same branch a `kubectl create -f` for an already-known IP, or a
/// disaster-recovery restore, needs — never collides with it: that
/// address is already among the ones scanned as "used" for every create
/// after that first one.
async fn allocate_cluster_ip(
    storage: &mut StorageClient,
    mut object: Value,
) -> Result<Value, Error> {
    // Owned copies only: `spec`'s borrow of `object` must not outlive this
    // block, since every branch below either returns `object` by value or
    // mutates it through a fresh `as_object_mut()` borrow.
    let Some(spec) = object.get("spec").and_then(Value::as_object) else {
        return Ok(object);
    };
    let service_type = spec
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("ClusterIP")
        .to_string();
    let requested = spec
        .get("clusterIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty())
        .map(str::to_string);
    if service_type == "ExternalName" {
        return Ok(object);
    }
    // Headless (`clusterIP: "None"`) never gets a real address.
    if requested.as_deref() == Some("None") {
        return Ok(object);
    }
    let ip = match requested {
        Some(ip) => ip,
        None => {
            let cidr = service_cluster_ip_range();
            let used = collect_used_cluster_ips(storage).await?;
            match next_free_cluster_ip(&cidr, &used) {
                Some(ip) => ip,
                // The range is exhausted (or misconfigured). Real
                // upstream would reject the create outright
                // (`ServerTimeoutError: rangeIsFull`); this build leaves
                // clusterIP unset rather than invent a new
                // `CreateOutcome` variant no caller maps to a real status
                // yet — a real, named gap, not a silent success.
                None => return Ok(object),
            }
        }
    };
    if let Some(map) = object.as_object_mut() {
        if let Some(spec) = map.get_mut("spec").and_then(Value::as_object_mut) {
            spec.insert("clusterIP".to_string(), Value::String(ip.clone()));
            let has_ips = spec
                .get("clusterIPs")
                .and_then(Value::as_array)
                .is_some_and(|ips| !ips.is_empty());
            if !has_ips {
                spec.insert("clusterIPs".to_string(), json!([ip]));
            }
        }
    }
    Ok(object)
}

/// Every already-stored Service's own `spec.clusterIP`/`spec.clusterIPs`,
/// across every namespace — ClusterIP allocation is cluster-scoped, the
/// same way real upstream's own allocator is a single cluster-wide bitmap
/// regardless of which namespace a Service lives in.
async fn collect_used_cluster_ips(
    storage: &mut StorageClient,
) -> Result<std::collections::HashSet<std::net::Ipv4Addr>, Error> {
    let prefix = keys::list_prefix("", "services", None).into_bytes();
    let range_end = prefix_range_end(&prefix);
    let resp = storage
        .range(RangeRequest {
            key: prefix,
            range_end,
            ..Default::default()
        })
        .await?;
    let mut used = std::collections::HashSet::new();
    for kv in resp.kvs {
        let object = decrypt_and_decode_with_rotation(
            storage,
            "",
            "services",
            &kv.key,
            &kv.value,
            kv.mod_revision,
        )
        .await?;
        let mut record_ip = |ip: &str| {
            if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                used.insert(addr);
            }
        };
        if let Some(ip) = object.pointer("/spec/clusterIP").and_then(Value::as_str) {
            record_ip(ip);
        }
        if let Some(ips) = object.pointer("/spec/clusterIPs").and_then(Value::as_array) {
            for ip in ips.iter().filter_map(Value::as_str) {
                record_ip(ip);
            }
        }
    }
    Ok(used)
}

/// The first IPv4 address in `cidr` (`"10.43.0.0/16"`-shaped) not present
/// in `used`, skipping the network and broadcast addresses. `None` means
/// the CIDR failed to parse or the whole range is already allocated.
fn next_free_cluster_ip(cidr: &str, used: &std::collections::HashSet<std::net::Ipv4Addr>) -> Option<String> {
    let (base, prefix_len) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = base.parse().ok()?;
    let prefix_len: u32 = prefix_len.parse().ok()?;
    if prefix_len == 0 || prefix_len > 32 {
        return None;
    }
    let mask = if prefix_len == 32 { u32::MAX } else { !0u32 << (32 - prefix_len) };
    let network = u32::from(base) & mask;
    let broadcast = network | !mask;
    if broadcast <= network + 1 {
        return None;
    }
    for candidate in (network + 1)..broadcast {
        let addr = std::net::Ipv4Addr::from(candidate);
        if !used.contains(&addr) {
            return Some(addr.to_string());
        }
    }
    None
}
