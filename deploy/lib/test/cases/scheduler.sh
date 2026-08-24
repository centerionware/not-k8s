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

# Has the scheduler bound this pod yet?
#
# wait_until takes <timeout> <description> <command...> and needs a command
# that *exits* 0, not one that prints a value. Passing `pod_field` directly
# silently consumed it as the description and then tried to execute the pod's
# name as a command, so the wait could never succeed and every such test
# failed after its full timeout. The giveaway was the harness reporting
# "waiting for: pod_field" — the description slot echoing back a function
# name.
_pod_is_bound() { # _pod_is_bound <pod>
    [[ -n "$(pod_field "$1" '{.spec.nodeName}')" ]]
}

# The node's allocatable CPU in millicores, whichever spelling it uses.
#
# The apiserver canonicalises quantities, so this comes back as "4", "3800m"
# or even "4k" depending on the value — the same canonicalisation that made a
# 10000-core pod look like it requested nothing (docs/E2E_FINDINGS.md finding
# 21). Only the two forms a node realistically reports are handled here, and
# anything else yields 0 so the caller skips rather than computing a nonsense
# request.
_node_allocatable_milli_cpu() { # _node_allocatable_milli_cpu <node>
    local raw
    raw="$(kubectl get node "$1" -o jsonpath='{.status.allocatable.cpu}' 2>/dev/null)"
    case "$raw" in
        "")      echo 0 ;;
        *m)      echo "${raw%m}" ;;
        *[!0-9]*) echo 0 ;;
        *)       echo $(( raw * 1000 )) ;;
    esac
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
    wait_until 60 "$pod to be bound to a node" _pod_is_bound "$pod" \
        || die "pod was never bound to a node — is nodescheduler running and leader?"
    local node
    node="$(pod_field "$pod" '{.spec.nodeName}')"
    assert_not_empty "$node" "the scheduler must record its decision in spec.nodeName"

    wait_until 90 "$pod to reach Running" pod_is_phase "$pod" Running \
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

    # The quiet part, and the one most likely to regress.
    #
    # A gated pod DOES carry PodScheduled=False — the apiserver sets it at
    # admission with reason SchedulingGated, which is what makes kubectl show
    # the pod's status as "SchedulingGated". An earlier version of this test
    # asserted the condition was absent entirely; that was wrong about
    # upstream, and it failed against a scheduler that was behaving
    # perfectly.
    #
    # What must hold is that the *reason* is not ours. A gated pod has not
    # been rejected by scheduling, it has not entered scheduling, so
    # reason=Unschedulable (which is what report.rs writes) would misreport a
    # controller doing its job as a scheduling failure.
    local reason
    reason="$(pod_field "$pod" '{.status.conditions[?(@.type=="PodScheduled")].reason}')"
    assert_not_eq "$reason" "Unschedulable" \
        "a gated pod must not be reported as a scheduling failure — it never entered scheduling"

    # And no FailedScheduling event, for the same reason: a gated pod must
    # not appear in kubectl describe as something the scheduler rejected.
    local events
    events="$(kctl get events --field-selector "involvedObject.name=$pod,reason=FailedScheduling" -o name 2>/dev/null || true)"
    assert_eq "$events" "" \
        "a gated pod must not get a FailedScheduling event from the scheduler"

    # Removing the gate must place it — and must do so promptly, via the
    # UPDATE_POD_SCHEDULING_GATES_ELIMINATED subscription rather than the
    # five-minute net.
    kctl patch pod "$pod" --type=json -p '[{"op":"remove","path":"/spec/schedulingGates"}]' >/dev/null

    local start=$SECONDS
    wait_until 60 "$pod to be bound to a node" _pod_is_bound "$pod" \
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

    wait_until 60 "$pod to be bound to a node" _pod_is_bound "$pod" \
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
    # Self-diagnosing on failure. The first live run bound this pod and the
    # cause could not be determined from the assertion alone — every unit test
    # of the same path, including one driving a real Pod object through the
    # projection and the cycle, correctly rejects it. So if it happens again,
    # print the two numbers the decision is actually made from rather than
    # prompting another round-trip to find out.
    local bound_to
    bound_to="$(pod_field "$pod" '{.spec.nodeName}')"
    if [[ -n "$bound_to" ]]; then
        warn "pod was bound to '$bound_to' — dumping the inputs the fit check uses:"
        warn "  node allocatable: $(kubectl get node "$bound_to" -o jsonpath='{.status.allocatable}' 2>&1)"
        warn "  pod requests:     $(pod_field "$pod" '{.spec.containers[0].resources.requests}')"
        warn "  pod conditions:   $(pod_field "$pod" '{.status.conditions}')"
    fi
    assert_eq "$bound_to" "" \
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
    # The taint must come off on every exit, including a failed assertion —
    # `die` aborts the function immediately, and a NoSchedule taint left on
    # the cluster's only node would fail every test that runs after this
    # one, turning one failure into a cascade of unrelated ones.
    # EXIT, not RETURN: `die` (used below) calls a hard `exit`, which never
    # triggers a RETURN trap — only EXIT fires on that path. Each test
    # already runs in its own subshell (see harness.sh), so this only tears
    # down that subshell's trap, not the whole suite's.
    trap 'kubectl taint node "'"$node"'" example.com/sched-test- >/dev/null 2>&1 || true' EXIT

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

    wait_until 60 "$tolerating to be bound to a node" _pod_is_bound "$tolerating" \
        || die "the tolerating pod was never scheduled onto the tainted node"
    assert_eq "$(pod_field "$tainted" '{.spec.nodeName}')" "" \
        "the pod without a toleration must not be placed on the tainted node"

    # Removing the taint must place the other one, via the
    # Node/UPDATE_NODE_TAINT subscription. The trap above will try the same
    # untaint again on return, which is a harmless no-op by then.
    kubectl taint node "$node" example.com/sched-test- >/dev/null 2>&1 || true

    local start=$SECONDS
    wait_until 60 "$tainted to be bound to a node" _pod_is_bound "$tainted" \
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
    wait_until 60 "$blocker to be bound to a node" _pod_is_bound "$blocker" \
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
    #
    # Measured from the pod's *actual disappearance*, not from the delete
    # request. Those are not the same moment and the gap is not the
    # scheduler's: `kubectl delete` only sets a deletionTimestamp, and the
    # capacity stays spent until nodelet finishes tearing the pod down and
    # issues the final delete — exactly as upstream kube-scheduler only drops
    # a pod from its cache on the real delete event, never on the timestamp.
    #
    # This distinction is not hypothetical. Timing from the request instead
    # blamed the scheduler for a 77s reschedule that was really 71s of
    # nodelet teardown followed by a 4s reschedule, and the failure message
    # asserted a cause ("the 5-minute safety net") that the number itself
    # contradicts — 77 is neither ~2 nor ~300. Splitting the two windows
    # means each failure names the component that actually owns it.
    local start=$SECONDS
    delete_pod_if_exists "$blocker"

    wait_until 120 "$blocker to actually be gone from the apiserver" \
        bash -c "! kubectl get pod '$blocker' -n '$TEST_NAMESPACE' >/dev/null 2>&1" \
        || die "the blocking pod never actually left the apiserver — that is a node-agent teardown problem, not a scheduling one; check nodelet's logs for 'torn down $blocker'"
    local freed=$SECONDS
    local teardown_s=$((freed - start))

    wait_until 120 "$waiter to be bound to a node" _pod_is_bound "$waiter" \
        || die "freeing the CPU never got the waiting pod scheduled at all"
    local elapsed=$((SECONDS - freed))

    [[ "$elapsed" -lt 60 ]] || die \
        "took ${elapsed}s to schedule after the blocker actually disappeared \
(its teardown itself took ${teardown_s}s, which is not counted here). A correct \
event subscription reschedules in a second or two; 300 would be the \
unschedulable-timeout safety net rescuing it. Either way some plugin's \
events_to_register() is incomplete — see crates/nodescheduler/src/queue/hints.rs."

    # $waiter itself claims ~60% of allocatable — wait for it to actually
    # be gone before returning, or the next test inherits a node that
    # looks emptier than it is (see delete_pod_and_wait_gone's own
    # comment).
    delete_pod_and_wait_gone "$waiter"
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

    # It must actually be renewing it, not just have taken it once. Poll
    # rather than taking one fixed-width sample: a fresh scheduler can be
    # between its initial lease write and the first renewal when this test
    # starts, especially after another control-plane test has just recreated
    # the lease object.
    local first second
    if ! first="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)" || [[ -z "$first" ]]; then
        die "the kube-scheduler lease renewTime could not be read before polling"
    fi
    if ! try_wait_until 30 bash -c "
        current=\$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null) &&
        [[ -n \"\$current\" && \"\$current\" != \"$first\" ]]
    "; then
        second="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)"
        die "the lease renewTime did not advance from '$first' (still '$second') — a stale lease means another replica will take over mid-flight"
    fi
}
register_test test_scheduler_holds_the_leader_lease

# ── Phase 2: topology ───────────────────────────────────────────────────
#
# All three run on a single node, which CI has. Anti-affinity and affinity
# scope to kubernetes.io/hostname, where the domain *is* the node; spread is
# made to bite with minDomains, which forces globalMin to 0 while fewer
# domains exist than the constraint asks for. Without that a one-node cluster
# has globalMin equal to its own count, skew is always 0, and the constraint
# silently passes — which would make this test prove nothing.

test_scheduler_honours_pod_anti_affinity() {
    _require_nodescheduler
    local first="sched-anti-a" second="sched-anti-b"
    delete_pod_if_exists "$first"
    delete_pod_if_exists "$second"

    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $first
  labels:
    sched-test: anti
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML
    wait_until 60 "$first to be bound to a node" _pod_is_bound "$first" \
        || die "the first pod was never scheduled"

    # Requires no pod labelled sched-test=anti on the same host. The first
    # pod is exactly that, so on a single-node cluster there is nowhere left.
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $second
  labels:
    sched-test: anti
spec:
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchLabels:
            sched-test: anti
        topologyKey: kubernetes.io/hostname
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML

    sleep 10
    assert_eq "$(pod_field "$second" '{.spec.nodeName}')" "" \
        "anti-affinity must keep the second pod off the node the first is on"
    assert_eq "$(pod_condition_status "$second" PodScheduled)" "False" \
        "and it must say why, rather than sitting Pending with no explanation"

    # Deleting the blocker must release it promptly — via the AssignedPod
    # DELETE subscription, not the five-minute net.
    local start=$SECONDS
    delete_pod_if_exists "$first"
    wait_until 90 "$second to be bound to a node" _pod_is_bound "$second" \
        || die "removing the conflicting pod never got the anti-affinity pod scheduled"
    local elapsed=$((SECONDS - start))
    [[ "$elapsed" -lt 60 ]] \
        || die "took ${elapsed}s after the conflict was removed — that is the safety net, not the event"

    delete_pod_if_exists "$second"
}
register_test test_scheduler_honours_pod_anti_affinity

test_scheduler_honours_pod_affinity() {
    _require_nodescheduler
    local anchor="sched-affin-anchor" follower="sched-affin-follower"
    delete_pod_if_exists "$anchor"
    delete_pod_if_exists "$follower"

    # The follower requires an anchor pod on the same host. Created first,
    # while no anchor exists, so it must stay Pending — proving the rule is
    # actually evaluated rather than trivially satisfied.
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $follower
spec:
  affinity:
    podAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchLabels:
            sched-test: anchor
        topologyKey: kubernetes.io/hostname
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML

    sleep 10
    assert_eq "$(pod_field "$follower" '{.spec.nodeName}')" "" \
        "pod affinity with nothing to match must not be satisfied"

    local start=$SECONDS
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $anchor
  labels:
    sched-test: anchor
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML

    wait_until 90 "$follower to be bound to a node" _pod_is_bound "$follower" \
        || die "creating the anchor never satisfied the follower's pod affinity"
    local elapsed=$((SECONDS - start))
    [[ "$elapsed" -lt 60 ]] \
        || die "took ${elapsed}s after the anchor appeared — that is the safety net, not the event"

    assert_eq "$(pod_field "$follower" '{.spec.nodeName}')" "$(pod_field "$anchor" '{.spec.nodeName}')" \
        "affinity must place the follower on the same node as its anchor"

    delete_pod_if_exists "$follower"
    delete_pod_if_exists "$anchor"
}
register_test test_scheduler_honours_pod_affinity

test_scheduler_honours_topology_spread() {
    _require_nodescheduler
    local first="sched-spread-a" second="sched-spread-b"
    delete_pod_if_exists "$first"
    delete_pod_if_exists "$second"

    # minDomains 2 on a one-node cluster: fewer eligible domains exist than
    # asked for, so globalMin is forced to 0 and the skew limit applies. This
    # is the guard that stops the constraint silently doing nothing on a
    # narrow cluster, and it is what makes this testable with one node.
    local spread='
  topologySpreadConstraints:
  - maxSkew: 1
    minDomains: 2
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: DoNotSchedule
    labelSelector:
      matchLabels:
        sched-test: spread'

    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $first
  labels:
    sched-test: spread
spec:$spread
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML
    wait_until 60 "$first to be bound to a node" _pod_is_bound "$first" \
        || die "the first pod should fit: one pod in one domain is skew 1"

    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $second
  labels:
    sched-test: spread
spec:$spread
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML

    sleep 10
    assert_eq "$(pod_field "$second" '{.spec.nodeName}')" "" \
        "a second pod in the same domain would be skew 2, over maxSkew 1"

    delete_pod_if_exists "$first"
    delete_pod_if_exists "$second"
}
register_test test_scheduler_honours_topology_spread

# ── Phase 3: preemption ─────────────────────────────────────────────────

test_scheduler_preempts_a_lower_priority_pod() {
    _require_nodescheduler
    local low="sched-preempt-low" high="sched-preempt-high" node
    node="$(_the_node)"
    assert_not_empty "$node" "needs at least one node"
    delete_pod_if_exists "$low"
    delete_pod_if_exists "$high"

    # Cluster-scoped, so named for this suite and cleaned up at the end.
    kubectl apply -f - >/dev/null <<YAML
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata:
  name: sched-test-low
value: 100
globalDefault: false
---
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata:
  name: sched-test-high
value: 100000
globalDefault: false
YAML

    # Claim most of the node, so the high-priority pod cannot fit beside it
    # and the only way to place it is to take this one's room.
    local milli
    milli="$(_node_allocatable_milli_cpu "$node")"
    [[ "$milli" -gt 0 ]] || skip_test "node reports no allocatable CPU to work with"
    local most=$(( milli * 60 / 100 ))

    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $low
spec:
  priorityClassName: sched-test-low
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${most}m"
YAML
    wait_until 60 "$low to be bound to a node" _pod_is_bound "$low" \
        || die "the low-priority pod was never scheduled"

    local start=$SECONDS
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $high
spec:
  priorityClassName: sched-test-high
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${most}m"
YAML

    # The whole point: the high-priority pod is placed, and it happens by
    # evicting the low-priority one rather than by waiting for capacity that
    # is never coming.
    wait_until 120 "$high to be bound to a node" _pod_is_bound "$high" \
        || die "preemption never made room for the high-priority pod"
    local elapsed=$((SECONDS - start))

    wait_until 60 "$low to be gone" pod_gone "$low" \
        || die "the high-priority pod was placed but the low-priority one was never evicted"

    [[ "$elapsed" -lt 120 ]] \
        || die "took ${elapsed}s to preempt — too slow to be the event path"

    # $high itself claims ~60% of allocatable — wait for it to actually be
    # gone, not just for the API to accept the delete, or the next test
    # inherits a node that looks emptier than it is (see
    # delete_pod_and_wait_gone's own comment; found live in CI as
    # cpu_manager's exclusive-core test timing out right after this one).
    delete_pod_and_wait_gone "$high"
    kubectl delete priorityclass sched-test-low sched-test-high --ignore-not-found >/dev/null 2>&1 || true
}
register_test test_scheduler_preempts_a_lower_priority_pod

test_scheduler_does_not_preempt_when_policy_forbids_it() {
    _require_nodescheduler
    local low="sched-nopreempt-low" high="sched-nopreempt-high" node
    node="$(_the_node)"
    delete_pod_if_exists "$low"
    delete_pod_if_exists "$high"

    kubectl apply -f - >/dev/null <<YAML
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata:
  name: sched-test-low
value: 100
globalDefault: false
---
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata:
  name: sched-test-never
value: 100000
preemptionPolicy: Never
globalDefault: false
YAML

    local milli
    milli="$(_node_allocatable_milli_cpu "$node")"
    [[ "$milli" -gt 0 ]] || skip_test "node reports no allocatable CPU to work with"
    local most=$(( milli * 60 / 100 ))

    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $low
spec:
  priorityClassName: sched-test-low
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${most}m"
YAML
    wait_until 60 "$low to be bound to a node" _pod_is_bound "$low" \
        || die "the low-priority pod was never scheduled"

    # Higher priority, but forbidden from preempting. It must wait rather than
    # evict — a scheduler that ignores preemptionPolicy kills work the author
    # explicitly protected.
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $high
spec:
  priorityClassName: sched-test-never
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
    resources:
      requests:
        cpu: "${most}m"
YAML

    sleep 20
    assert_eq "$(pod_field "$high" '{.spec.nodeName}')" "" \
        "a pod with preemptionPolicy Never must wait, not evict"
    assert_true pod_exists "$low"

    delete_pod_if_exists "$high"
    # $low itself claims ~60% of allocatable and is actually Running —
    # wait for it to actually be gone, not just for the API to accept the
    # delete, or the next test inherits a node that looks emptier than it
    # is (see delete_pod_and_wait_gone's own comment; found live in CI as
    # cpu_manager's exclusive-core test timing out right after this one).
    delete_pod_and_wait_gone "$low"
    kubectl delete priorityclass sched-test-low sched-test-never --ignore-not-found >/dev/null 2>&1 || true
}
register_test test_scheduler_does_not_preempt_when_policy_forbids_it

# ── namespaceSelector on a pod affinity term ────────────────────────────
#
# This one is here because the previous behaviour passed every affinity test
# in this file. Terms using namespaceSelector used to match *every* namespace,
# for want of a Namespace watch — over-matching only refuses a placement,
# which is the safer of two wrong answers, and it is still wrong. The pair of
# assertions below is what tells the two apart: fail-open blocks in both
# directions, so only the second one can catch it.

_ns_selector_ns="sched-nsel-other"

_delete_nsel_namespace() {
    kubectl delete namespace "$_ns_selector_ns" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

test_scheduler_resolves_a_namespace_selector_against_real_labels() {
    _require_nodescheduler
    local blocked="sched-nsel-blocked" allowed="sched-nsel-allowed"
    delete_pod_if_exists "$blocked"
    delete_pod_if_exists "$allowed"
    _delete_nsel_namespace
    # A namespace pending deletion cannot be recreated, so wait it out rather
    # than racing it.
    try_wait_until 60 bash -c "! kubectl get namespace $_ns_selector_ns >/dev/null 2>&1" \
        || warn "namespace $_ns_selector_ns still terminating; the create below may fail"

    kubectl create namespace "$_ns_selector_ns" >/dev/null 2>&1 || true
    kubectl label namespace "$_ns_selector_ns" sched-nsel=other --overwrite >/dev/null 2>&1 \
        || die "could not label the helper namespace"

    kubectl apply -n "$_ns_selector_ns" -f - >/dev/null <<YAML || die "could not create the blocker pod"
apiVersion: v1
kind: Pod
metadata:
  name: sched-nsel-blocker
  labels:
    sched-test: nsel
spec:
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML
    try_wait_until 90 bash -c \
        "[ -n \"\$(kubectl get pod sched-nsel-blocker -n $_ns_selector_ns -o jsonpath='{.spec.nodeName}' 2>/dev/null)\" ]" \
        || die "the blocker pod in $_ns_selector_ns was never scheduled"

    # Selector matches the blocker's namespace: the anti-affinity applies, and
    # on a single node there is nowhere left.
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $blocked
spec:
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchLabels:
            sched-test: nsel
        namespaceSelector:
          matchLabels:
            sched-nsel: other
        topologyKey: kubernetes.io/hostname
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML
    sleep 10
    assert_eq "$(pod_field "$blocked" '{.spec.nodeName}')" "" \
        "a namespaceSelector that matches the blocker's namespace must apply the term"

    # Same term, a selector no namespace satisfies. The old fail-open path
    # blocked this too, which is what makes it the discriminating case.
    apply_manifest <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $allowed
spec:
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
      - labelSelector:
          matchLabels:
            sched-test: nsel
        namespaceSelector:
          matchLabels:
            sched-nsel: no-namespace-has-this
        topologyKey: kubernetes.io/hostname
  containers:
  - name: c
    image: busybox:1.36
    command: ["sh", "-c", "sleep 300"]
YAML
    wait_until 60 "$allowed to be bound to a node" _pod_is_bound "$allowed" \
        || die "a namespaceSelector matching no namespace must not apply the term — this is the fail-open regression"

    delete_pod_if_exists "$blocked"
    delete_pod_if_exists "$allowed"
    _delete_nsel_namespace
}
register_test test_scheduler_resolves_a_namespace_selector_against_real_labels

# ── PodTopologySpread's system default constraints ──────────────────────
#
# Deliberately a smoke test, and worth saying why rather than dressing it up.
# Both defaults are ScheduleAnyway, so they move scores and never feasibility
# — on a single-node cluster there is no second node for a score to prefer,
# and nothing about the *placement* can distinguish them from their absence.
# The score arithmetic and the ANDed selector derivation are covered by unit
# tests, which can build a three-zone cluster; what only a live cluster can
# check is that the Service/ReplicaSet watches feeding them exist, stay up,
# and do not wedge scheduling for the pods they now apply to.
test_scheduler_schedules_pods_that_get_default_spread_constraints() {
    _require_nodescheduler
    local name="sched-defspread"
    kctl delete deployment "$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kctl delete service "$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true

    # Selected by both a Service and a ReplicaSet, so the derived selector is
    # the intersection of two — the case that has an extra way to go wrong.
    apply_manifest <<YAML
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  selector:
    app: $name
  ports:
  - port: 80
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $name
spec:
  replicas: 2
  selector:
    matchLabels:
      app: $name
  template:
    metadata:
      labels:
        app: $name
        tier: front
    spec:
      containers:
      - name: c
        image: busybox:1.36
        command: ["sh", "-c", "sleep 300"]
YAML

    try_wait_until 120 bash -c \
        "[ \"\$(kubectl get pods -n \$TEST_NAMESPACE -l app=$name -o jsonpath='{.items[*].spec.nodeName}' | wc -w)\" -eq 2 ]" \
        || die "pods owned by a Service and a ReplicaSet were not both scheduled — the default spread constraints derived from those watches must not block anything"

    kctl delete deployment "$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kctl delete service "$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
register_test test_scheduler_schedules_pods_that_get_default_spread_constraints

# Phase 4: storage plugins (VolumeBinding's WaitForFirstConsumer path, and
# VolumeRestrictions' ReadWriteOncePod exclusivity). Needs real infrastructure
# this suite can't stand up itself — see e2e-full-setup.sh, which installs
# csi-driver-host-path and applies the WaitForFirstConsumer StorageClass this
# case needs (TEST_CSI_STORAGE_CLASS_WAIT); the other two classes it applies
# are both Immediate and cannot exercise this path at all.
test_scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen() {
    _require_nodescheduler
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS_WAIT:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS_WAIT not set — export it to a WaitForFirstConsumer StorageClass to exercise this"
    fi

    local claim="sched-wfc-claim" name="sched-wfc-pod"
    kctl delete pod "$name" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kctl delete pvc "$claim" --ignore-not-found >/dev/null 2>&1 || true

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $TEST_CSI_STORAGE_CLASS_WAIT
  resources:
    requests:
      storage: 64Mi
EOF

    sleep 8
    assert_eq "$(kubectl get pvc "$claim" -n "$TEST_NAMESPACE" -o jsonpath='{.status.phase}')" "Pending" \
        "a WaitForFirstConsumer PVC must stay unbound with no pod referencing it yet"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 300"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF

    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        local reason
        reason="$(kctl get pvc "$claim" -o jsonpath='{.status.phase}' 2>/dev/null)"
        delete_pod_and_pvc "$name" "$claim"
        skip_test "PVC never bound after a node was chosen (phase=$reason) — needs a working external-provisioner for TEST_CSI_STORAGE_CLASS_WAIT"
    fi

    local node selected_node
    node="$(pod_field "$name" '{.spec.nodeName}')"
    selected_node="$(kctl get pvc "$claim" -o jsonpath='{.metadata.annotations.volume\.kubernetes\.io/selected-node}')"
    assert_eq "$selected_node" "$node" \
        "VolumeBinding's PreBind must annotate the PVC with the exact node it chose"
    assert_eq "$(kctl get pvc "$claim" -o jsonpath='{.status.phase}')" "Bound" \
        "the PVC must actually be Bound once the pod is Running"

    delete_pod_and_pvc "$name" "$claim"
}
register_test test_scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen csi_dra

test_scheduler_claims_a_static_wait_for_first_consumer_volume() {
    _require_nodescheduler

    local class="sched-static-wfc" pv="sched-static-pv"
    local claim="sched-static-claim" pod="sched-static-pod"
    kctl delete pod "$pod" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kctl delete pvc "$claim" --ignore-not-found >/dev/null 2>&1 || true
    kubectl delete pv "$pv" --ignore-not-found >/dev/null 2>&1 || true
    kubectl delete storageclass "$class" --ignore-not-found >/dev/null 2>&1 || true

    # The no-provisioner + WaitForFirstConsumer combination is deliberate:
    # the controller-manager cannot bind this static PV before a scheduler
    # chooses a node. This therefore exercises nodescheduler's static-PV
    # Reserve/PreBind path rather than passing because the claim was already
    # Bound when the pod entered the queue.
    apply_manifest <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: $class
provisioner: kubernetes.io/no-provisioner
volumeBindingMode: WaitForFirstConsumer
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: $pv
spec:
  capacity:
    storage: 64Mi
  accessModes: ["ReadWriteOnce"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: $class
  hostPath:
    path: /tmp/notk8s-scheduler-static-pv
    type: DirectoryOrCreate
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: $class
  resources:
    requests:
      storage: 32Mi
EOF

    sleep 5
    assert_eq "$(kctl get pvc "$claim" -o jsonpath='{.status.phase}')" "Pending" \
        "a static WaitForFirstConsumer claim must wait for a scheduling decision"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo static-ok >/data/proof && sleep 300"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF

    # This is a scheduler/storage-controller assertion, not a kubelet volume
    # mount assertion. The nodelet deliberately supports PV-backed CSI only;
    # an in-tree hostPath PV is useful here because it needs no external
    # driver, but nodelet must not be expected to mount it. CSI-backed pod
    # startup is covered by csi_pvc.sh.
    wait_until 60 "$pod to be bound to a node" _pod_is_bound "$pod" \
        || die "nodescheduler did not choose a node for the static PV/PVC"
    wait_until 60 "$claim to become Bound" bash -c \
        "kubectl get pvc '$claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound" \
        || die "nodescheduler did not complete the static PV/PVC binding path"
    assert_eq "$(kubectl get pv "$pv" -o jsonpath='{.spec.claimRef.name}')" "$claim" \
        "VolumeBinding PreBind must prebind the PV to the selected claim"
    assert_eq "$(kubectl get pv "$pv" -o jsonpath='{.metadata.annotations.pv\.kubernetes\.io/bound-by-controller}')" "yes" \
        "a scheduler-created static prebind must carry upstream's bound-by-controller marker"
    assert_eq "$(kctl get pvc "$claim" -o jsonpath='{.spec.volumeName}')" "$pv" \
        "the PV binder must publish the scheduler's static PV choice back to the PVC"
    assert_eq "$(kctl get pvc "$claim" -o jsonpath='{.status.phase}')" "Bound" \
        "the PV binder must observe and complete nodescheduler's static choice"

    delete_pod_and_pvc "$pod" "$claim"
    kubectl delete pv "$pv" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kubectl delete storageclass "$class" --ignore-not-found >/dev/null 2>&1 || true
}
register_test test_scheduler_claims_a_static_wait_for_first_consumer_volume

test_scheduler_enforces_read_write_once_pod_exclusivity() {
    _require_nodescheduler
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_STORAGE_CLASS:-}" ]]; then
        skip_test "TEST_CSI_STORAGE_CLASS not set — export it to a StorageClass backed by a CSI driver to exercise this"
    fi

    local claim="sched-rwop-claim" first="sched-rwop-a" second="sched-rwop-b"
    kctl delete pod "$first" "$second" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kctl delete pvc "$claim" --ignore-not-found >/dev/null 2>&1 || true

    apply_manifest <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
  accessModes: ["ReadWriteOncePod"]
  storageClassName: $TEST_CSI_STORAGE_CLASS
  resources:
    requests:
      storage: 64Mi
EOF

    if ! try_wait_until 90 bash -c "kubectl get pvc '$claim' -n '$TEST_NAMESPACE' -o jsonpath='{.status.phase}' | grep -q Bound"; then
        kctl delete pvc "$claim" --ignore-not-found >/dev/null 2>&1
        skip_test "ReadWriteOncePod PVC never bound — driver for TEST_CSI_STORAGE_CLASS may not support that access mode"
    fi

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $first
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 300"]
      volumeMounts: [{name: data, mountPath: /data}]
  volumes:
    - name: data
      persistentVolumeClaim: {claimName: $claim}
EOF
    wait_until 60 "$first to be bound to a node" _pod_is_bound "$first" \
        || die "the first RWOP pod was never scheduled"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $second
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 300"]
      volumeMounts: [{name: data, mountPath: /data}]
  volumes:
    - name: data
      persistentVolumeClaim: {claimName: $claim}
EOF

    sleep 10
    assert_eq "$(pod_field "$second" '{.spec.nodeName}')" "" \
        "a second pod must never be scheduled while a ReadWriteOncePod PVC is already claimed"
    assert_contains "$(kubectl get events -n "$TEST_NAMESPACE" --field-selector involvedObject.name="$second" -o jsonpath='{.items[*].message}' 2>/dev/null)" \
        "ReadWriteOncePod" "the pod must say why, not just sit Pending"

    # The scheduler drops the holder from its cache on the actual DELETE
    # event, not when deletionTimestamp is set. CSI teardown must also finish
    # before nodelet removes that object. Under full-suite load the reference
    # attacher has taken just over two minutes to release the holder, so a
    # 90-second helper budget can fail even though the scheduler wakes and
    # binds the replacement correctly.
    delete_pod_if_exists "$first"
    wait_until 240 "$first gone" pod_gone "$first"
    wait_until 60 "$second to be bound to a node" _pod_is_bound "$second" \
        || die "freeing the ReadWriteOncePod claim never got the second pod scheduled"

    delete_pod_and_pvc "$second" "$claim"
}
register_test test_scheduler_enforces_read_write_once_pod_exclusivity csi_dra

# ── Phase 5: HTTP extenders ─────────────────────────────────────────────
#
# extender.rs is unit-tested for parsing/wire-format fidelity with no
# cluster involved. What that can't prove is that a real HTTP round trip
# out of cycle.rs's now-async schedule_one actually happens, that a
# rejection an extender returns actually blocks a real Binding, and that
# the reason it gave surfaces on the pod the way any other filter
# rejection's does. fake_extender.py fakes the extender, not the protocol
# — same "real protocol, fabricated backend" pattern fake_device_plugin.py
# (device_plugins.sh) uses.

_fake_extender_setup() { # sets FEXT_* globals; skips if python3 is missing
    if ! command -v python3 &>/dev/null; then
        skip_test "python3 not on PATH — needed to run the fake HTTP extender"
    fi
    FEXT_WORK="$(mktemp -d)"
    FEXT_PORT=18762
    FEXT_CONTROL="$FEXT_WORK/control"
    FEXT_LOG="$FEXT_WORK/requests.log"
    FEXT_SERVER_LOG="$FEXT_WORK/server.log"
    echo "accept" > "$FEXT_CONTROL"
    : > "$FEXT_LOG"

    cat > "$FEXT_WORK/fake_extender.py" <<'PYEOF'
# Fake HTTP extender for e2e testing — speaks the real
# k8s.io/kube-scheduler/extender/v1 wire format against whatever verdict the
# control file (argv[2]) currently holds,
# polled fresh on every request so a test can flip behaviour mid-run with
# no restart: "accept" passes every node back via NodeNames (the same
# explicit-list shape a real extender uses, not the "neither field set"
# edge case); "reject:<reason>" fails every node it was asked about with
# that reason and deliberately omits NodeNames/Nodes, exercising the "an
# extender that never echoes back a survivor is read as nobody passing"
# case a real extender.go was checked against.
#
# Upstream's extender/v1 structs have no JSON tags. Go's encoding/json emits
# their exported field names verbatim: Pod, Nodes, NodeNames, FailedNodes,
# Error. Keeping this fake byte-compatible matters; otherwise the fake and
# Rust implementation can agree with each other while both reject real
# extenders.
import sys, json, http.server, socketserver

port, control_file, log_file = int(sys.argv[1]), sys.argv[2], sys.argv[3]

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        args = json.loads(self.rfile.read(length))
        pod_name = (args.get("Pod") or {}).get("metadata", {}).get("name", "")
        node_items = ((args.get("Nodes") or {}).get("items")) or []
        node_items = node_items or [{"metadata": {"name": n}} for n in (args.get("NodeNames") or [])]
        node_names = [n.get("metadata", {}).get("name", "") for n in node_items]
        with open(log_file, "a") as f:
            f.write(f"{self.path} pod={pod_name} nodes={','.join(node_names)}\n")

        try:
            verdict = open(control_file).read().strip()
        except FileNotFoundError:
            verdict = "accept"

        if self.path.endswith("/filter"):
            if verdict.startswith("reject:"):
                reason = verdict[len("reject:"):]
                result = {"FailedNodes": {n: reason for n in node_names}}
            else:
                result = {"NodeNames": node_names}
        else:
            result = {"Error": f"fake extender has no verb {self.path}"}

        payload = json.dumps(result).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

# allow_reuse_address: without it, a back-to-back test run reusing this
# same fixed port (see FEXT_PORT) can hit the previous run's socket still
# sitting in TIME_WAIT, and TCPServer's bind() raises "Address already in
# use" and the process exits before ever listening — the caller's own
# wait_until then times out with no obvious reason why.
socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", port), Handler) as httpd:
    httpd.serve_forever()
PYEOF

    python3 "$FEXT_WORK/fake_extender.py" "$FEXT_PORT" "$FEXT_CONTROL" "$FEXT_LOG" \
        > "$FEXT_SERVER_LOG" 2>&1 &
    FEXT_PID=$!
    wait_until 10 "fake HTTP extender listening on 127.0.0.1:$FEXT_PORT" \
        bash -c "exec 3<>/dev/tcp/127.0.0.1/$FEXT_PORT" \
        || { kill "$FEXT_PID" 2>/dev/null || true; skip_test "fake HTTP extender never started listening — see $FEXT_SERVER_LOG"; }
}

_fake_extender_teardown() {
    nodescheduler_restore_env
    [[ -n "${FEXT_PID:-}" ]] && kill "$FEXT_PID" 2>/dev/null || true
    [[ -n "${FEXT_WORK:-}" ]] && rm -rf "$FEXT_WORK"
}

test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection() {
    _require_nodescheduler
    _fake_extender_setup
    trap '_fake_extender_teardown' EXIT

    echo "reject:no-gpu-quota-fake-extender" > "$FEXT_CONTROL"
    # systemd's own Environment= parsing strips a bare `"` out of an
    # assignment (confirmed live: `Environment=V=[{"a":"b"}]` reaches the
    # process as `[{a:b}]`, silently invalid JSON) — `\"` is how a literal
    # quote survives into the child's environment.
    local json_env
    json_env='NODESCHEDULER_EXTENDERS_JSON=[{\"urlPrefix\":\"http://127.0.0.1:'"$FEXT_PORT"'\",\"filterVerb\":\"filter\",\"nodeCacheCapable\":true}]'
    nodescheduler_restart_with_env "$json_env"

    local pod="sched-extender-reject"
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

    sleep 10
    assert_eq "$(pod_field "$pod" '{.spec.nodeName}')" "" \
        "a pod the extender rejects on every node must never be scheduled"
    # Found live in CI: the FailedScheduling event write is one more async
    # step past the HTTP round trip to the extender itself (pod created ->
    # cycle runs -> extender POST -> reject -> event write), and under a
    # loaded shared runner a flat 10s sleep isn't always enough for all of
    # that plus the event becoming queryable — a plain assert_contains right
    # after would flake with an empty string, not a wrong one. Retrying
    # gives it real headroom without slowing down the common on-time case.
    try_wait_until 30 bash -c "kctl get events --field-selector involvedObject.name=$pod -o jsonpath='{.items[*].message}' 2>/dev/null | grep -q no-gpu-quota-fake-extender"
    local event_message
    event_message="$(kctl get events --field-selector involvedObject.name="$pod" -o jsonpath='{.items[*].message}' 2>/dev/null)"
    assert_contains "$event_message" \
        "no-gpu-quota-fake-extender" "the FailedScheduling event must carry the extender's own rejection reason"
    assert_contains "$(cat "$FEXT_LOG")" "/filter pod=$pod" \
        "the extender must actually have been called with this pod"

    delete_pod_if_exists "$pod"
    _fake_extender_teardown
    trap - EXIT
}
register_test test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection

test_scheduler_schedules_a_pod_an_http_extender_approves() {
    _require_nodescheduler
    _fake_extender_setup
    trap '_fake_extender_teardown' EXIT

    echo "accept" > "$FEXT_CONTROL"
    # systemd's own Environment= parsing strips a bare `"` out of an
    # assignment (confirmed live: `Environment=V=[{"a":"b"}]` reaches the
    # process as `[{a:b}]`, silently invalid JSON) — `\"` is how a literal
    # quote survives into the child's environment.
    local json_env
    json_env='NODESCHEDULER_EXTENDERS_JSON=[{\"urlPrefix\":\"http://127.0.0.1:'"$FEXT_PORT"'\",\"filterVerb\":\"filter\",\"nodeCacheCapable\":true}]'
    nodescheduler_restart_with_env "$json_env"

    local pod="sched-extender-accept"
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

    wait_until 60 "$pod to be bound to a node" _pod_is_bound "$pod" \
        || die "an extender approving every node must not block scheduling — is nodescheduler actually calling it? check $FEXT_SERVER_LOG"
    assert_contains "$(cat "$FEXT_LOG")" "/filter pod=$pod" \
        "the extender must actually have been called with this pod, not merely have never blocked it"

    delete_pod_if_exists "$pod"
    _fake_extender_teardown
    trap - EXIT
}
register_test test_scheduler_schedules_a_pod_an_http_extender_approves
