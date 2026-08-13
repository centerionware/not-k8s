# lib/test/cases/scheduler.sh — pod placement, against a real cluster.
#
# These tests prove behaviour that unit tests structurally cannot. Every
# scoring formula and filter predicate in crates/nodescheduler is already
# tested against hand-computed numbers with no cluster involved — that is what
# the purity rule in cycle.rs buys. What none of that proves is that a pod
# actually ends up Running on the node the scheduler picked, that the Binding
# subresource POST has the right shape, or that a pod stuck for a real reason
# is woken by a real cluster event rather than sitting until the five-minute
# safety net rescues it.
#
# That last one is the test this file exists for. The whole design rests on
# QueueingHints being complete, and an incomplete one produces no error, no log
# and no failing unit test — just a pod that takes five minutes instead of one
# second. `test_scheduler_wakes_a_pending_pod_on_a_real_event` measures the
# latency, so an incomplete hint fails here loudly instead of shipping.
#
# Skipping: every test self-skips unless nodescheduler is the scheduler this
# cluster is actually running (SCHEDULER=nodescheduler at bootstrap, which also
# passes --disable-scheduler to k3s). On a default deployment k3s's own
# kube-scheduler is placing pods, and asserting our behaviour against it would
# either pass for the wrong reason or fail for one.

# Is our scheduler the one placing pods here?
#
# Deliberately checks the *unit*, not the binary's presence: a node that built
# nodescheduler but is still running k3s's scheduler must skip, or these tests
# would be silently validating upstream instead.
_nodescheduler_is_running() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodescheduler 2>/dev/null && return 0
    fi
    # Fallback tiers (OpenRC, the supervised loop) have no unit to ask, so
    # fall back to the process itself.
    pgrep -x nodescheduler >/dev/null 2>&1
}

_require_nodescheduler() {
    _nodescheduler_is_running \
        || skip_test "nodescheduler isn't running here — deploy with --scheduler=nodescheduler (which also disables k3s's own scheduler) to exercise these"
}

# The node every pod lands on in a single-node deployment. Several tests need
# to name it to build an affinity or a taint.
_the_node() {
    kubectl get nodes -o jsonpath='{.items[0].metadata.name}' 2>/dev/null
}

# ── Placement ───────────────────────────────────────────────────────────

test_scheduler_places_an_ordinary_pod() {
    _require_nodescheduler
    local pod="sched-basic"
    delete_pod_if_exists "$pod"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    # nodeName being set is the scheduler's whole output; Running additionally
    # proves the Binding was one kubelet could act on, rather than merely one
    # the apiserver accepted.
    wait_until 60 pod_field "$pod" '{.spec.nodeName}' \
        || die "pod was never bound to a node — is nodescheduler running and leader?"
    local node
    node="$(pod_field "$pod" '{.spec.nodeName}')"
    assert_not_empty "$node" "the scheduler must record its decision in spec.nodeName"

    wait_until 90 pod_is_phase "$pod" Running \
        || die "pod was bound to $node but never reached Running"

    delete_pod_if_exists "$pod"
}
register_test test_scheduler_places_an_ordinary_pod

test_scheduler_leaves_a_gated_pod_alone() {
    _require_nodescheduler
    local pod="sched-gated"
    delete_pod_if_exists "$pod"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  schedulingGates:
  - name: example.com/hold
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    sleep 10
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "" \
        "a gated pod must not be bound"

    # The quiet part, and the one most likely to regress: a gated pod is not
    # being *rejected* by scheduling, it has not entered scheduling. Writing a
    # PodScheduled=False condition here makes every gated pod look broken to
    # everything watching the cluster.
    assert_eq "$(pod_condition_status "$pod" PodScheduled)" "" \
        "a gated pod must carry no PodScheduled condition at all"

    # Removing the gate must place it — and must do so promptly, via the
    # UPDATE_POD_SCHEDULING_GATES_ELIMINATED subscription rather than the
    # five-minute net.
    kctl patch pod "$pod" --type=json -p '[{"op":"remove","path":"/spec/schedulingGates"}]' >/dev/null

    local start=$SECONDS
    wait_until 60 pod_field "$pod" '{.spec.nodeName}' \
        || die "ungating the pod never got it scheduled"
    local elapsed=$((SECONDS - start))
    [[ "$elapsed" -lt 60 ]] \
        || die "took ${elapsed}s to schedule after ungating — that is the safety net firing, not the gate subscription"

    delete_pod_if_exists "$pod"
}
register_test test_scheduler_leaves_a_gated_pod_alone

test_scheduler_honours_a_node_selector_that_matches_nothing() {
    _require_nodescheduler
    local pod="sched-nosuchnode"
    delete_pod_if_exists "$pod"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  nodeSelector:
    example.com/nonexistent: "true"
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    sleep 10
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "" \
        "a pod whose nodeSelector matches no node must stay unbound"

    # Unlike a gated pod, this one WAS rejected by scheduling, so it must say
    # so — this is the diagnostic a user actually sees.
    assert_eq "$(pod_condition_status "$pod" PodScheduled)" "False" \
        "a genuinely unschedulable pod must report PodScheduled=False"

    delete_pod_if_exists "$pod"
}
register_test test_scheduler_honours_a_node_selector_that_matches_nothing

test_scheduler_honours_a_node_selector_that_matches() {
    _require_nodescheduler
    local pod="sched-selector" node
    node="$(_the_node)"
    assert_not_empty "$node" "needs at least one node"
    delete_pod_if_exists "$pod"

    kubectl label node "$node" example.com/sched-test=yes --overwrite >/dev/null

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  nodeSelector:
    example.com/sched-test: "yes"
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    wait_until 60 pod_field "$pod" '{.spec.nodeName}' \
        || { kubectl label node "$node" example.com/sched-test- >/dev/null 2>&1
             die "a pod selecting a label the node has was never scheduled"; }
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "$node" \
        "the pod must land on the labelled node"

    delete_pod_if_exists "$pod"
    kubectl label node "$node" example.com/sched-test- >/dev/null 2>&1 || true
}
register_test test_scheduler_honours_a_node_selector_that_matches

test_scheduler_rejects_a_pod_that_does_not_fit() {
    _require_nodescheduler
    local pod="sched-toobig"
    delete_pod_if_exists "$pod"

    # Far more CPU than any real node advertises, so this is a resource
    # rejection rather than anything else.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "10000"
EOF

    sleep 10
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "" \
        "a pod requesting more CPU than exists must stay unbound"

    # The message is the deliverable here — "Insufficient cpu" is what tells a
    # user what to change, and it is the exact string upstream produces.
    local events
    events="$(kctl get events --field-selector "involvedObject.name=$pod" -o json 2>/dev/null || true)"
    local reason
    reason="$(pod_json "$pod" | grep -o 'Insufficient cpu' | head -1)"
    [[ -n "$reason" || "$events" == *"Insufficient cpu"* ]] \
        || die "the rejection must name the resource that did not fit (want 'Insufficient cpu')"

    delete_pod_if_exists "$pod"
}
register_test test_scheduler_rejects_a_pod_that_does_not_fit

test_scheduler_respects_a_taint_and_its_toleration() {
    _require_nodescheduler
    local node tainted="sched-tainted" tolerating="sched-tolerating"
    node="$(_the_node)"
    assert_not_empty "$node" "needs at least one node"
    delete_pod_if_exists "$tainted"
    delete_pod_if_exists "$tolerating"

    kubectl taint node "$node" example.com/sched-test=yes:NoSchedule --overwrite >/dev/null

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $tainted
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
---
apiVersion: v1
kind: Pod
metadata:
  name: $tolerating
spec:
  tolerations:
  - key: example.com/sched-test
    operator: Equal
    value: "yes"
    effect: NoSchedule
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    if ! wait_until 60 pod_field "$tolerating" '{.spec.nodeName}'; then
        kubectl taint node "$node" example.com/sched-test- >/dev/null 2>&1 || true
        die "the tolerating pod was never scheduled onto the tainted node"
    fi
    assert_eq "$(pod_field "$tainted" '{.spec.nodeName}')" "" \
        "the pod without a toleration must not be placed on the tainted node"

    # Removing the taint must place the other one, via the
    # Node/UPDATE_NODE_TAINT subscription.
    kubectl taint node "$node" example.com/sched-test- >/dev/null 2>&1 || true

    local start=$SECONDS
    wait_until 60 pod_field "$tainted" '{.spec.nodeName}' \
        || die "removing the taint never got the untolerating pod scheduled"
    local elapsed=$((SECONDS - start))
    [[ "$elapsed" -lt 60 ]] \
        || die "took ${elapsed}s after untainting — the taint subscription is not working; this is the safety net"

    delete_pod_if_exists "$tainted"
    delete_pod_if_exists "$tolerating"
}
register_test test_scheduler_respects_a_taint_and_its_toleration

# ── The event-driven claim ──────────────────────────────────────────────

test_scheduler_wakes_a_pending_pod_on_a_real_event() {
    _require_nodescheduler
    local blocker="sched-blocker" waiter="sched-waiter" node
    node="$(_the_node)"
    assert_not_empty "$node" "needs at least one node"
    delete_pod_if_exists "$blocker"
    delete_pod_if_exists "$waiter"

    # Claim nearly all of the node's allocatable CPU, then ask for the same
    # again. The second pod is unschedulable for a real resource reason, and
    # the ONLY thing that can free it is the first pod going away.
    local allocatable_milli
    allocatable_milli="$(kubectl get node "$node" -o jsonpath='{.status.allocatable.cpu}' 2>/dev/null)"
    assert_not_empty "$allocatable_milli" "could not read the node's allocatable CPU"
    # Normalise "4" or "3800m" to millicores, then take 60% each so two of
    # them cannot both fit but one comfortably does.
    local milli
    case "$allocatable_milli" in
        *m) milli="${allocatable_milli%m}" ;;
        *)  milli=$(( allocatable_milli * 1000 )) ;;
    esac
    local each=$(( milli * 60 / 100 ))
    [[ "$each" -gt 0 ]] || skip_test "node reports no allocatable CPU to work with"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $blocker
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${each}m"
EOF
    wait_until 60 pod_field "$blocker" '{.spec.nodeName}' \
        || die "the blocking pod was never scheduled"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $waiter
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${each}m"
EOF

    sleep 8
    assert_eq "$(pod_field "$waiter" '{.spec.nodeName}')" "" \
        "the second pod should not fit alongside the first"

    # THE measurement. Deleting the blocker emits AssignedPod/DELETE, which
    # NodeResourcesFit subscribes to. If that subscription is missing or the
    # hint is wrong, the pod still gets scheduled — but only after the
    # five-minute unschedulable timeout rescues it. So the assertion is on
    # LATENCY, not on eventual success: a correct implementation is a second
    # or two, a broken one is 300.
    local start=$SECONDS
    delete_pod_if_exists "$blocker"

    wait_until 120 pod_field "$waiter" '{.spec.nodeName}' \
        || die "freeing the CPU never got the waiting pod scheduled at all"
    local elapsed=$((SECONDS - start))

    [[ "$elapsed" -lt 60 ]] || die \
        "took ${elapsed}s to reschedule after capacity was freed. That is the \
5-minute unschedulable-timeout safety net doing the work, not an event — some \
plugin's events_to_register() is incomplete. See crates/nodescheduler/src/queue/hints.rs."

    delete_pod_if_exists "$waiter"
}
register_test test_scheduler_wakes_a_pending_pod_on_a_real_event

# ── Multi-profile ───────────────────────────────────────────────────────

test_scheduler_ignores_pods_for_another_scheduler() {
    _require_nodescheduler
    local pod="sched-other-profile"
    delete_pod_if_exists "$pod"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  schedulerName: some-other-scheduler
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
EOF

    sleep 12
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "" \
        "a pod naming another scheduler must be left entirely alone"
    # And left alone means *untouched* — not rejected. Writing a condition for
    # a pod we do not own would misreport another scheduler's backlog as our
    # failure.
    assert_eq "$(pod_condition_status "$pod" PodScheduled)" "" \
        "a pod belonging to another scheduler must carry no condition from us"

    delete_pod_if_exists "$pod"
}
register_test test_scheduler_ignores_pods_for_another_scheduler

# ── Leadership ──────────────────────────────────────────────────────────

test_scheduler_holds_the_leader_lease() {
    _require_nodescheduler

    # The lease is how a second control-plane node knows not to schedule. If
    # this is absent, a multi-control-plane cluster has no protection against
    # two schedulers both binding, which is the failure this whole mechanism
    # exists to prevent.
    local holder
    holder="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.holderIdentity}' 2>/dev/null || true)"
    assert_not_empty "$holder" \
        "nodescheduler must hold the kube-scheduler lease in kube-system while it is scheduling"

    # It must actually be renewing it, not just have taken it once.
    local first second
    first="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)"
    sleep 8
    second="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)"
    assert_not_eq "$first" "$second" \
        "the lease renewTime must advance — a stale lease means another replica will take over mid-flight"
}
register_test test_scheduler_holds_the_leader_lease
