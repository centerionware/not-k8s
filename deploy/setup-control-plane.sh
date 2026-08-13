#!/usr/bin/env bash
# setup-control-plane.sh — Install and start a stripped k3s control plane (NO agent/kubelet).
#
# This gives us a real Kubernetes apiserver + scheduler + controller-manager + kine/sqlite
# datastore — the full kubectl/CRD API surface — without the heavy kubelet, containerd,
# or any optional add-ons.  The "nodelet" binary replaces the kubelet on the node side.
#
# Usage:
#   sudo ./setup-control-plane.sh          # install + start k3s control-plane-only
#   export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
#   kubectl get nodes                       # empty until nodelet registers
#
set -euo pipefail

# ── Preflight checks ────────────────────────────────────────────────────────

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: This script must be run as root (or with sudo)." >&2
    echo "       k3s installs systemd units and writes to /etc/rancher/." >&2
    exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|aarch64|arm64|armv7l) ;;  # supported by k3s
    *)
        echo "ERROR: Unsupported architecture: $ARCH" >&2
        echo "       k3s supports x86_64, aarch64, and armv7l." >&2
        exit 1
        ;;
esac

echo "==> Architecture: $ARCH"
echo "==> Setting up stripped k3s control plane (apiserver + scheduler + controller-manager only)"
echo ""

# ── Install k3s if missing ──────────────────────────────────────────────────

if command -v k3s &>/dev/null; then
    echo "==> k3s binary already present: $(k3s --version)"
else
    echo "==> Installing k3s via official installer..."
    curl -sfL https://get.k3s.io | sh -s - "" # args come from INSTALL_K3S_EXEC below
fi

# ── Configure and (re)start k3s with agent disabled ─────────────────────────
#
# Key flags explained:
#
#   --disable-agent
#       THE critical flag.  Tells k3s NOT to start its built-in kubelet or
#       containerd.  This is the entire point: the heavy node agent is replaced
#       by our lean Rust "nodelet" binary.  Without this flag, k3s runs a full
#       kubelet (~120 MB RSS, PLEG polling, cAdvisor, etc.) that we don't want.
#
#   --disable traefik
#   --disable servicelb
#   --disable local-storage
#   --disable metrics-server
#       Turn off all bundled add-on controllers.  They're not needed on a
#       single-device edge deployment and each wastes memory + watch traffic.
#
#   --disable-cloud-controller
#       No cloud provider integration — we're an edge device.
#
#   --disable-network-policy
#       Network-policy controller is not needed without a real CNI.
#
#   --flannel-backend=none
#       Don't run k3s's own embedded flannel controller — that code path lives
#       in the agent/kubelet we're not running anyway. This is k3s's documented
#       way to bring your own CNI: node PodCIDR allocation on the
#       controller-manager (--allocate-node-cidrs) stays on regardless of this
#       flag, so an externally-run flannel (see deploy/bootstrap-source.sh
#       --with-cri, which installs and starts it) can still read/assign
#       per-node subnets from Node objects via the "kube" subnet manager.
#
#   --kube-controller-manager-arg=node-monitor-period=10s
#       How often the controller-manager checks node health via the Lease.
#       10s is a sane default that matches nodelet's heartbeat interval.
#       (Stock k8s is 5s; we're slightly relaxed for lower idle CPU.)
#
#   --write-kubeconfig-mode=0644
#       Make the kubeconfig world-readable so non-root users can export it.
#       Acceptable on a single-user edge device.
#
#   --cluster-cidr / --service-cidr
#       Comma-separated v4,v6 pairs turn on dual-stack Service/Pod IP
#       allocation (this is what --allocate-node-cidrs, which stays on
#       regardless of --flannel-backend, actually allocates from). Defaults
#       below are IPv4-only; deploy/bootstrap-source.sh overrides both via
#       NOTK8S_CLUSTER_CIDR/NOTK8S_SERVICE_CIDR once it's resolved
#       --ip-family, so this script and nodelet's Service proxy always agree
#       on the same mode.
#
#   --kube-apiserver-arg=feature-gates=ServiceAccountTokenPodNodeInfo=true
#       Real Kubernetes 1.36+ feature gate, off by default upstream too.
#       A DRA driver's ResourceSlice objects (published per-node, describing
#       what devices it has to offer) get a ValidatingAdmissionPolicy in
#       front of them (the reference kubernetes-sigs/dra-example-driver's
#       own Helm chart ships one) that checks the publishing service
#       account token actually carries pod/node identity info — without
#       this gate, every ResourceSlice create is rejected with "no node
#       association found for user", and DRA-based device allocation (CDI/
#       GPU passthrough) can never advertise anything to allocate. Found
#       live standing up a real DRA driver for e2e testing (round 121).
#
#   --kube-apiserver-arg=kubelet-certificate-authority=...
#       Only added once NOTK8S_KUBELET_CA_PEM is set (deploy/bootstrap-
#       source.sh does this in a *second* call to this script, after
#       nodelet's own first startup has actually generated the cert this
#       needs to point at — see enable_kubelet_certificate_authority_trust()
#       in lib/control-plane.sh for why it can't just be included from the
#       first call). Without it, the apiserver refuses to trust nodelet's
#       self-signed kubelet-style server cert at all ("certificate signed
#       by unknown authority"), and containerLogs/exec/attach/port-forward
#       proxying is unreachable through it — confirmed for real. This
#       trusts nodelet's own leaf cert directly as if it were a CA (valid
#       x509: a self-signed cert can vouch for itself when explicitly
#       placed in a trust store this way) — not a real CA/CSR pipeline,
#       which stays a separate, lower-priority open item (docs/GAP_CLOSURE.md).

CLUSTER_CIDR="${NOTK8S_CLUSTER_CIDR:-10.42.0.0/16}"
SERVICE_CIDR="${NOTK8S_SERVICE_CIDR:-10.43.0.0/16}"
KUBELET_CA_ARG=""
[[ -n "${NOTK8S_KUBELET_CA_PEM:-}" ]] && KUBELET_CA_ARG="--kube-apiserver-arg=kubelet-certificate-authority=$NOTK8S_KUBELET_CA_PEM"

# --datastore-endpoint: point k3s at our own datastore (crates/nodestore —
# the etcd v3 API over sqlite) instead of its bundled kine. Unset by default,
# in which case k3s uses kine exactly as before and nothing here changes.
#
# k3s treats an etcd endpoint here as a real external etcd, which is the whole
# point: nodestore speaks the etcd v3 gRPC API, so the apiserver needs no
# knowledge that it isn't etcd. Set by bootstrap-source.sh only after the
# store is confirmed listening — k3s fails to start, repeatedly, if this
# points at nothing.
DATASTORE_ARG=""
if [[ -n "${NOTK8S_DATASTORE_ENDPOINT:-}" ]]; then
    DATASTORE_ARG="--datastore-endpoint=$NOTK8S_DATASTORE_ENDPOINT"
    # Checked here rather than left to fail later: with an https endpoint and
    # no client certificate, k3s installs perfectly happily and then simply
    # never becomes ready, and the only symptom is this script's own 60s
    # readiness loop timing out with a message that names none of this.
    if [[ "$NOTK8S_DATASTORE_ENDPOINT" == https://* ]]; then
        for _v in NOTK8S_DATASTORE_CAFILE NOTK8S_DATASTORE_CERTFILE NOTK8S_DATASTORE_KEYFILE; do
            if [[ ! -s "${!_v:-}" ]]; then
                echo "ERROR: $NOTK8S_DATASTORE_ENDPOINT needs a client certificate, but $_v is unset, empty, or names a file that does not exist." >&2
                echo "       The datastore has no plaintext mode — it holds every Secret in the cluster and the" >&2
                echo "       etcd v3 API has no authentication of its own, so mutual TLS is the only way in." >&2
                echo "       Without these three, k3s would install and then never become ready." >&2
                exit 1
            fi
        done
    fi
    # The datastore requires TLS with a client certificate — it holds every
    # Secret in the cluster and the etcd v3 API has no authentication of its
    # own, so there is no plaintext mode to fall back to. These are k3s's
    # spellings of kube-apiserver's --etcd-cafile/--etcd-certfile/
    # --etcd-keyfile. Without them k3s dials in the clear and the handshake
    # fails, which surfaces as an apiserver that never becomes ready.
    if [[ -n "${NOTK8S_DATASTORE_CAFILE:-}" ]]; then
        DATASTORE_ARG="$DATASTORE_ARG --datastore-cafile=$NOTK8S_DATASTORE_CAFILE"
    fi
    if [[ -n "${NOTK8S_DATASTORE_CERTFILE:-}" ]]; then
        DATASTORE_ARG="$DATASTORE_ARG --datastore-certfile=$NOTK8S_DATASTORE_CERTFILE"
    fi
    if [[ -n "${NOTK8S_DATASTORE_KEYFILE:-}" ]]; then
        DATASTORE_ARG="$DATASTORE_ARG --datastore-keyfile=$NOTK8S_DATASTORE_KEYFILE"
    fi
fi

# ── Our scheduler instead of k3s's ──────────────────────────────────────────
#
# SCHEDULER=nodescheduler (bootstrap-source.sh --scheduler=nodescheduler) means
# crates/nodescheduler is installed as its own service and does the placing, so
# k3s must stop running its own kube-scheduler in-process. These are two halves
# of one switch, not independent knobs: leaving both on gives two schedulers
# watching the same unbound pods and both writing Bindings, which is a race
# whose usual symptom is a pod bound to a node that a different scheduler
# already rejected — intermittent, and very hard to read backwards from.
#
# Deliberately not defaulted on: with SCHEDULER unset this expands to nothing
# and the control plane is exactly what every release so far shipped.
SCHEDULER_ARG=""
if [[ "${SCHEDULER:-none}" == "nodescheduler" ]]; then
    SCHEDULER_ARG="--disable-scheduler"
    echo "==> SCHEDULER=nodescheduler — disabling k3s's own kube-scheduler; ours will do the placing."
    echo "    (Until nodescheduler is running, new pods stay Pending. That is expected, not a hang.)"
fi

export INSTALL_K3S_EXEC="server \
    --disable-agent \
    $SCHEDULER_ARG \
    --disable traefik \
    --disable servicelb \
    --disable local-storage \
    --disable metrics-server \
    --disable-cloud-controller \
    --disable-network-policy \
    --flannel-backend=none \
    --cluster-cidr=$CLUSTER_CIDR \
    --service-cidr=$SERVICE_CIDR \
    --kube-controller-manager-arg=node-monitor-period=10s \
    --kube-apiserver-arg=feature-gates=ServiceAccountTokenPodNodeInfo=true \
    $KUBELET_CA_ARG \
    $DATASTORE_ARG \
    --write-kubeconfig-mode=0644"

echo "==> Starting k3s with INSTALL_K3S_EXEC:"
echo "    $INSTALL_K3S_EXEC"
echo ""

# If k3s was already installed, we still need to ensure the service is running
# with the correct flags.  The installer script handles both fresh install and
# reconfiguration when INSTALL_K3S_EXEC is set.
curl -sfL https://get.k3s.io | sh -

# ── Wait for the apiserver to become reachable ───────────────────────────────

echo ""
echo "==> Waiting for k3s apiserver to become ready..."

KUBECONFIG=/etc/rancher/k3s/k3s.yaml
MAX_WAIT=60
WAITED=0
until k3s kubectl --kubeconfig="$KUBECONFIG" get --raw /readyz &>/dev/null; do
    if (( WAITED >= MAX_WAIT )); then
        echo "ERROR: apiserver not ready after ${MAX_WAIT}s.  Check: journalctl -u k3s" >&2
        exit 1
    fi
    sleep 2
    WAITED=$((WAITED + 2))
    echo "    ... waited ${WAITED}s"
done

echo "==> apiserver is ready (waited ${WAITED}s)."
echo ""

# ── Print kubeconfig info ────────────────────────────────────────────────────

echo "==> Kubeconfig written to: $KUBECONFIG"
echo ""
echo "    Run this in your shell (or add to ~/.bashrc):"
echo ""
echo "        export KUBECONFIG=$KUBECONFIG"
echo ""

# Verify: there should be NO nodes yet (the agent is disabled).
NODE_COUNT=$(k3s kubectl --kubeconfig="$KUBECONFIG" get nodes --no-headers 2>/dev/null | wc -l)
if (( NODE_COUNT == 0 )); then
    echo "==> Verified: 0 nodes registered (expected — no agent running)."
else
    echo "==> Warning: $NODE_COUNT node(s) already registered.  If a previous k3s"
    echo "    agent is still running, stop it: systemctl stop k3s-agent"
fi

# ── Next steps ───────────────────────────────────────────────────────────────

echo ""
echo "==> Next steps:"
echo ""
echo "  1. Build nodelet (if not already built):"
echo "       cd /workspace/not-k8s"
echo "       cargo build --release                    # mock runtime (no containerd needed)"
echo "       # or: cargo build --release --features cri  # real containerd runtime"
echo ""
echo "  2. Run nodelet:"
echo "       export KUBECONFIG=$KUBECONFIG"
echo "       ./deploy/run-nodelet.sh"
echo ""
echo "  3. Verify the node registered:"
echo "       kubectl get nodes"
echo ""
echo "  4. Deploy a test workload:"
echo "       kubectl apply -f deploy/demo-pod.yaml"
echo "       kubectl get pods -w"
echo ""
echo "==> Done.  Control plane is running (apiserver + scheduler + controller-manager)."
echo "    No kubelet, no containerd, no add-ons.  Lean and clean."
