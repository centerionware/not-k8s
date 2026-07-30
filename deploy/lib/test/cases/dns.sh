# lib/test/cases/dns.sh — dnsPolicy/dnsConfig -> the container's real
# /etc/resolv.conf. containerd writes that file inside the container's own
# mount namespace (not somewhere nodelet materializes on the host), so
# unlike volumes.sh this needs the container to report it itself into a
# shared emptyDir.

test_custom_dns_config_reaches_resolv_conf() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="dns-config"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  dnsPolicy: None
  dnsConfig:
    nameservers: ["203.0.113.53"]
    searches: ["example.test"]
    options:
      - name: ndots
        value: "3"
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /etc/resolv.conf > /shared/resolv.conf; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local resolv
    resolv="$(wait_for_check_file "$name" shared resolv.conf 30)"
    assert_contains "$resolv" "203.0.113.53" "custom nameserver"
    assert_contains "$resolv" "example.test" "custom search domain"
    assert_contains "$resolv" "ndots:3" "custom option"
    delete_pod_if_exists "$name"
}

register_test test_custom_dns_config_reaches_resolv_conf
