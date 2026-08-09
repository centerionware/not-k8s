# lib/test/cases/service_proxy.sh — ClusterIP/NodePort routing, i.e. the
# `nodeproxy` binary (crates/nodeproxy/src/svc.rs), kube-proxy's job.
#
# Written when that code was split out of nodelet into its own binary, and
# overdue independently of the split: before this file, service networking
# had NO e2e coverage at all. Its only verification was svc.rs's three
# inline unit tests, and those self-skip when `nft` is missing OR when
# `nft -c` produces no output (no CAP_NET_ADMIN) — so a broken ruleset, a
# proxy that never started, or a Service whose backends never resolved
# would all have passed CI silently.
#
# These tests curl real addresses from the host: a ClusterIP (which no
# interface owns — reaching it at all proves the nat/output DNAT rule
# exists) and the node's own IP at a NodePort. Nothing here inspects
# nodeproxy's internals; if the traffic lands in the container, the whole
# chain worked.

# A one-shot-per-connection HTTP responder, same busybox `nc -lp` trick
# cases/networking.sh uses and for the same reason: alpine's busybox has no
# httpd applet (confirmed live), which used to surface as a misleading
# "pod never reached Running".
_svc_responder_command() { # _svc_responder_command <marker> <port>
    echo "[\"sh\", \"-c\", \"printf 'HTTP/1.1 200 OK\\\\r\\\\nContent-Type: text/plain\\\\r\\\\nConnection: close\\\\r\\\\n\\\\r\\\\n$1\\\\n' > /tmp/resp && while true; do nc -lp $2 < /tmp/resp; done\"]"
}

# nft needs CAP_NET_ADMIN and this suite runs unprivileged (other cases —
# credential_provider.sh, log_rotation.sh — reach for sudo the same way).
# Try direct first so a root-run suite doesn't need sudo installed at all.
_nft() {
    if nft "$@" 2>/dev/null; then return 0; fi
    command -v sudo >/dev/null 2>&1 && sudo nft "$@"
}

_delete_svc_if_exists() {
    kctl delete service "$1" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

# Every test here needs a real CRI runtime (pods with real IPs to route to),
# a usable nft, and a service proxy actually installed on this node —
# --proxy=none is a legitimate deployment (something else owns the
# datapath) and must skip, not fail.
_require_service_proxy() {
    node_uses_cri_runtime || skip_test "needs cri runtime (mock pods have no real IPs to route to)"
    # Probe through _nft, not `command -v nft`. nft lives in /usr/sbin,
    # which is not on an unprivileged PATH on Debian — so `command -v nft`
    # fails on a host where `sudo nft` works perfectly, and every test in
    # this file skipped with "needs nft" against a cluster that was
    # routing traffic correctly. Found running this suite for real.
    _nft --version >/dev/null 2>&1 \
        || skip_test "needs nft (nftables) reachable either directly or via sudo"
    _nft list table inet not_k8s_svc >/dev/null 2>&1 \
        || skip_test "no readable 'inet not_k8s_svc' table — either nodeproxy isn't running on this node (deployed with --proxy=none?) or this suite can't reach nftables (needs root or passwordless sudo)"
}

# Waits until the Service's EndpointSlice actually has a ready address.
# nodeproxy programs nothing for a Service with no backends (dnat_target()
# returns None for zero backends), so curling before this is just a race
# against the control plane's own EndpointSlice controller, not a proxy bug.
_wait_for_ready_endpoint() { # _wait_for_ready_endpoint <service-name> <timeout>
    try_wait_until "$2" bash -c \
        "kubectl get endpointslices -n '$TEST_NAMESPACE' -l kubernetes.io/service-name='$1' \
         -o jsonpath='{.items[*].endpoints[*].addresses[0]}' 2>/dev/null | grep -q ."
}

# Cleanup state for the EXIT trap, deliberately global. A `trap _cleanup
# EXIT` closing over the test function's `local` variables fires AFTER that
# function has returned, when those locals no longer exist — and under the
# harness's `set -u` that aborts with "unbound variable". Found running this
# suite for real: four tests failed that way with every assertion already
# passed, reporting a cleanup crash as a product failure.
_SVC_CLEANUP_PODS=()
_SVC_CLEANUP_SVCS=()
_SVC_CLEANUP_RESTORE_NODEPROXY=0

_svc_track_pod() { _SVC_CLEANUP_PODS+=("$@"); }
_svc_track_svc() { _SVC_CLEANUP_SVCS+=("$@"); }

_svc_cleanup() {
    local x
    for x in ${_SVC_CLEANUP_PODS[@]+"${_SVC_CLEANUP_PODS[@]}"}; do delete_pod_if_exists "$x"; done
    for x in ${_SVC_CLEANUP_SVCS[@]+"${_SVC_CLEANUP_SVCS[@]}"}; do _delete_svc_if_exists "$x"; done
    [[ "$_SVC_CLEANUP_RESTORE_NODEPROXY" -eq 1 ]] && { nodeproxy_restore_env || true; }
    _SVC_CLEANUP_PODS=(); _SVC_CLEANUP_SVCS=(); _SVC_CLEANUP_RESTORE_NODEPROXY=0
    return 0
}

# Whether this kernel can actually apply a given load-balancing selector.
# nft_numgen and nft_hash are separate kernel modules, absent on some
# builds — confirmed on an Android-derived 6.12 kernel, where BOTH are
# missing and any rule using them fails with "Could not process rule: No
# such file or directory" at the selector token. dnat_target()'s
# single-backend fast path exists because of exactly this. A test that
# needs a selector this kernel can't run must skip, not fail.
_svc_kernel_supports_rule() { # <rule-body>
    local probe="probe_svc_cap_$$"
    local ok=0
    _nft -f - >/dev/null 2>&1 <<EOF && ok=1
add table inet $probe
add chain inet $probe pre { type nat hook prerouting priority dstnat ; policy accept ; }
add rule inet $probe pre $1
EOF
    _nft delete table inet "$probe" >/dev/null 2>&1
    [[ "$ok" -eq 1 ]]
}

_svc_kernel_supports_selector() { # <selector-expression>
    _svc_kernel_supports_rule \
        "ip daddr 10.99.0.1 tcp dport 80 dnat to $1 mod 2 map { 0 : 10.42.0.5 . 8080, 1 : 10.42.0.6 . 8080 }"
}

# NodePort rules are all `fib daddr type local` (nft_fib), a separate kernel
# module absent on some builds — confirmed on an Android-derived 6.12
# kernel, alongside missing nft_numgen and nft_hash.
#
# Be precise about what a skip here means. It does NOT mean NodePort is
# unimplementable on such a kernel: `ip daddr <node-ip>` and a named
# `ip daddr @localips` set both apply fine there (verified directly), and
# svc.rs's own comment names the explicit-local-IP approach as the
# alternative it chose fib over — fib avoids having to track every local
# address. So this skip means "the current implementation cannot run here",
# which is a gap in nodeproxy, not a fact about the kernel.
#
# It matters more than an ordinary skip because the ruleset is applied
# atomically: on such a kernel a single NodePort Service takes every other
# Service's rules down with it, and (since apply failures are fatal) puts
# nodeproxy in a restart loop until that Service is deleted.
_svc_kernel_supports_nodeport() {
    _svc_kernel_supports_rule "fib daddr type local tcp dport 31999 dnat ip to 10.42.0.5:8080"
}

# _svc_backend_pod <name> <label> <marker> — a pod running the responder,
# waited to Running. Split out because most tests below need one and the
# manifest is otherwise repeated verbatim.
_svc_backend_pod() {
    local name="$1" label="$2" marker="$3"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: $label
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command "$marker" 8080)
      ports:
        - containerPort: 8080
EOF
    try_wait_until 90 pod_is_phase "$name" Running
}

# _svc_clusterip_service <name> <label> [extra-spec-lines] — a plain
# ClusterIP Service selecting <label> on 80 -> 8080.
_svc_clusterip_service() {
    local name="$1" label="$2" extra="${3:-}"
    apply_manifest <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  selector:
    app: $label
${extra}  ports:
    - port: 80
      targetPort: 8080
EOF
}

# The rules nodeproxy has programmed for one ClusterIP, as text. Used by
# the tests that assert on the ruleset rather than on traffic — a rule
# being present is not proof it works, but a rule being ABSENT (or being
# the wrong form) is proof of a specific bug, and it's the only way to see
# things like which load-balancing expression was chosen.
# Matches the address only at a rule-token boundary. A plain substring
# match makes 10.43.0.1 match a rule written for 10.43.0.10 — confirmed —
# which cuts both ways: it can mask a genuinely missing rule, and it can
# make the "rules are gone" waits below time out against an unrelated
# Service that merely shares an address prefix.
_svc_rules_for() { # _svc_rules_for <cluster-ip>
    local ip_re="${1//./\\.}"
    _nft list table inet not_k8s_svc 2>/dev/null | grep -E "(^|[^0-9.])${ip_re}([^0-9.]|\$)" || true
}

# Predicates for try_wait_until, which invokes its command in the current
# shell — so these work directly, without the `bash -c` (and the exported
# functions it would then need) the inline versions used to require.
_svc_rules_gone() { # _svc_rules_gone <cluster-ip>
    [[ -z "$(_svc_rules_for "$1")" ]]
}

_svc_rules_match() { # _svc_rules_match <cluster-ip> <pattern>
    _svc_rules_for "$1" | grep -q "$2"
}

# True once this ClusterIP has rules but none of them use a selector the
# kernel can't run. "Has rules" matters: an empty result would otherwise
# satisfy this trivially while the Service was in fact unrouted.
_svc_rules_gone_of_selector() { # <cluster-ip>
    local rules; rules="$(_svc_rules_for "$1")"
    [[ -n "$rules" ]] && ! grep -qE "numgen|jhash" <<<"$rules"
}

test_clusterip_service_routes_to_its_backend_pod() {
    _require_service_proxy
    local name="svc-clusterip-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command clusterip-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  selector:
    app: $name
  ports:
    - port: 80
      targetPort: 8080
EOF
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "backend pod never reached Running — this is a pod-lifecycle failure, not a service-routing one; check nodelet's logs first"
    fi
    if ! _wait_for_ready_endpoint "$name" 60; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "the Service's EndpointSlice never got a ready address — that's the control plane's endpointslice controller, upstream of nodeproxy, which programs nothing for a backend-less Service by design"
    fi

    local cluster_ip body
    cluster_ip="$(kctl get service "$name" -o jsonpath='{.spec.clusterIP}')"
    # A ClusterIP is owned by no interface anywhere — if the nat/output
    # DNAT rule isn't there, this connection has nowhere to go at all.
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q clusterip-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "curling ClusterIP $cluster_ip never reached the backend pod — check nodeproxy's log (journalctl -u nodeproxy) and build_ruleset()/dnat_target() in crates/nodeproxy/src/svc.rs against the ruleset dumped above"
    fi
    assert_contains "$body" "clusterip-marker" "response body from curling the Service's ClusterIP"
    delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
}

test_nodeport_service_is_reachable_on_the_node_ip() {
    _require_service_proxy
    # Deliberately NOT gated on nft_fib. NodePort has to work on kernels
    # without it too — nodeproxy falls back to matching this node's own
    # addresses explicitly. That fallback exists because of this exact
    # hardware, so skipping here would retire the only test that covers it.
    local name="svc-nodeport-check"
    local node_port=31890
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command nodeport-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  type: NodePort
  selector:
    app: $name
  ports:
    - port: 80
      targetPort: 8080
      nodePort: $node_port
EOF
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "backend pod never reached Running — a pod-lifecycle failure, not a service-routing one"
    fi
    if ! _wait_for_ready_endpoint "$name" 60; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "the Service's EndpointSlice never got a ready address"
    fi

    # The NodePort rule is the one that needed an explicit family qualifier
    # (`dnat ip to ...`) because `fib daddr type local` doesn't imply one —
    # see svc.rs's module doc. Without that, nft fails silently (non-zero
    # exit, no error text), which is exactly the kind of break this test
    # exists to catch.
    local node_ip body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$node_ip:$node_port/ | grep -q nodeport-marker" \
        && curl -sS --max-time 5 "http://$node_ip:$node_port/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "curling $node_ip:$node_port never reached the backend pod — check the 'fib daddr type local ... dnat ip to' rules in the ruleset dumped above, and that this host's firewall allows the test script to reach that port"
    fi
    assert_contains "$body" "nodeport-marker" "response body from curling the node's own IP at the NodePort"
    delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
}

test_service_with_no_endpoints_does_not_wedge_the_ruleset() {
    _require_service_proxy
    # The whole ruleset is rebuilt and re-applied atomically on every
    # Service/EndpointSlice event. A Service that resolves to zero backends
    # is the case most likely to emit something nft rejects — and because
    # `nft -f -` applies the file as a unit, one bad rule takes down ALL
    # service routing on the node, not just that Service's. So: stand up a
    # working Service, add a backend-less one beside it, and confirm the
    # working one still works.
    local live="svc-endpointless-live" dead="svc-endpointless-dead"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $live
  labels:
    app: $live
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command endpointless-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $live
spec:
  selector:
    app: $live
  ports:
    - port: 80
      targetPort: 8080
EOF
    if ! try_wait_until 90 pod_is_phase "$live" Running; then
        delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"
        die "backend pod never reached Running"
    fi
    if ! _wait_for_ready_endpoint "$live" 60; then
        delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"
        die "the live Service's EndpointSlice never got a ready address"
    fi

    apply_manifest <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $dead
spec:
  selector:
    app: nothing-matches-this-selector
  ports:
    - port: 80
      targetPort: 8080
EOF

    # Curling $live here would prove nothing: its rules already exist from
    # before $dead was created, so a nodeproxy that crashed or emitted an
    # nft-rejected ruleset on $dead's event would still be serving the
    # PREVIOUS, still-installed table and this test would pass green.
    #
    # So probe with a Service created strictly after $dead instead. Its rules
    # can only exist if a rebuild that included the backend-less Service was
    # generated AND accepted by the kernel. Same pod behind it — this is one
    # more Service object, not more workload.
    local probe="svc-endpointless-probe"
    apply_manifest <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $probe
spec:
  selector:
    app: $live
  ports:
    - port: 80
      targetPort: 8080
EOF
    if ! _wait_for_ready_endpoint "$probe" 60; then
        delete_pod_if_exists "$live"
        _delete_svc_if_exists "$live"; _delete_svc_if_exists "$dead"; _delete_svc_if_exists "$probe"
        die "the probe Service's EndpointSlice never got a ready address — control-plane side, upstream of nodeproxy"
    fi

    local cluster_ip body
    cluster_ip="$(kctl get service "$probe" -o jsonpath='{.spec.clusterIP}')"
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q endpointless-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$live"
        _delete_svc_if_exists "$live"; _delete_svc_if_exists "$dead"; _delete_svc_if_exists "$probe"
        die "a Service created after a backend-less one never got working rules — the whole ruleset is applied atomically, so one rule nft rejects takes every Service down together. Check build_ruleset()/dnat_target()'s zero-backend path against the dump above, and journalctl -u nodeproxy for an 'failed to apply nft ruleset' warning"
    fi
    assert_contains "$body" "endpointless-marker" "a Service created after a backend-less Service still routes"
    # And the table itself is still parseable, not left in some half state.
    assert_true _nft list table inet not_k8s_svc

    delete_pod_if_exists "$live"
    _delete_svc_if_exists "$live"; _delete_svc_if_exists "$dead"; _delete_svc_if_exists "$probe"
}

test_nodeproxy_runs_as_its_own_service_separate_from_nodelet() {
    # Cheap packaging assertion: the split's whole point is that Service
    # routing is a separate, independently replaceable process. If the unit
    # never got installed, the routing tests above would fail with a
    # confusing "curl never reached the pod"; this fails with the real
    # reason instead.
    node_uses_cri_runtime || skip_test "needs cri runtime"
    command -v systemctl >/dev/null 2>&1 || skip_test "not a systemd host (OpenRC/fallback tiers aren't asserted here)"
    systemctl list-unit-files nodeproxy.service >/dev/null 2>&1 \
        && systemctl cat nodeproxy.service >/dev/null 2>&1 \
        || skip_test "no nodeproxy.service installed (deployed with --proxy=none?)"

    assert_true systemctl is-active --quiet nodeproxy.service
    # Independent of nodelet, deliberately: neither unit orders against the
    # other (see deploy/lib/nodeproxy-service.sh's header).
    #
    # Assert on the ordering directives, not the whole unit text. The unit
    # carries a comment that mentions nodelet.service by name (explaining
    # where its StartLimitIntervalSec reasoning came from), so a substring
    # search over `systemctl cat` output failed against a unit whose
    # ordering was in fact correct. Found running this suite for real.
    local unit ordering
    unit="$(systemctl cat nodeproxy.service)"
    ordering="$(printf '%s\n' "$unit" | grep -E '^(After|Before|Wants|Requires|Requisite|BindsTo|PartOf)=' || true)"
    assert_not_empty "$ordering" "nodeproxy.service must declare some ordering (it needs k3s)"
    assert_not_contains "$ordering" "nodelet" "nodeproxy.service must not order against nodelet.service — they're independent components"
    assert_contains "$unit" "run-nodeproxy.sh" "nodeproxy.service ExecStart"
}

# ── Traffic originated inside a pod ───────────────────────────────────
# Everything above curls from the host, which only ever exercises the nat
# `output` chain. Pod-originated traffic arrives on `prerouting` instead —
# a different chain, and one that additionally depends on br_netfilter
# actually being loaded so bridged traffic reaches netfilter at all
# (deploy/lib/nft.sh's enable_bridge_netfilter). None of that was covered.

test_clusterip_is_reachable_from_inside_a_pod() {
    _require_service_proxy
    local backend="svc-prerouting-backend" client="svc-prerouting-client"
    local svc="svc-prerouting"
    trap _svc_cleanup EXIT
    _svc_track_pod "$backend" "$client"; _svc_track_svc "$svc"

    _svc_backend_pod "$backend" "$svc" prerouting-marker \
        || die "backend pod never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    # A separate pod, so this is pod -> ClusterIP -> a DIFFERENT pod: the
    # ordinary east-west path, not the hairpin case below.
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $client
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
EOF
    try_wait_until 90 pod_is_phase "$client" Running || die "client pod never reached Running"

    local cluster_ip out
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"
    if ! try_wait_until 60 bash -c \
        "kubectl exec -n '$TEST_NAMESPACE' '$client' -- wget -qO- --timeout=5 http://$cluster_ip:80/ 2>/dev/null | grep -q prerouting-marker"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        echo "--- br_netfilter ---"
        cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo "(bridge-nf-call-iptables absent)"
        die "a pod could not reach a ClusterIP — this is the nat prerouting path, not the output path the host-originated tests cover. Check the prerouting rules above and that br_netfilter is loaded (enable_bridge_netfilter in deploy/lib/nft.sh)"
    fi
    out="$(kubectl exec -n "$TEST_NAMESPACE" "$client" -- wget -qO- --timeout=5 "http://$cluster_ip:80/" 2>/dev/null)"
    assert_contains "$out" "prerouting-marker" "response body from a pod curling a ClusterIP"
}

test_a_pod_reaching_its_own_service_gets_hairpin_masquerade() {
    _require_service_proxy
    # The postrouting rule (`ct status dnat masquerade`) exists solely for
    # this case: a pod calls a Service whose only backend is itself, so
    # without SNAT the reply is sourced from the same address the request
    # was sent to and the connection hangs. It is the one rule in
    # build_ruleset() no other test in this file touches at all.
    local name="svc-hairpin" svc="svc-hairpin"
    trap _svc_cleanup EXIT
    _svc_track_pod "$name"; _svc_track_svc "$svc"

    _svc_backend_pod "$name" "$svc" hairpin-marker || die "pod never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    local cluster_ip
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"

    assert_contains "$(_nft list table inet not_k8s_svc 2>/dev/null)" "masquerade" \
        "postrouting chain must carry the hairpin masquerade rule"

    # The responder serves one connection at a time, so it has to be free
    # to answer its own request — nc's loop reopens the listener after each
    # connection, which is enough here.
    if ! try_wait_until 60 bash -c \
        "kubectl exec -n '$TEST_NAMESPACE' '$name' -- wget -qO- --timeout=5 http://$cluster_ip:80/ 2>/dev/null | grep -q hairpin-marker"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        die "a pod could not reach a Service that routes back to itself — the classic hairpin failure. Check the 'ct status dnat masquerade' rule in the postrouting chain"
    fi
}

# ── Backend-set changes ───────────────────────────────────────────────

test_multiple_backends_use_the_load_balancing_map_form() {
    _require_service_proxy
    # dnat_target() deliberately emits a bare <ip>:<port> for exactly one
    # backend and a `numgen ... mod N map { ... }` for more than one. That
    # split is not cosmetic: the single-backend fast path exists because a
    # real Android-derived kernel rejected `numgen` outright (missing
    # nft_numgen module), and the map form is the only way to express
    # "pick one of N". A regression collapsing the two would be invisible
    # to every other test here, since both forms route fine for N=1.
    local svc="svc-multibackend"
    trap _svc_cleanup EXIT
    _svc_track_pod "$svc-a" "$svc-b"; _svc_track_svc "$svc"
    # Not gated: a two-backend Service must ROUTE on every kernel. Only the
    # map-form assertion below is conditional, because a kernel without
    # nft_numgen genuinely cannot select among backends and nodeproxy
    # degrades to sending everything to one of them.
    local can_lb=0
    _svc_kernel_supports_selector "numgen random" && can_lb=1

    _svc_backend_pod "$svc-a" "$svc" multibackend-marker || die "backend a never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    local cluster_ip
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"

    # One backend: the fast path, no numgen anywhere near this ClusterIP.
    assert_not_contains "$(_svc_rules_for "$cluster_ip")" "numgen" \
        "a single-backend Service must use a bare dnat target, not a numgen map (that path exists for kernels without nft_numgen)"

    _svc_backend_pod "$svc-b" "$svc" multibackend-marker || die "backend b never reached Running"
    if ! try_wait_until 60 bash -c \
        "kubectl get endpointslices -n '$TEST_NAMESPACE' -l kubernetes.io/service-name='$svc' -o jsonpath='{.items[*].endpoints[*].addresses[0]}' 2>/dev/null | wc -w | grep -qx 2"; then
        die "the Service never reported two ready backends"
    fi

    if [[ "$can_lb" -eq 1 ]]; then
        # Two backends on a capable kernel: the map form.
        if ! try_wait_until 60 _svc_rules_match "$cluster_ip" 'map {'; then
            echo "--- rules for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
            die "a two-backend Service never got the 'numgen ... mod N map { ... }' form — check dnat_target()'s N>1 branch"
        fi
    else
        # No nft_numgen: the ruleset must contain no selector at all, since
        # one rejected rule takes the whole atomically-applied file with it.
        echo "  (kernel has no nft_numgen — asserting the degraded path instead)"
        if ! try_wait_until 60 _svc_rules_gone_of_selector "$cluster_ip"; then
            echo "--- rules for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
            die "a kernel without nft_numgen still got a numgen/jhash selector emitted for it — that rule is rejected and takes every other Service's rules down with it. Check probe_caps()/lb_expr()"
        fi
    fi

    local body
    body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q multibackend-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")" \
        || die "a two-backend Service stopped routing entirely once it had the map form"
    assert_contains "$body" "multibackend-marker" "response from a load-balanced two-backend Service"
}

test_losing_every_backend_removes_the_dnat_rule() {
    _require_service_proxy
    # dnat_target() returns None for zero backends, so the Service's rules
    # must disappear rather than linger pointing at a dead pod IP — which
    # would blackhole traffic instead of failing it fast.
    local svc="svc-drain"
    trap _svc_cleanup EXIT
    _svc_track_pod "$svc-a"; _svc_track_svc "$svc"

    _svc_backend_pod "$svc-a" "$svc" drain-marker || die "backend never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    local cluster_ip
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"
    assert_not_empty "$(_svc_rules_for "$cluster_ip")" "rules for a Service that has a ready backend"

    kctl delete pod "$svc-a" --wait=true >/dev/null 2>&1 || true
    if ! try_wait_until 90 _svc_rules_gone "$cluster_ip"; then
        echo "--- rules still present for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
        die "a Service whose last backend went away kept its DNAT rule, which blackholes traffic at a dead pod IP instead of failing it fast"
    fi
}

test_deleting_a_service_removes_its_rules() {
    _require_service_proxy
    local svc="svc-deleted"
    trap _svc_cleanup EXIT
    _svc_track_pod "$svc-a"; _svc_track_svc "$svc"

    _svc_backend_pod "$svc-a" "$svc" deleted-marker || die "backend never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    local cluster_ip
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"
    assert_not_empty "$(_svc_rules_for "$cluster_ip")" "rules for a live Service"

    # Exercises apply_event()'s Delete arm. A ClusterIP is recycled by the
    # apiserver, so a stale rule here doesn't just waste space — it can
    # silently hijack a DIFFERENT Service allocated the same address later.
    _delete_svc_if_exists "$svc"
    if ! try_wait_until 90 _svc_rules_gone "$cluster_ip"; then
        echo "--- rules still present for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
        die "a deleted Service left its rules behind — check apply_event()'s Delete arm. ClusterIPs get recycled, so a stale rule can hijack whichever Service is allocated that address next"
    fi
}

# ── Per-Service policy and special cases ──────────────────────────────

test_session_affinity_client_ip_forces_source_hash() {
    _require_service_proxy
    # sessionAffinity: ClientIP must override the proxy-wide default,
    # because it's a per-Service opt-in. With two backends the choice is
    # visible in the rule itself: jhash rather than numgen. This is a
    # ruleset assertion on purpose — proving stickiness by traffic would
    # need many connections from distinct source IPs, which this harness
    # has no way to produce.
    local svc="svc-affinity"
    trap _svc_cleanup EXIT
    _svc_track_pod "$svc-a" "$svc-b"; _svc_track_svc "$svc"
    # Not gated: a ClientIP-affinity Service must route on every kernel.
    # Only the jhash assertion is conditional — without nft_hash, nodeproxy
    # pins everything to one backend, which satisfies "same client, same
    # backend" trivially and is the correct degradation.
    local can_hash=0
    _svc_kernel_supports_selector "jhash ip saddr" && can_hash=1

    _svc_backend_pod "$svc-a" "$svc" affinity-marker || die "backend a never reached Running"
    _svc_backend_pod "$svc-b" "$svc" affinity-marker || die "backend b never reached Running"
    _svc_clusterip_service "$svc" "$svc" "  sessionAffinity: ClientIP
"
    if ! try_wait_until 60 bash -c \
        "kubectl get endpointslices -n '$TEST_NAMESPACE' -l kubernetes.io/service-name='$svc' -o jsonpath='{.items[*].endpoints[*].addresses[0]}' 2>/dev/null | wc -w | grep -qx 2"; then
        die "the Service never reported two ready backends"
    fi

    local cluster_ip
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"
    if [[ "$can_hash" -eq 1 ]]; then
        if ! try_wait_until 60 _svc_rules_match "$cluster_ip" 'jhash'; then
            echo "--- rules for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
            die "sessionAffinity: ClientIP did not produce a jhash selector — check lb_expr()'s sticky branch, which must win over the configured default"
        fi
    else
        echo "  (kernel has no nft_hash — asserting the degraded path instead)"
        if ! try_wait_until 60 _svc_rules_gone_of_selector "$cluster_ip"; then
            echo "--- rules for $cluster_ip ---"; _svc_rules_for "$cluster_ip"
            die "a kernel without nft_hash still got a jhash selector emitted for it — that rule is rejected and takes every other Service down with it"
        fi
    fi
    assert_not_contains "$(_svc_rules_for "$cluster_ip")" "numgen" \
        "a ClientIP-affinity Service must not use a random/round-robin selector"
    # Either way it has to actually serve traffic.
    local body2
    body2="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q affinity-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")" \
        || die "a sessionAffinity: ClientIP Service did not route at all"
    assert_contains "$body2" "affinity-marker" "response from a ClientIP-affinity Service"
}

test_headless_service_programs_no_rules_and_does_not_break_others() {
    _require_service_proxy
    # A headless Service has clusterIP: None — there is nothing to DNAT,
    # and build_ruleset() skips it explicitly. The risk isn't a missing
    # rule, it's the literal string "None" reaching the ruleset and taking
    # the whole atomically-applied table down with it, which would break
    # every other Service on the node.
    local headless="svc-headless" probe="svc-headless-probe"
    trap _svc_cleanup EXIT
    _svc_track_pod "$headless-a"; _svc_track_svc "$headless" "$probe"

    _svc_backend_pod "$headless-a" "$headless" headless-marker || die "backend never reached Running"
    _svc_clusterip_service "$headless" "$headless" "  clusterIP: None
"
    _wait_for_ready_endpoint "$headless" 60 || die "headless Service's EndpointSlice never got a ready address"

    assert_not_contains "$(_nft list table inet not_k8s_svc 2>/dev/null)" "None" \
        "the literal 'None' must never reach the ruleset"

    # A Service created after the headless one can only get rules if the
    # rebuild that saw the headless Service was accepted by the kernel.
    _svc_clusterip_service "$probe" "$headless"
    _wait_for_ready_endpoint "$probe" 60 || die "probe Service's EndpointSlice never got a ready address"

    local cluster_ip body
    cluster_ip="$(kctl get service "$probe" -o jsonpath='{.spec.clusterIP}')"
    body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q headless-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")" \
        || { echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
             die "a headless Service broke routing for everything else — check build_ruleset()'s clusterIP == \"None\" skip"; }
    assert_contains "$body" "headless-marker" "a Service created alongside a headless one still routes"
}

# ── Restart ───────────────────────────────────────────────────────────
# Deferred to the end of the run automatically: harness.sh's
# _reorder_env_reconfiguring_tests_last greps each test's own source for
# nodeproxy_restart_*, same as it already does for nodelet restarts.

test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart() {
    _require_service_proxy
    nodeproxy_restart_supported || skip_test "needs systemd with a nodeproxy.service unit"
    # nodeproxy holds all Service/EndpointSlice state in memory and rebuilds
    # the entire table on every event; a restart drops that state and
    # relists (apply_event()'s Init arm clears the mirror). If the rebuild
    # after a relist were incomplete, a restart would silently drop routing
    # for Services that existed before it — and nothing else in this suite
    # would notice, since every other test creates its Service after
    # nodeproxy is already running.
    local svc="svc-restart"
    trap _svc_cleanup EXIT
    _svc_track_pod "$svc-a"; _svc_track_svc "$svc"
    _SVC_CLEANUP_RESTORE_NODEPROXY=1

    _svc_backend_pod "$svc-a" "$svc" restart-marker || die "backend never reached Running"
    _svc_clusterip_service "$svc" "$svc"
    _wait_for_ready_endpoint "$svc" 60 || die "EndpointSlice never got a ready address"

    local cluster_ip body
    cluster_ip="$(kctl get service "$svc" -o jsonpath='{.spec.clusterIP}')"
    body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q restart-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")" \
        || die "Service never routed before the restart"
    assert_contains "$body" "restart-marker" "response before restarting nodeproxy"

    nodeproxy_restart_plain

    if ! try_wait_until 90 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q restart-marker"; then
        echo "--- nft ruleset after restart ---"; _nft list table inet not_k8s_svc 2>&1 || true
        echo "--- nodeproxy log ---"; sudo journalctl -u nodeproxy -n 30 --no-pager 2>&1 || true
        die "a Service that existed before nodeproxy restarted never got its rules back — the post-relist rebuild is incomplete (apply_event()'s Init arm clears the mirror; every Service must be re-added from the relist)"
    fi
}

register_test test_clusterip_service_routes_to_its_backend_pod
register_test test_nodeport_service_is_reachable_on_the_node_ip
register_test test_service_with_no_endpoints_does_not_wedge_the_ruleset
register_test test_nodeproxy_runs_as_its_own_service_separate_from_nodelet
register_test test_clusterip_is_reachable_from_inside_a_pod
register_test test_a_pod_reaching_its_own_service_gets_hairpin_masquerade
register_test test_multiple_backends_use_the_load_balancing_map_form
register_test test_losing_every_backend_removes_the_dnat_rule
register_test test_deleting_a_service_removes_its_rules
register_test test_session_affinity_client_ip_forces_source_hash
register_test test_headless_service_programs_no_rules_and_does_not_break_others
register_test test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart
