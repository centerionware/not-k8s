# lib/test/cases/bootstrap.sh — NODELET_BOOTSTRAP_KUBECONFIG (round 96):
# the CSR-based initial-client-cert-issuance flow real kubelet runs when
# started with --bootstrap-kubeconfig. Nodelet generates a keypair,
# submits a certificates.k8s.io/v1 CertificateSigningRequest (signerName
# kubernetes.io/kube-apiserver-client-kubelet, CN=system:node:<node>,
# O=system:nodes), waits for the apiserver's own node-authorizer/
# csrapproving controller to approve+sign it, then writes a real
# client-cert kubeconfig to NODELET_KUBECONFIG before the normal client
# is built. Nodelet never self-approves.
#
# The pure decision logic (node_identity_dn(), build_csr_object(),
# csr_outcome(), build_output_kubeconfig()) has full unit-test coverage
# in bootstrap.rs's own tests_pure_functions module. What's NOT
# automatable here: this suite runs against an already-started nodelet
# and can't control its own startup environment or provide it a bootstrap
# token / CSR-approving cluster to bootstrap against (same limitation
# every other opt-in NODELET_* startup-time setting already carries, most
# recently round 94's --config file and round 95's client cert auth).

test_tls_bootstrap_manual_note() {
    skip_test "NODELET_BOOTSTRAP_KUBECONFIG (round 96) exchanges a low-privilege bootstrap credential for a real client-certificate kubeconfig via a certificates.k8s.io CSR -- the pure CSR-building/outcome-decision logic is fully unit-tested (bootstrap.rs's tests_pure_functions module), but exercising the real flow needs a cluster where CSR auto-approval is configured (a real k3s/kubeadm control plane's node-authorizer, or 'kubectl certificate approve' run manually) plus controlling nodelet's own startup environment, neither of which this e2e suite (running against an already-started nodelet against this test cluster) can provide. Manual spot-check: (1) create a low-privilege bootstrap kubeconfig authenticating as a bootstrap token or a user that can only create/get certificates.k8s.io/v1 CertificateSigningRequest objects (see https://kubernetes.io/docs/reference/access-authn-authz/kubelet-tls-bootstrapping/ for the standard ClusterRoleBinding), (2) start nodelet with NODELET_BOOTSTRAP_KUBECONFIG=/path/to/bootstrap-kubeconfig and NODELET_KUBECONFIG=/var/lib/nodelet/kubeconfig pointing at an empty/nonexistent path, (3) confirm a CertificateSigningRequest named nodelet-<node>-* appears ('kubectl get csr'), signerName kubernetes.io/kube-apiserver-client-kubelet, requesting CN=system:node:<node>/O=system:nodes, (4) approve it ('kubectl certificate approve nodelet-<node>-xxxxx') if no auto-approver is configured, (5) confirm nodelet logs 'TLS bootstrap: issued client certificate written' and /var/lib/nodelet/kubeconfig now exists with client-certificate-data/client-key-data populated, (6) restart nodelet with the same env vars and confirm it logs 'kubeconfig already present, reusing it' instead of submitting a second CSR (proof of the documented no-rotation-yet scope simplification)."
}

register_test test_tls_bootstrap_manual_note
