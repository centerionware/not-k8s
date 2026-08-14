//! `DynamicResources` — DRA. Get a pod's `ResourceClaim`s allocated and
//! reserved, real devices and real CEL selectors, not a placeholder.
//!
//! # Scope
//!
//! Implemented, faithfully: a claim's `spec.devices.requests[]`, each with
//! `deviceClassName` + `count` (`allocationMode: ExactCount`, the default),
//! evaluated against real `ResourceSlice` device inventory using a real CEL
//! interpreter (`cel-interpreter`) for both the request's own `selectors`
//! and its `DeviceClass`'s. An already-allocated claim being reused by a
//! second pod is handled too — only `reservedFor` needs updating, not a new
//! allocation. `PreBind` writes `status.allocation` and appends to
//! `status.reservedFor`, then nothing further blocks — unlike
//! `VolumeBinding`, there is no external process to poll for, so PreBind
//! here is one API round trip, not a wait.
//!
//! `PostFilter` is also implemented (checked against upstream's real
//! `DynamicResources.PostFilter`): when a pod is unschedulable because a
//! claim it already owns is allocated to a topology no node satisfies, and
//! nothing else still needs that allocation (`reservedFor` empty or this
//! pod alone), the allocation is deallocated so the next attempt can pick a
//! different one — cheaper than upstream calls "DRA preemption" ever
//! reaching for `Scheduler::preempt`'s real pod eviction. See the trait's
//! own doc comment for why this needed `PostFilterPlugin` to become async.
//!
//! Also implemented, each checked directly against upstream's real
//! allocator (`k8s.io/dynamic-resource-allocation/structured/allocator.go`)
//! rather than assumed from the API docs:
//!
//!   * `firstAvailable` subrequests (beta in 1.33, `DRAPrioritizedList`) —
//!     tried in the order they're listed; the first one this node can fully
//!     satisfy wins, matching upstream's `allocateOne` subrequest loop.
//!   * `adminAccess` — an admin-access request is never excluded by another
//!     request's picks (upstream's `allocatingDevices`/`allocatedDevices`
//!     checks are both skipped for it) and never itself excludes a later
//!     request's pick — matching upstream's `allocateDevice`, which simply
//!     never marks an admin-access device "in use".
//!   * `allocationMode: All` — every device this node has that matches the
//!     request's selectors, not a fixed count. A node with zero matches
//!     fails the request the same way `ExactCount` with `count: 0` would,
//!     matching upstream's own "at least one device is required" check.
//!   * `DeviceClaim.constraints` (`matchAttribute`) — devices picked for the
//!     requests a constraint names (or every request, if it names none)
//!     must all report the same value for that attribute; the first device
//!     picked establishes the value, everything after it must match.
//!   * `ResourceSlice.nodeSelector` (a real `v1.NodeSelector`, evaluated the
//!     same way `NodeAffinity` evaluates a pod's own) and
//!     `perDeviceNodeSelection` (each device names its own
//!     `nodeName`/`nodeSelector`/`allNodes` instead of the whole slice
//!     sharing one) — see `cache/snapshot.rs`'s `resource_slices_for_node`
//!     and this file's `candidates_for_node`.
//!
//! One real, documented algorithmic divergence: upstream's allocator is a
//! full backtracking search (`allocateOne` tries a device, recurses, and
//! rolls back and tries another if the recursion fails) so that an earlier
//! request's suboptimal pick never dooms a later one. `allocate_on_node`
//! here is a single greedy forward pass — each request picks the first
//! selectable, unexcluded, constraint-satisfying devices it finds and never
//! reconsiders them. This is exact for the overwhelmingly common shapes (one
//! request, or several requests with disjoint device pools) and can, in
//! principle, report "no fit" for a claim a full backtracking search would
//! find a solution for when multiple requests compete for the same narrow
//! pool under a shared constraint. It never does the reverse — accepts
//! nothing a real allocation wouldn't be valid for.
//!
//! # The CEL environment this exposes, and where it diverges from upstream
//!
//! A selector's `device` variable carries `driver` (string), `attributes`
//! (map from domain to map from name to bool/int/string — a `version`
//! attribute is exposed as its string form), and `capacity` (map from
//! domain to map from name to a plain `f64` in base units, but see below).
//! Unprefixed attribute keys are exposed under both the empty domain and the
//! device's own driver's domain, matching the common case
//! (`device.attributes["dra.example.com"].foo` for an attribute the driver
//! wrote as bare `foo`).
//!
//! Upstream's own CEL environment instead types `capacity` map values (and
//! the `quantity(str)` function's return) as a custom `apiservercel`
//! `Quantity` — an arbitrary-precision value with its own `isGreaterThan`/
//! `isLessThan`/`compareTo`/`add`/`sub`/`sign`/`isInteger`/`asInteger`/
//! `asApproximateFloat` methods. `cel-interpreter`'s `Value` is a closed enum
//! with no opaque/custom-type variant — there is no way to add a real
//! `Quantity` type without forking it, so `quantity.rs` registers those same
//! method/function names operating on `f64` instead: a `quantity("10Gi")`
//! parses to a plain float in base units the same way `device.capacity`
//! entries already are, and every method above is implemented against that
//! float. Every selector expression real DRA drivers write against capacity
//! evaluates correctly under this — the divergence is precision only
//! (`f64`'s ~15-17 significant decimal digits vs upstream's exact rational
//! arithmetic), which no real device capacity value gets close to.
//!
//! # Why the actual device picks happen in `PreFilter`, not `Filter`
//!
//! Same reason as `VolumeBinding` (see that plugin's module header) —
//! `FilterPlugin::filter` is not handed the `Snapshot`. Unlike
//! `VolumeBinding`, this can't be collapsed to "resolve once, check
//! everywhere" — which devices are free genuinely differs per node. So
//! `PreFilter` computes the *whole* per-node candidate table once (a single
//! pass over every node's `ResourceSlice`s), and `Filter` does nothing but
//! look up whether the node it was handed has an entry.
//!
//! # Why this plugin needs its own assume cache, and `VolumeBinding` didn't
//!
//! Two different pods scheduled back to back, before either's binding cycle
//! has written anything to the apiserver, must never be handed the same
//! physical device — a real scarce-resource race `VolumeBinding` never has
//! (dynamic provisioning always makes a *new* volume; there is no shared
//! pool to double-book). `Reserve` therefore tentatively marks the devices
//! it picked as used, `Unreserve`/`PostBind` release the mark, and
//! `PreFilter`'s per-node candidate table excludes both this in-memory mark
//! and every device a real `ResourceClaim.status.allocation` already claims
//! — the same shape `cache/assume.rs` uses for node capacity, kept local to
//! this plugin because nothing else needs it.

use crate::cache::{
    NodeInfo, PodClaimRef, PodInfo, RawDeviceClass, RawDeviceRequestAllocationResult,
    RawResourceClaim, Snapshot,
};
use crate::events::{ActionType, ClusterEvent, EventResource};
use crate::framework::status::Status;
use crate::framework::{
    ClusterEventWithHint, CycleState, FilterPlugin, Plugin, PreEnqueuePlugin, PreFilterPlugin,
};
use k8s_openapi::api::core::v1::{NodeSelector, NodeSelectorRequirement, NodeSelectorTerm};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const NAME: &str = "DynamicResources";

/// A device's identity: which driver, which pool, which named device within
/// it — the same triple `RawDeviceRequestAllocationResult` records, and the
/// key both the allocated-elsewhere check and the assume cache use.
type DeviceId = (String, String, String);

fn device_id(r: &RawDeviceRequestAllocationResult) -> DeviceId {
    (r.driver.clone(), r.pool.clone(), r.device.clone())
}

/// One claim's resolution, computed once in `PreFilter`.
enum ClaimPlan {
    /// Already allocated. `needs_reservation` is `false` when this pod is
    /// already listed in `reservedFor` (nothing to write in Reserve/PreBind)
    /// and `true` when it still needs adding. Either way `node_selector` —
    /// the *existing* allocation's own topology constraint, if any — still
    /// has to be honoured by Filter and (if every node fails it) is what
    /// `PostFilter` can free: an already-reserved-for-this-pod claim on an
    /// unreachable topology is exactly as stuck as one that still needs the
    /// reservation write, so both must carry it, not just the latter.
    Reserved { claim_key: String, needs_reservation: bool, node_selector: Option<Box<NodeSelector>> },
    /// Unallocated. `by_node` is only ever `Some` for a node that can
    /// satisfy every request in the claim.
    Allocate { claim_key: String, by_node: HashMap<String, Vec<RawDeviceRequestAllocationResult>> },
}

struct WantedClaims(Vec<ClaimPlan>);

/// What `Reserve` assumed for one pod, so `PreBind`/`PostBind`/`Unreserve`
/// (none of which see `CycleState`) can find it again by `pod.uid`.
#[derive(Clone)]
struct AssumedClaim {
    claim_key: String,
    /// Empty for `AddReservation` — nothing new was allocated, only a
    /// reservation needs writing.
    new_devices: Vec<RawDeviceRequestAllocationResult>,
}

/// `Clone`, deliberately: `Registry` boxes a separate instance per extension
/// point (the same pattern every other multi-point plugin in this directory
/// uses — `PodTopologySpread` appears four times, for example), but this
/// plugin's assume cache has to be the *same* one everywhere it appears —
/// `PreFilter` reads it, `Reserve` writes it, `Unreserve`/`PostBind` clear
/// it, `PreBind` reads it again. `default_registry` constructs one instance
/// and clones it into every vec it belongs to; the clone is two `Arc` bumps
/// and a `kube::Client` handle, not a copy of the state itself.
#[derive(Clone)]
pub struct DynamicResources {
    client: kube::Client,
    assumed_devices: Arc<Mutex<HashSet<DeviceId>>>,
    assumed_by_pod: Arc<Mutex<HashMap<String, Vec<AssumedClaim>>>>,
}

impl DynamicResources {
    pub fn new(client: kube::Client) -> Self {
        Self {
            client,
            assumed_devices: Arc::new(Mutex::new(HashSet::new())),
            assumed_by_pod: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Plugin for DynamicResources {
    fn name(&self) -> &'static str {
        NAME
    }

    fn events_to_register(&self) -> Vec<ClusterEventWithHint> {
        events_impl()
    }
}

/// The pure half of `events_to_register` — see `pre_filter_impl`'s doc
/// comment (`volume_binding.rs` established the pattern this mirrors): no
/// `&self`, so no `kube::Client` needed to exercise it directly in a test.
fn events_impl() -> Vec<ClusterEventWithHint> {
    vec![
        ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::ResourceClaim,
            ActionType::ADD | ActionType::UPDATE,
        )),
        ClusterEventWithHint::always(ClusterEvent::new(
            EventResource::ResourceSlice,
            ActionType::ADD | ActionType::UPDATE | ActionType::DELETE,
        )),
        ClusterEventWithHint::always(ClusterEvent::new(EventResource::DeviceClass, ActionType::ADD)),
        // A claim reservation freeing up (the pod that held it is gone) is
        // the ordinary way a device shortage resolves.
        ClusterEventWithHint::always(ClusterEvent::new(EventResource::AssignedPod, ActionType::DELETE)),
    ]
}

impl PreEnqueuePlugin for DynamicResources {
    /// The resource-claim controller (part of this project's stripped
    /// kube-controller-manager, same as upstream) materialises a
    /// template-based pod-claim asynchronously. A pod referencing one that
    /// has not shown up in `status.resourceClaimStatuses` yet has not failed
    /// scheduling — it has not reached scheduling, the same reasoning
    /// `SchedulingGates` documents for its own hold.
    fn pre_enqueue(&self, pod: &PodInfo) -> Status {
        pre_enqueue_impl(pod)
    }
}

fn pre_enqueue_impl(pod: &PodInfo) -> Status {
    for pc in &pod.resource_claims {
        if pc.resource_claim_name.is_none()
            && pc.resource_claim_template_name.is_some()
            && pc.object_name(&pod.resource_claim_statuses).is_none()
        {
            return Status::unschedulable(
                NAME,
                format!("waiting for a generated ResourceClaim for {:?}", pc.name),
            );
        }
    }
    Status::success()
}

/// The CEL `device` variable's shape. `#[derive(Serialize)]` is enough to
/// hand this straight to `cel_interpreter::Context::add_variable` — see the
/// crate's blanket `TryIntoValue for T: Serialize`.
#[derive(serde::Serialize)]
struct CelDevice {
    driver: String,
    attributes: BTreeMap<String, BTreeMap<String, CelAttr>>,
    capacity: BTreeMap<String, BTreeMap<String, f64>>,
}

#[derive(serde::Serialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
enum CelAttr {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// Read one attribute's value off a device by its (possibly unqualified)
/// name — the same lookup `matchAttributeConstraint` needs, and the same
/// "fully-qualified match, else bare name in the device's own driver's
/// domain" rule `split_qualified`/`cel_device` already establish for the CEL
/// environment. Mirrors upstream's `lookupAttribute`.
fn lookup_attribute(basic: &crate::cache::dra::RawBasicDevice, driver: &str, qualified_name: &str) -> Option<CelAttr> {
    let attrs = basic.attributes.as_ref()?;
    let raw = if let Some(a) = attrs.get(qualified_name) {
        a
    } else {
        let (domain, name) = qualified_name.split_once('/')?;
        if domain != driver {
            return None;
        }
        attrs.get(name)?
    };
    if let Some(b) = raw.bool {
        Some(CelAttr::Bool(b))
    } else if let Some(i) = raw.int {
        Some(CelAttr::Int(i))
    } else {
        raw.string.as_ref().or(raw.version.as_ref()).map(|s| CelAttr::Str(s.clone()))
    }
}

/// `"domain/name"` splits on the first `/`; a bare key belongs to the
/// device's own driver's domain.
fn split_qualified<'a>(key: &'a str, own_driver: &str) -> (String, &'a str) {
    match key.split_once('/') {
        Some((domain, name)) => (domain.to_string(), name),
        None => (own_driver.to_string(), key),
    }
}

fn cel_device(driver: &str, basic: &crate::cache::dra::RawBasicDevice) -> CelDevice {
    let mut attributes: BTreeMap<String, BTreeMap<String, CelAttr>> = BTreeMap::new();
    for (key, attr) in basic.attributes.iter().flatten() {
        let (domain, name) = split_qualified(key, driver);
        let value = if let Some(b) = attr.bool {
            CelAttr::Bool(b)
        } else if let Some(i) = attr.int {
            CelAttr::Int(i)
        } else if let Some(s) = attr.string.as_ref().or(attr.version.as_ref()) {
            CelAttr::Str(s.clone())
        } else {
            continue;
        };
        attributes.entry(domain).or_default().insert(name.to_string(), value);
    }
    let mut capacity: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for (key, cap) in basic.capacity.iter().flatten() {
        let (domain, name) = split_qualified(key, driver);
        // The exact parsed value, not `parse_quantity`'s `.ceil()`-to-i64 —
        // that rounding is right for a whole-unit resource ledger
        // (millicores, bytes) but would silently corrupt a fractional
        // device capacity (bandwidth in GB/s, say) before any selector ever
        // saw it.
        let value = crate::cache::pod::parse_quantity_f64(&cap.value.0).unwrap_or(0.0);
        capacity.entry(domain).or_default().insert(name.to_string(), value);
    }
    CelDevice { driver: driver.to_string(), attributes, capacity }
}

/// Whether a device satisfies every one of a list of CEL selectors. Fails
/// closed: a selector that does not parse, does not bind, or does not
/// evaluate to a plain `true` excludes the device rather than admitting it —
/// the same posture `LegacyVolumeId`'s unknown-operator branch and this
/// crate's other "malformed input" cases take throughout.
fn device_matches(
    selectors: &[crate::cache::dra::RawDeviceSelector],
    driver: &str,
    basic: &crate::cache::dra::RawBasicDevice,
) -> bool {
    for sel in selectors {
        let Some(cel) = &sel.cel else { continue };
        let Ok(program) = cel_interpreter::Program::compile(&cel.expression) else {
            return false;
        };
        let mut ctx = cel_interpreter::Context::default();
        crate::framework::plugins::quantity::install(&mut ctx);
        if ctx.add_variable("device", cel_device(driver, basic)).is_err() {
            return false;
        }
        match program.execute(&ctx) {
            Ok(cel_interpreter::Value::Bool(true)) => {}
            _ => return false,
        }
    }
    true
}

/// One candidate device, flattened out of its `ResourceSlice`.
struct Candidate {
    driver: String,
    pool: String,
    name: String,
    basic: crate::cache::dra::RawBasicDevice,
}

/// `resource_slices_for_node` already resolved slice-level node
/// applicability (`nodeName`/`allNodes`/`nodeSelector`); a
/// `perDeviceNodeSelection: true` slice defers that decision to each device,
/// resolved here.
fn candidates_for_node(snapshot: &Snapshot, node: &NodeInfo) -> Vec<Candidate> {
    snapshot
        .resource_slices_for_node(node)
        .flat_map(|slice| {
            let per_device = slice.spec.per_device_node_selection == Some(true);
            slice.spec.devices.iter().flatten().filter_map(move |d| {
                if per_device && !device_applies_to_node(&d.basic, node) {
                    return None;
                }
                Some(Candidate {
                    driver: slice.spec.driver.clone(),
                    pool: slice.spec.pool.name.clone(),
                    name: d.name.clone(),
                    basic: d.basic.clone(),
                })
            })
        })
        .collect()
}

/// A `perDeviceNodeSelection` device's own applicability check — mirrors the
/// `ptr.Deref(slice.Spec.PerDeviceNodeSelection, false)` branch in
/// upstream's `isSelectable`. Real API validation requires exactly one of
/// the three to be set; none set is therefore malformed input, and (as
/// everywhere else in this file) fails closed rather than admitting the
/// device.
fn device_applies_to_node(basic: &crate::cache::dra::RawBasicDevice, node: &NodeInfo) -> bool {
    if basic.all_nodes == Some(true) {
        return true;
    }
    if let Some(name) = &basic.node_name {
        return name == &node.name;
    }
    if let Some(sel) = &basic.node_selector {
        return crate::framework::plugins::node_affinity::matches_node_selector(sel, node);
    }
    false
}

/// One request's fully-resolved shape, whether it came from `exactly` or one
/// alternative of a `firstAvailable` list — the same flattening upstream's
/// `requestAccessor` interface gives `deviceRequestAccessor`/
/// `deviceSubRequestAccessor`.
struct EffectiveRequest {
    /// `"req"` for a plain request, `"req/sub"` for a `firstAvailable`
    /// alternative — the exact string upstream records in
    /// `DeviceRequestAllocationResult.Request`.
    result_name: String,
    device_class_name: String,
    selectors: Vec<crate::cache::dra::RawDeviceSelector>,
    all_devices: bool,
    count: usize,
    admin_access: bool,
}

/// Every alternative to try for one top-level request, in order — one entry
/// for a plain `exactly` request, one entry per subrequest for a
/// `firstAvailable` one. `None` means the request is malformed (neither arm
/// set, or a device class name missing) and the claim cannot be allocated at
/// all, on this node or any other.
fn effective_requests(req: &crate::cache::dra::RawDeviceRequest) -> Option<Vec<EffectiveRequest>> {
    if let Some(exactly) = &req.exactly {
        let device_class_name = exactly.device_class_name.clone()?;
        let all_devices = matches!(exactly.allocation_mode.as_deref(), Some("All"));
        if !all_devices && !matches!(exactly.allocation_mode.as_deref(), None | Some("ExactCount")) {
            return None; // an allocationMode this crate doesn't recognize at all
        }
        return Some(vec![EffectiveRequest {
            result_name: req.name.clone(),
            device_class_name,
            selectors: exactly.selectors.clone().unwrap_or_default(),
            all_devices,
            count: if all_devices { 0 } else { exactly.count.unwrap_or(1).max(0) as usize },
            admin_access: exactly.admin_access == Some(true),
        }]);
    }
    if let Some(subs) = &req.first_available {
        if subs.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(subs.len());
        for sub in subs {
            let device_class_name = sub.device_class_name.clone()?;
            let all_devices = matches!(sub.allocation_mode.as_deref(), Some("All"));
            if !all_devices && !matches!(sub.allocation_mode.as_deref(), None | Some("ExactCount")) {
                return None;
            }
            out.push(EffectiveRequest {
                result_name: format!("{}/{}", req.name, sub.name),
                device_class_name,
                selectors: sub.selectors.clone().unwrap_or_default(),
                all_devices,
                count: if all_devices { 0 } else { sub.count.unwrap_or(1).max(0) as usize },
                // Subrequests can never carry adminAccess — see
                // `RawDeviceSubRequest`'s doc comment.
                admin_access: false,
            });
        }
        return Some(out);
    }
    None // neither `exactly` nor `firstAvailable` — not valid upstream either
}

/// Running state for one `matchAttribute` constraint across the devices
/// picked so far for this claim on this node — the value the first covered
/// device established, and how many devices are currently "in" the group
/// (upstream only needs the count; we only need the value itself, since
/// there's no backtracking `remove` to support here).
struct ConstraintState {
    constraint: crate::cache::dra::RawDeviceConstraint,
    value: Option<CelAttr>,
}

/// Whether `constraint` applies to `result_name` — upstream's
/// `matchAttributeConstraint.matches`, ported directly: an empty `requests`
/// list means every request, otherwise the plain request name or the
/// `"request/subrequest"` form must be listed.
fn constraint_applies(requests: &[String], result_name: &str) -> bool {
    requests.is_empty()
        || requests.iter().any(|r| r == result_name || result_name.starts_with(&format!("{r}/")))
}

/// Whether picking `driver`/`basic` for `result_name` is still consistent
/// with every constraint it's covered by — and, if so, records it (a
/// constraint with no established value yet takes whatever this device
/// offers, same as upstream's "first device in the set can always be
/// picked"). Devices this claim has already committed to are assumed
/// consistent — nothing here un-registers a earlier pick, matching this
/// file's single-forward-pass design (see the module header).
fn constraint_allows(
    states: &mut [ConstraintState],
    result_name: &str,
    driver: &str,
    basic: &crate::cache::dra::RawBasicDevice,
) -> bool {
    for state in states.iter_mut() {
        let Some(attr_name) = &state.constraint.match_attribute else { continue };
        if !constraint_applies(&state.constraint.requests, result_name) {
            continue;
        }
        let Some(value) = lookup_attribute(basic, driver, attr_name) else {
            return false; // doesn't carry the attribute at all
        };
        match &state.value {
            None => state.value = Some(value),
            Some(existing) if *existing == value => {}
            Some(_) => return false,
        }
    }
    true
}

/// Try to satisfy every request in a claim against one node's candidate
/// devices, given the identities already spoken for. `None` means this node
/// cannot satisfy the claim at all. See the module header for the one
/// deliberate divergence from upstream (a single greedy forward pass, no
/// backtracking).
fn allocate_on_node(
    requests: &[crate::cache::dra::RawDeviceRequest],
    constraints: &[crate::cache::dra::RawDeviceConstraint],
    device_classes: &HashMap<&str, &RawDeviceClass>,
    candidates: &[Candidate],
    excluded: &HashSet<DeviceId>,
) -> Option<Vec<RawDeviceRequestAllocationResult>> {
    let mut picked: HashSet<DeviceId> = HashSet::new();
    let mut out = Vec::new();
    let mut constraint_states: Vec<ConstraintState> =
        constraints.iter().map(|c| ConstraintState { constraint: c.clone(), value: None }).collect();

    for req in requests {
        let alternatives = effective_requests(req)?;
        let mut satisfied = false;

        'alternatives: for eff in &alternatives {
            let Some(class) = device_classes.get(eff.device_class_name.as_str()) else { continue };
            let mut selectors = eff.selectors.clone();
            selectors.extend(class.spec.selectors.clone().unwrap_or_default());

            // Trying an alternative must not leave partial state behind if
            // it fails partway — clone the constraint values so a failed
            // attempt can be discarded rather than corrupting the next
            // alternative's (or the next request's) view.
            let mut trial_states: Vec<ConstraintState> = constraint_states
                .iter()
                .map(|s| ConstraintState { constraint: s.constraint.clone(), value: s.value.clone() })
                .collect();
            let mut trial_picked = picked.clone();
            let mut found = Vec::new();

            // A request for exactly zero devices (not valid upstream — API
            // validation requires count >= 1 — but defensively handled the
            // same safe way as any other malformed-but-parseable input)
            // needs no candidate search at all: it is trivially satisfied.
            if !eff.all_devices && eff.count == 0 {
                picked = trial_picked;
                satisfied = true;
                break 'alternatives;
            }

            for c in candidates {
                let id = (c.driver.clone(), c.pool.clone(), c.name.clone());
                if !eff.admin_access && (excluded.contains(&id) || trial_picked.contains(&id)) {
                    continue;
                }
                if !device_matches(&selectors, &c.driver, &c.basic) {
                    continue;
                }
                if !constraint_allows(&mut trial_states, &eff.result_name, &c.driver, &c.basic) {
                    continue;
                }
                if !eff.admin_access {
                    trial_picked.insert(id);
                }
                found.push(RawDeviceRequestAllocationResult {
                    request: eff.result_name.clone(),
                    driver: c.driver.clone(),
                    pool: c.pool.clone(),
                    device: c.name.clone(),
                    admin_access: eff.admin_access,
                });
                if !eff.all_devices && found.len() == eff.count {
                    break;
                }
            }

            let got_enough = if eff.all_devices { !found.is_empty() } else { found.len() == eff.count };
            if !got_enough {
                continue 'alternatives; // this alternative doesn't fit; try the next
            }

            picked = trial_picked;
            constraint_states = trial_states;
            out.extend(found);
            satisfied = true;
            break 'alternatives;
        }

        if !satisfied {
            return None;
        }
    }
    Some(out)
}

impl PreFilterPlugin for DynamicResources {
    fn pre_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
    ) -> (Status, Option<Vec<String>>) {
        let excluded = self.assumed_devices.lock().unwrap().clone();
        pre_filter_impl(state, pod, snapshot, &excluded)
    }
}

/// The pure half of `PreFilter` — everything except the assume-cache
/// snapshot, which the trait impl above takes under lock and hands in as a
/// plain `&HashSet`. Mirrors `volume_binding.rs`'s `pre_filter_impl` split;
/// unlike that one, this plugin's PreFilter genuinely reads `&self` state
/// (the assume cache), so the split takes one extra parameter instead of
/// none — still enough to test every allocation case with no `kube::Client`.
fn pre_filter_impl(
    state: &mut CycleState,
    pod: &PodInfo,
    snapshot: &Snapshot,
    excluded: &HashSet<DeviceId>,
) -> (Status, Option<Vec<String>>) {
    if pod.resource_claims.is_empty() {
        state.skip_filter(NAME);
        return (Status::skip(), None);
    }

    let device_classes: HashMap<&str, &RawDeviceClass> =
        snapshot.device_classes.iter().map(|(k, v)| (k.as_str(), v)).collect();

        let mut plans = Vec::new();
        for pc in &pod.resource_claims {
            let Some(claim_name) = pc.object_name(&pod.resource_claim_statuses) else {
                // PreEnqueue already gates this; a claim missing here despite
                // that means it was deleted after admission — the pod will
                // sit unschedulable until a new one is generated.
                return (
                    Status::unschedulable(NAME, format!("resourceclaim for {:?} does not exist", pc.name)),
                    None,
                );
            };
            let Some(claim) = snapshot.resource_claim(&pod.namespace, &claim_name) else {
                return (
                    Status::unschedulable(NAME, format!("resourceclaim {claim_name:?} not found")),
                    None,
                );
            };
            let claim_key = claim.key();

            if claim.bound() {
                let already_reserved = claim
                    .status
                    .as_ref()
                    .and_then(|s| s.reserved_for.as_ref())
                    .into_iter()
                    .flatten()
                    .any(|r| r.resource == "pods" && r.name == pod.name && r.uid == pod.uid);
                let node_selector = claim
                    .status
                    .as_ref()
                    .and_then(|s| s.allocation.as_ref())
                    .and_then(|a| a.node_selector.clone())
                    .map(Box::new);
                plans.push(ClaimPlan::Reserved { claim_key, needs_reservation: !already_reserved, node_selector });
                continue;
            }

            let requests = claim
                .spec
                .devices
                .as_ref()
                .and_then(|d| d.requests.clone())
                .unwrap_or_default();
            if requests.iter().any(|r| effective_requests(r).is_none()) {
                return (
                    Status::unresolvable(
                        NAME,
                        format!("resourceclaim {claim_name:?} has a malformed device request (neither exactly nor firstAvailable set, a missing deviceClassName, or an unrecognized allocationMode)"),
                    ),
                    None,
                );
            }
            let constraints = claim.spec.devices.as_ref().and_then(|d| d.constraints.clone()).unwrap_or_default();

            let mut by_node = HashMap::new();
            for node in snapshot.nodes() {
                let candidates = candidates_for_node(snapshot, node);
                if let Some(devices) =
                    allocate_on_node(&requests, &constraints, &device_classes, &candidates, excluded)
                {
                    by_node.insert(node.name.clone(), devices);
                }
            }
            plans.push(ClaimPlan::Allocate { claim_key, by_node });
        }

    state.write(NAME, WantedClaims(plans));
    (Status::success(), None)
}

impl FilterPlugin for DynamicResources {
    fn filter(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Status {
        filter_impl(state, pod, node)
    }
}

/// The pure half of `Filter` — no `&self` access at all, unlike `PreFilter`.
fn filter_impl(state: &CycleState, _pod: &PodInfo, node: &NodeInfo) -> Status {
    let Some(wanted) = state.read::<WantedClaims>(NAME) else {
        return Status::success();
    };
    for plan in &wanted.0 {
        match plan {
            ClaimPlan::Reserved { node_selector, .. } => {
                if let Some(sel) = node_selector {
                    if !crate::framework::plugins::node_affinity::matches_node_selector(sel, node) {
                        return Status::unresolvable(NAME, "node(s) not in the claim's allocated topology");
                    }
                }
            }
            ClaimPlan::Allocate { by_node, .. } => {
                if !by_node.contains_key(&node.name) {
                    return Status::unschedulable(NAME, "node(s) did not have available devices for a resource claim");
                }
            }
        }
    }
    Status::success()
}

/// The pure half of `PostFilter`: which claim (if any) should be
/// deallocated to help this pod. `None` means nothing here can help —
/// either no claim is stuck, or a stuck one still reserves the device for
/// someone else, and freeing it would break them.
///
/// Matches upstream's real `DynamicResources.PostFilter` with one
/// structural difference: upstream tracks "unavailable claims" as a
/// byproduct of its own Filter loop; this reruns the same node-selector
/// check `filter_impl` already made, since `CycleState` doesn't carry that
/// byproduct here.
fn post_filter_impl(wanted: &WantedClaims, pod: &PodInfo, snapshot: &Snapshot) -> Option<(String, String)> {
    for plan in &wanted.0 {
        let ClaimPlan::Reserved { claim_key, node_selector: Some(sel), .. } = plan else { continue };
        let unavailable_everywhere =
            snapshot.nodes().iter().all(|n| !crate::framework::plugins::node_affinity::matches_node_selector(sel, n));
        if !unavailable_everywhere {
            continue;
        }
        let Some((namespace, name)) = claim_key.split_once('/') else { continue };
        let Some(claim) = snapshot.resource_claim(namespace, name) else { continue };
        let reserved_for = claim.status.as_ref().and_then(|s| s.reserved_for.as_ref());
        let only_us = reserved_for.is_none_or(|r| {
            r.is_empty() || (r.len() == 1 && r[0].resource == "pods" && r[0].name == pod.name && r[0].uid == pod.uid)
        });
        if !only_us {
            continue; // someone else still needs this allocation; freeing it would break them
        }
        return Some((namespace.to_string(), name.to_string()));
    }
    None
}

#[async_trait::async_trait]
impl crate::framework::PostFilterPlugin for DynamicResources {
    async fn post_filter(
        &self,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
        _node_statuses: &crate::framework::status::NodeToStatus,
    ) -> (Status, Option<String>) {
        let Some(wanted) = state.read::<WantedClaims>(NAME) else {
            return (Status::unschedulable(NAME, "no claims to deallocate"), None);
        };
        let Some((namespace, name)) = post_filter_impl(&wanted, pod, snapshot) else {
            return (Status::unschedulable(NAME, "still not schedulable"), None);
        };

        let body = serde_json::json!({ "status": { "allocation": null, "reservedFor": null } });
        if let Err(status) = raw_patch_claim_status(&self.client, &namespace, &name, body).await {
            return (status, None);
        }
        (Status::unschedulable(NAME, "deallocation of ResourceClaim completed"), None)
    }
}

impl crate::framework::ReservePlugin for DynamicResources {
    /// Turn this cycle's `WantedClaims` into concrete assumptions for the
    /// one node that won — see the module header's "assume cache" section.
    fn reserve(&self, state: &mut CycleState, pod: &PodInfo, node: &str) -> Status {
        let Some(wanted) = state.read::<WantedClaims>(NAME) else {
            return Status::success();
        };

        let mut assumed = Vec::new();
        for plan in &wanted.0 {
            match plan {
                // Always assumed, even when `needs_reservation` is false:
                // `commit_claim` re-checks the *fresh* claim before writing
                // and no-ops the reservedFor append if it's already there,
                // so this is idempotent — and it must run either way, or
                // PreBind (which only iterates entries the assume cache
                // remembered) never confirms the write for a
                // no-longer-quite-so-nothing-to-do claim.
                ClaimPlan::Reserved { claim_key, .. } => {
                    assumed.push(AssumedClaim { claim_key: claim_key.clone(), new_devices: Vec::new() });
                }
                ClaimPlan::Allocate { claim_key, by_node } => {
                    // Filter already checked this node has an entry; a miss
                    // here means a plugin bug, not a scheduling outcome.
                    let Some(devices) = by_node.get(node) else {
                        return Status::error(
                            NAME,
                            format!("internal: Filter approved node {node} with no computed allocation for {claim_key}"),
                        );
                    };
                    assumed.push(AssumedClaim { claim_key: claim_key.clone(), new_devices: devices.clone() });
                }
            }
        }

        {
            let mut devices = self.assumed_devices.lock().unwrap();
            for a in &assumed {
                // An admin-access pick is never exclusive — see
                // `allocate_on_node`'s doc comment — so it must not enter
                // the assume cache either, or a later cycle's `excluded`
                // would wrongly treat it as spoken for.
                for d in a.new_devices.iter().filter(|d| !d.admin_access) {
                    devices.insert(device_id(d));
                }
            }
        }
        self.assumed_by_pod.lock().unwrap().insert(pod.uid.clone(), assumed);
        Status::success()
    }

    /// Must not fail and must be idempotent — see the trait's own doc
    /// comment. `remove` on a uid with nothing assumed is simply a no-op.
    fn unreserve(&self, _state: &mut CycleState, pod: &PodInfo, _node: &str) {
        self.release(&pod.uid);
    }
}

impl DynamicResources {
    /// Drop this pod's tentative device assumptions — called on any failure
    /// after `Reserve` (`Unreserve`) and on a successful bind (`PostBind`,
    /// once the real write has already landed, so nothing is lost by
    /// forgetting the local mark early — see the module header).
    fn release(&self, pod_uid: &str) {
        if let Some(assumed) = self.assumed_by_pod.lock().unwrap().remove(pod_uid) {
            let mut devices = self.assumed_devices.lock().unwrap();
            for a in &assumed {
                for d in &a.new_devices {
                    devices.remove(&device_id(d));
                }
            }
        }
    }

    async fn commit_claim(&self, pod: &PodInfo, node: &str, assumed: &AssumedClaim) -> Result<(), Status> {
        let (namespace, name) = assumed
            .claim_key
            .split_once('/')
            .ok_or_else(|| Status::error(NAME, format!("malformed claim key {:?}", assumed.claim_key)))?;

        let current = raw_get_claim(&self.client, namespace, name).await?;

        let mut reserved_for =
            current.status.as_ref().and_then(|s| s.reserved_for.clone()).unwrap_or_default();
        let already_reserved =
            reserved_for.iter().any(|r| r.resource == "pods" && r.name == pod.name && r.uid == pod.uid);
        if !already_reserved {
            reserved_for.push(crate::cache::dra::RawConsumerReference {
                api_group: None,
                resource: "pods".to_string(),
                name: pod.name.clone(),
                uid: pod.uid.clone(),
            });
        }

        let allocation = match current.status.as_ref().and_then(|s| s.allocation.clone()) {
            // Already allocated (by us or another scheduler instance that
            // raced us) — only the reservation needed adding.
            Some(existing) => existing,
            None => crate::cache::dra::RawAllocationResult {
                devices: Some(crate::cache::dra::RawDeviceAllocationResult {
                    results: Some(assumed.new_devices.clone()),
                }),
                node_selector: Some(single_node_selector(node)),
            },
        };

        let body = serde_json::json!({
            "status": {
                "allocation": allocation,
                "reservedFor": reserved_for,
            }
        });
        raw_patch_claim_status(&self.client, namespace, name, body).await
    }
}

/// A `NodeSelector` that matches exactly one node by `kubernetes.io/hostname`
/// — how an allocation ties a claim to the node it was allocated for.
fn single_node_selector(node: &str) -> NodeSelector {
    NodeSelector {
        node_selector_terms: vec![NodeSelectorTerm {
            match_expressions: Some(vec![NodeSelectorRequirement {
                key: "kubernetes.io/hostname".to_string(),
                operator: "In".to_string(),
                values: Some(vec![node.to_string()]),
            }]),
            match_fields: None,
        }],
    }
}

async fn raw_get_claim(client: &kube::Client, namespace: &str, name: &str) -> Result<RawResourceClaim, Status> {
    let req = http::Request::builder()
        .method("GET")
        .uri(format!("/apis/resource.k8s.io/v1/namespaces/{namespace}/resourceclaims/{name}"))
        .body(Vec::new())
        .map_err(|e| Status::error(NAME, format!("building resourceclaim GET: {e}")))?;
    client
        .request(req)
        .await
        .map_err(|e| Status::error(NAME, format!("reading resourceclaim {name:?}: {e}")))
}

async fn raw_patch_claim_status(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    body: serde_json::Value,
) -> Result<(), Status> {
    let bytes = serde_json::to_vec(&body)
        .map_err(|e| Status::error(NAME, format!("encoding resourceclaim status patch: {e}")))?;
    let req = http::Request::builder()
        .method("PATCH")
        .uri(format!(
            "/apis/resource.k8s.io/v1/namespaces/{namespace}/resourceclaims/{name}/status"
        ))
        .header("Content-Type", "application/merge-patch+json")
        .body(bytes)
        .map_err(|e| Status::error(NAME, format!("building resourceclaim status PATCH: {e}")))?;
    client
        .request::<serde_json::Value>(req)
        .await
        .map(|_| ())
        .map_err(|e| Status::error(NAME, format!("patching resourceclaim {name:?} status: {e}")))
}

#[async_trait::async_trait]
impl crate::framework::PreBindPlugin for DynamicResources {
    async fn pre_bind(&self, pod: &PodInfo, node: &str) -> Status {
        let assumed = self.assumed_by_pod.lock().unwrap().get(&pod.uid).cloned().unwrap_or_default();
        for a in &assumed {
            if let Err(status) = self.commit_claim(pod, node, a).await {
                return status;
            }
        }
        Status::success()
    }
}

#[async_trait::async_trait]
impl crate::framework::PostBindPlugin for DynamicResources {
    /// The apiserver write already landed in `PreBind` — this only clears
    /// the local tentative mark. See the module header's assume-cache
    /// section for why that is safe to do immediately rather than waiting
    /// for a watch event to confirm it, unlike `cache::AssumedPods`.
    async fn post_bind(&self, pod: &PodInfo, _node: &str) {
        self.release(&pod.uid);
    }
}

#[cfg(test)]
#[path = "dynamic_resources_tests.rs"]
mod tests;
