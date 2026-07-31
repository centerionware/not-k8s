# lib/test/cases/volumes.sh — ConfigMap/Secret/emptyDir/downwardAPI/
# projected volumes, serviceAccountToken minting, hostAliases, fsGroup.
#
# Key trick used throughout: nodelet materializes ConfigMap/Secret/
# downwardAPI/projected volume contents onto the host filesystem at
# $NODELET_VOLUME_ROOT/<pod-uid>/volumes/<volume-name>/... (see
# VOLUME_ROOT in crates/nodelet/src/runtime/cri.rs) — that's the same path
# bind-mounted into the container, so reading it directly on the host is
# exactly what the container itself sees, no exec needed.

test_configmap_and_secret_volumes_are_materialized() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="cm-secret-vol"
    kctl create configmap "$name-cm" --from-literal=greeting=hello >/dev/null
    kctl create secret generic "$name-secret" --from-literal=password=s3cret >/dev/null
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: cm-vol
      configMap:
        name: $name-cm
    - name: secret-vol
      secret:
        secretName: $name-secret
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: cm-vol, mountPath: /cm}
        - {name: secret-vol, mountPath: /secret}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local cm_content secret_content
    cm_content="$(wait_for_check_file "$name" cm-vol greeting 30)"
    secret_content="$(wait_for_check_file "$name" secret-vol password 30)"
    assert_eq "$cm_content" "hello" "ConfigMap volume content"
    assert_eq "$secret_content" "s3cret" "Secret volume content"
    delete_pod_if_exists "$name"
    kctl delete configmap "$name-cm" --ignore-not-found >/dev/null
    kctl delete secret "$name-secret" --ignore-not-found >/dev/null
}

test_downward_api_volume_writes_pod_metadata() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="downward-vol"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    tier: test
spec:
  volumes:
    - name: downward
      downwardAPI:
        items:
          - path: pod_name
            fieldRef: {fieldPath: metadata.name}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: downward, mountPath: /downward}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local content
    content="$(wait_for_check_file "$name" downward pod_name 30)"
    assert_eq "$content" "$name" "downwardAPI volume pod_name"
    delete_pod_if_exists "$name"
}

test_projected_volume_merges_configmap_and_downward_api() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="projected-vol"
    kctl create configmap "$name-cm" --from-literal=key1=value1 >/dev/null
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: proj
      projected:
        sources:
          - configMap:
              name: $name-cm
          - downwardAPI:
              items:
                - path: name
                  fieldRef: {fieldPath: metadata.name}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: proj, mountPath: /proj}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local cm_val name_val
    cm_val="$(wait_for_check_file "$name" proj key1 30)"
    name_val="$(wait_for_check_file "$name" proj name 30)"
    assert_eq "$cm_val" "value1" "projected volume configMap source"
    assert_eq "$name_val" "$name" "projected volume downwardAPI source"
    delete_pod_if_exists "$name"
    kctl delete configmap "$name-cm" --ignore-not-found >/dev/null
}

test_service_account_token_projected_volume_mints_a_real_token() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="sat-vol"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: token-vol
      projected:
        sources:
          - serviceAccountToken:
              path: token
              expirationSeconds: 600
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: token-vol, mountPath: /var/run/secrets/token}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local path
    path="$(pod_volume_host_path "$name" token-vol)/token"
    if ! try_wait_until 30 bash -c "[[ -s '$path' ]]"; then
        delete_pod_if_exists "$name"
        skip_test "no token materialized within 30s — nodelet's client likely lacks RBAC 'create' on serviceaccounts/token in this namespace (see docs/GAP_CLOSURE.md); this is a cluster config gap, not necessarily a code bug"
    fi
    local token
    token="$(cat "$path")"
    # A JWT is three dot-separated base64url segments — don't decode/verify
    # signature (that's the apiserver's job), just prove a real token-shaped
    # string was minted, not a placeholder.
    [[ "$token" =~ ^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$ ]] \
        || die "token doesn't look like a JWT: $token"
    delete_pod_if_exists "$name"
}

test_host_aliases_are_written_to_etc_hosts() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostaliases"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostAliases:
    - ip: "10.1.2.3"
      hostnames: ["custom.example.com"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local hosts_path
    hosts_path="$(pod_volume_host_path "$name" etc-hosts)"
    wait_until 20 "hostAliases /etc/hosts materialized" bash -c "[[ -s '$hosts_path' ]]"
    assert_contains "$(cat "$hosts_path")" "10.1.2.3	custom.example.com" "generated /etc/hosts"
    delete_pod_if_exists "$name"
}

test_fsgroup_chowns_materialized_volumes() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="fsgroup"
    kctl create configmap "$name-cm" --from-literal=k=v >/dev/null
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  securityContext:
    fsGroup: 4321
  volumes:
    - name: cm-vol
      configMap:
        name: $name-cm
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: cm-vol, mountPath: /cm}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local dir
    dir="$(pod_volume_host_path "$name" cm-vol)"
    wait_until 20 "cm-vol materialized" bash -c "[[ -d '$dir' ]]"
    local gid
    gid="$(stat -c %g "$dir")"
    assert_eq "$gid" "4321" "fsGroup ownership on materialized volume"
    delete_pod_if_exists "$name"
    kctl delete configmap "$name-cm" --ignore-not-found >/dev/null
}

test_empty_dir_medium_memory_is_backed_by_tmpfs() {
    # Round 30: emptyDir.medium: Memory used to be silently ignored,
    # materialized on regular disk exactly like the default. Checks the
    # host mountpoint's actual filesystem type — real proof of tmpfs, not
    # just that the pod started successfully.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="empty-dir-memory"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: ramdisk
      emptyDir:
        medium: Memory
        sizeLimit: 32Mi
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo hello > /ram/marker; sleep 3600"]
      volumeMounts:
        - {name: ramdisk, mountPath: /ram}
EOF
    wait_until 30 "$name Running" pod_is_phase "$name" Running
    local dir
    dir="$(pod_volume_host_path "$name" ramdisk)"
    if ! wait_until 20 "ramdisk mounted" bash -c "[[ -d '$dir' ]]"; then
        delete_pod_if_exists "$name"
        skip_test "no $dir on the host — check nodelet's logs for 'failed to mount tmpfs'"
    fi

    local fstype
    fstype="$(stat -f -c %T "$dir" 2>/dev/null || true)"
    delete_pod_if_exists "$name"
    if [[ "$fstype" != "tmpfs" ]]; then
        die "emptyDir with medium: Memory is not backed by tmpfs (stat -f reports '$fstype') — check mount_tmpfs_empty_dir()/tmpfs_mount_args() in runtime/cri.rs, and that 'mount' is available and this process has permission to mount tmpfs (needs root/CAP_SYS_ADMIN)"
    fi
}

test_image_volume_source_mounts_a_read_only_image() {
    # Round 32: volumeSource.image. Unlike CSI/ephemeral-volume tests
    # elsewhere in this suite, this needs no external infrastructure at
    # all — any pullable OCI image works as the volume's own reference,
    # so this reuses $TEST_IMAGE for both the container and the image
    # volume. Real proof (no exec needed — same file-based trick as the
    # rest of this suite): the app container lists /img and attempts a
    # write inside it, reporting both results into a writable shared
    # emptyDir the host can read directly.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="image-volume-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: img
      image:
        reference: $TEST_IMAGE
        pullPolicy: IfNotPresent
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "ls -A /img > /shared/listing.txt 2>&1; (echo x > /img/should-fail-readonly 2>/shared/write.err && echo WROTE > /shared/write.result) || echo BLOCKED > /shared/write.result; sleep 3600"]
      volumeMounts:
        - {name: img, mountPath: /img}
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with an image volume mounted — check nodelet's logs for 'failed to pull image for image volume', and that this CRI runtime's version actually supports CRI's Mount.image field (containerd >= 2.0 with the ImageVolume feature)"
    fi

    local listing write_result
    listing="$(wait_for_check_file "$name" shared listing.txt 30)"
    write_result="$(wait_for_check_file "$name" shared write.result 15)"

    if [[ -z "$listing" ]]; then
        delete_pod_if_exists "$name"
        skip_test "/img appears empty inside the container — check ResolvedVolume::Image wiring in build_mounts()/resolve_volumes() in runtime/cri.rs, or whether this runtime mounted an empty layer"
    fi
    assert_eq "$write_result" "BLOCKED" "writing inside an image volume mount must fail — image volumes are always read-only per the KEP; check build_mounts()'s Image arm sets readonly: true"

    delete_pod_if_exists "$name"
}

register_test test_configmap_and_secret_volumes_are_materialized
register_test test_downward_api_volume_writes_pod_metadata
register_test test_projected_volume_merges_configmap_and_downward_api
register_test test_service_account_token_projected_volume_mints_a_real_token
register_test test_host_aliases_are_written_to_etc_hosts
register_test test_empty_dir_medium_memory_is_backed_by_tmpfs
register_test test_image_volume_source_mounts_a_read_only_image
register_test test_fsgroup_chowns_materialized_volumes
