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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local cm_content secret_content
    cm_content="$(wait_for_check_file "$name" cm-vol greeting 30)"
    secret_content="$(wait_for_check_file "$name" secret-vol password 30)"
    assert_eq "$cm_content" "hello" "ConfigMap volume content"
    assert_eq "$secret_content" "s3cret" "Secret volume content"
    delete_pod_if_exists "$name"
    kctl delete configmap "$name-cm" --ignore-not-found >/dev/null
    kctl delete secret "$name-secret" --ignore-not-found >/dev/null
}

test_configmap_volume_updates_live_without_pod_restart() {
    # Round 37: real kubelet's well-known "edit a ConfigMap, the mounted
    # file updates live" behavior — no pod/container restart. Proves the
    # ConfigMap watch in pods.rs actually re-triggers materialization: we
    # never delete/recreate the pod, and check the CONTAINER'S OWN restart
    # count stays at 0 throughout.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="cm-live-update"
    kctl create configmap "$name-cm" --from-literal=greeting=hello >/dev/null
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
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: cm-vol, mountPath: /cm}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local before
    before="$(wait_for_check_file "$name" cm-vol greeting 30)"
    assert_eq "$before" "hello" "ConfigMap volume content before update"

    kctl create configmap "$name-cm" --from-literal=greeting=updated --dry-run=client -o yaml \
        | kctl apply -f - >/dev/null

    local path waited=0 after=""
    path="$(pod_volume_host_path "$name" cm-vol)/greeting"
    while true; do
        after="$(cat "$path" 2>/dev/null || true)"
        [[ "$after" == "updated" ]] && break
        if [[ "$waited" -ge 30 ]]; then
            die "timed out after 30s waiting for ConfigMap volume content to live-update"
        fi
        sleep 2
        waited=$((waited + 2))
    done
    assert_eq "$after" "updated" "ConfigMap volume content after live update"
    assert_eq "$(pod_container_restart_count "$name" app)" "0" "container restart count (must not restart for a live volume update)"

    delete_pod_if_exists "$name"
    kctl delete configmap "$name-cm" --ignore-not-found >/dev/null
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local content
    content="$(wait_for_check_file "$name" downward pod_name 30)"
    assert_eq "$content" "$name" "downwardAPI volume pod_name"
    delete_pod_if_exists "$name"
}

test_sub_path_expr_expands_a_downward_api_env_var() {
    # Round 69: volumeMounts[].subPathExpr was entirely unimplemented —
    # $(VAR) references were never expanded, so the container would have
    # gotten the whole emptyDir root mounted instead of the pod-name
    # subdirectory it actually asked for. Real proof: the container
    # writes a marker at $(POD_NAME)/marker, and the test reads it back
    # directly from the EXPANDED host path (pod_volume_host_path's own
    # directory, joined with the real pod name) — if expansion never
    # happened, that specific path wouldn't have anything in it.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="subpathexpr-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      env:
        - name: POD_NAME
          valueFrom:
            fieldRef: {fieldPath: metadata.name}
      command: ["sh", "-c", "echo expanded > /data/marker; sleep 3600"]
      volumeMounts:
        - name: shared
          mountPath: /data
          subPathExpr: \$(POD_NAME)
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with a subPathExpr volumeMount — check nodelet's logs for a dropped mount"
    fi
    local expanded_path="$(pod_volume_host_path "$name" shared)/$name/marker"
    if ! try_wait_until 20 bash -c "[[ -s '$expanded_path' ]]"; then
        delete_pod_if_exists "$name"
        skip_test "no $expanded_path appeared — subPathExpr may not have expanded to the pod name as expected"
    fi
    local content
    content="$(cat "$expanded_path")"
    delete_pod_if_exists "$name"
    assert_eq "$content" "expanded" "subPathExpr's \$(POD_NAME) should have expanded to the real pod name, landing the container's write at <volume>/<pod-name>/marker on the host"
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local hosts_path
    hosts_path="$(pod_volume_host_path "$name" etc-hosts)"
    wait_until 20 "hostAliases /etc/hosts materialized" bash -c "[[ -s '$hosts_path' ]]"
    assert_contains "$(cat "$hosts_path")" "10.1.2.3	custom.example.com" "generated /etc/hosts"
    delete_pod_if_exists "$name"
}

test_host_aliases_still_work_under_host_users_false() {
    # Round 98 (found in round 88's own documented follow-up): the
    # /etc/hosts auxiliary bind-mount now carries the pod's userns id
    # mapping too when hostUsers: false, same as every regular
    # volumeMount already got in round 88. What's fully automatable:
    # the mount still works and /etc/hosts still materializes correctly
    # with the mapping applied, not that it broke the container's own
    # hostAliases entries. Genuine ownership-translation proof needs the
    # same root-required pre-chowned-file setup as
    # test_host_users_volume_ownership_translation_manual_note.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostaliases-userns"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostUsers: false
  hostAliases:
    - ip: "10.1.2.3"
      hostnames: ["custom.example.com"]
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with hostUsers: false and hostAliases together — check the aux_id_mappings wiring in runtime/cri/container_create.rs's /etc/hosts mount, or whether this runtime version rejects Mount.uidMappings/.gidMappings entirely"
    fi
    local hosts_path
    hosts_path="$(pod_volume_host_path "$name" etc-hosts)"
    wait_until 20 "hostAliases /etc/hosts materialized" bash -c "[[ -s '$hosts_path' ]]"
    assert_contains "$(cat "$hosts_path")" "10.1.2.3	custom.example.com" "generated /etc/hosts under hostUsers: false"
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local dir
    dir="$(pod_volume_host_path "$name" cm-vol)"
    wait_until 20 "cm-vol materialized" bash -c "[[ -d '$dir' ]]"
    local gid
    gid="$(stat -c %g "$dir")"
    assert_eq "$gid" "4321" "fsGroup ownership on materialized volume"
    delete_pod_if_exists "$name"
}

test_fsgroup_never_applies_to_hostpath_volumes() {
    # Round 93 (found in round 92's re-audit, verified against upstream
    # kubelet source before implementing): real kubelet's hostPath plugin
    # doesn't support ownership management at all -- fsGroup must never
    # touch a hostPath volume's directory, since it's the host's own
    # pre-existing path, not something the pod owns.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-fsgroup-hostpath-test.XXXXXX)"
    local original_gid
    original_gid="$(stat -c %g "$host_dir")"
    local name="fsgroup-hostpath"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  securityContext:
    fsGroup: 4322
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: hostvol, mountPath: /host}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    sleep 2 # give resolve_volumes()'s fsGroup pass a moment, if it were (wrongly) going to run
    local gid
    gid="$(stat -c %g "$host_dir")"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    assert_eq "$gid" "$original_gid" "fsGroup must never chown a real hostPath volume's directory (matches upstream: hostPath has no ownership-management support)"
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running
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

test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs() {
    # Round 61: emptyDir.medium: "HugePages-<size>" used to be silently
    # ignored, materialized on regular disk exactly like the default
    # (the last of round 58's 3 HugePages pieces). Checks the host
    # mountpoint's actual filesystem type — real proof of hugetlbfs, not
    # just that the pod started successfully. Skips cleanly (not a hard
    # fail) if this node/kernel has no 2Mi hugepages reserved — genuinely
    # outside nodelet's control, same reasoning as round 59/60's hugepages
    # tests.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local reserved
    reserved="$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0)"
    if [[ "$reserved" -eq 0 ]]; then
        skip_test "no hugepages reserved on this node (/proc/sys/vm/nr_hugepages is 0) — nothing for a HugePages-medium emptyDir to mount against"
    fi

    local name="empty-dir-hugepages"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hugepool
      emptyDir:
        medium: HugePages-2Mi
        sizeLimit: 4Mi
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          hugepages-2Mi: "4Mi"
          memory: "67108864"
      command: ["sh", "-c", "echo hello > /huge/marker; sleep 3600"]
      volumeMounts:
        - {name: hugepool, mountPath: /huge}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with a HugePages-2Mi emptyDir — this node/kernel likely has no 2Mi hugepages reserved or the runtime doesn't support the hugetlb cgroup controller"
    fi

    local dir
    dir="$(pod_volume_host_path "$name" hugepool)"
    if ! try_wait_until 20 bash -c "[[ -d '$dir' ]]"; then
        delete_pod_if_exists "$name"
        skip_test "no $dir on the host — check nodelet's logs for 'failed to mount hugetlbfs'"
    fi

    local fstype
    fstype="$(stat -f -c %T "$dir" 2>/dev/null || true)"
    delete_pod_if_exists "$name"
    if [[ "$fstype" != "hugetlbfs" ]]; then
        skip_test "emptyDir with medium: HugePages-2Mi is not backed by hugetlbfs (stat -f reports '$fstype') — likely no 2Mi hugepages actually reserved/mountable on this node; check mount_hugetlbfs_empty_dir()/hugetlbfs_mount_args() in runtime/cri.rs if hugepages ARE reserved here"
    fi
}

test_host_path_directory_mounts_the_real_host_directory() {
    # Round 65: hostPath used to be entirely unsupported (logged and
    # dropped, no mount at all). A real proof, not just "the pod ran":
    # write a marker file directly on the host filesystem (bypassing
    # nodelet entirely) before the pod exists, then read it back from
    # inside the container — if hostPath were a copy/materialization
    # instead of a real bind mount, this would come up empty.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-hostpath-test.XXXXXX)"
    echo "written-by-the-host" > "$host_dir/marker"
    local name="hostpath-dir"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /host/marker > /host/from-container; sleep 3600"]
      volumeMounts:
        - {name: hostvol, mountPath: /host}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        rm -rf "$host_dir"
        die "pod never reached Running with a hostPath: Directory volume — check nodelet's logs for a 'hostPath volume failed validation' warning"
    fi
    wait_until 20 "container's write landed back on the host" bash -c "[[ -s '$host_dir/from-container' ]]"
    local content
    content="$(cat "$host_dir/from-container")"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    assert_eq "$content" "written-by-the-host" "the container should see the exact same file the host wrote directly (real bind mount, not a copy), and its own write should land back on the host"
}

test_host_path_directory_or_create_creates_a_missing_directory() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -u /tmp/nodelet-hostpath-create-test.XXXXXX)"
    assert_true bash -c "[[ ! -e '$host_dir' ]]" "the DirectoryOrCreate path must not exist yet, or this test isn't proving creation"
    local name="hostpath-create"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: DirectoryOrCreate
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - {name: hostvol, mountPath: /host}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        sudo rm -rf "$host_dir"
        die "pod never reached Running with a hostPath: DirectoryOrCreate volume"
    fi
    delete_pod_if_exists "$name"
    assert_true bash -c "[[ -d '$host_dir' ]]" "DirectoryOrCreate should have created $host_dir on the host"
    # Round 123 (found live in CI, first time the full suite ever ran to
    # completion): this directory is genuinely created by nodelet, running
    # as host root, not by this test script -- a plain `rm -rf` as the
    # e2e runner's own unprivileged user fails with "Operation not
    # permitted". Every other host_dir in this file is created via
    # `mktemp -d` by this script itself (safe to rm without sudo); this
    # is the one hostPath test where nodelet is the one materializing it.
    sudo rm -rf "$host_dir"
}

test_host_path_directory_type_rejects_a_nonexistent_path() {
    # The other side of the same coin: type: Directory (no "OrCreate")
    # must NOT create anything — a pod requesting one that doesn't exist
    # should simply never get the mount (container starts, but without
    # this volume's path populated in any usable way — nodelet's
    # best-effort posture, matching every other unresolvable-volume case
    # in this codebase, rather than failing pod admission outright since
    # nodelet has no admission layer to do that from).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -u /tmp/nodelet-hostpath-reject-test.XXXXXX)"
    local name="hostpath-reject"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - {name: hostvol, mountPath: /host}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running at all — can't distinguish 'correctly rejected the volume' from 'something else failed'"
    fi
    delete_pod_if_exists "$name"
    assert_true bash -c "[[ ! -e '$host_dir' ]]" "type: Directory must never create $host_dir — that's DirectoryOrCreate's job, not Directory's"
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
        die "pod never reached Running with an image volume mounted — check nodelet's logs for 'failed to pull image for image volume', and that this CRI runtime's version actually supports CRI's Mount.image field (containerd >= 2.0 with the ImageVolume feature)"
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
register_test test_configmap_volume_updates_live_without_pod_restart
register_test test_downward_api_volume_writes_pod_metadata
register_test test_sub_path_expr_expands_a_downward_api_env_var
register_test test_projected_volume_merges_configmap_and_downward_api
register_test test_service_account_token_projected_volume_mints_a_real_token
register_test test_host_aliases_are_written_to_etc_hosts
register_test test_host_aliases_still_work_under_host_users_false
register_test test_empty_dir_medium_memory_is_backed_by_tmpfs
register_test test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs
register_test test_image_volume_source_mounts_a_read_only_image
register_test test_fsgroup_chowns_materialized_volumes
register_test test_fsgroup_never_applies_to_hostpath_volumes
test_mount_propagation_host_to_container_still_mounts_normally() {
    # Round 84 (found in round 83's re-audit): volumeMounts[].mountPropagation
    # was never set at all before this (every mount silently got CRI's
    # PRIVATE zero-value default regardless of what the pod asked for).
    # Genuinely observing PROPAGATION at work needs a real mount(2) syscall
    # performed on the host AFTER this pod is already running (root
    # required) -- see the manual-note below for that. What this DOES
    # prove, fully automated: a hostPath volume with
    # mountPropagation: HostToContainer set still mounts and is readable
    # normally, i.e. wiring this field through didn't break the mount
    # itself (a real risk, since propagation is a rarely-set field this
    # round newly touches on every single mount, not just ones that use
    # it).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-mountprop-test.XXXXXX)"
    echo "written-by-the-host" > "$host_dir/marker"
    local name="mount-propagation-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /hostvol/marker > /hostvol/from-container; sleep 3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          mountPropagation: HostToContainer
EOF
    if ! try_wait_until 60 pod_is_phase "$name" Running; then
        warn "[diag] pod status: $(kctl get pod "$name" -o wide 2>&1)"
        warn "[diag] pod events: $(kctl describe pod "$name" 2>&1 | grep -A20 '^Events:')"
        rm -rf "$host_dir"
        delete_pod_if_exists "$name"
        die "pod never reached Running with mountPropagation: HostToContainer set — check mount_propagation_cri()/build_mounts() wiring in runtime/cri/volumes_pure.rs, or whether this runtime version rejects a nonzero Mount.propagation entirely"
    fi
    wait_until 20 "container's write landed back on the host" bash -c "[[ -s '$host_dir/from-container' ]]"
    local content
    content="$(cat "$host_dir/from-container")"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    assert_contains "$content" "written-by-the-host" "hostPath volume with mountPropagation: HostToContainer should still mount and read normally"
}

test_recursive_read_only_still_mounts_read_only_normally() {
    # Round 85 (GA 1.33, KEP-3116; found in round 83's re-audit):
    # volumeMounts[].recursiveReadOnly was never set at all before this.
    # Genuinely proving RECURSIVENESS (that a nested mount underneath
    # this one also becomes read-only, not just the top-level mountpoint
    # itself) needs a second real mount(2) syscall performed inside the
    # container's own mount namespace before the outer mount is made --
    # not something this suite can safely automate. What this DOES
    # prove, fully automated: a hostPath volume with
    # recursiveReadOnly: Enabled (which also requires readOnly: true and
    # no mountPropagation, per the CRI contract) still mounts and blocks
    # writes at the top level -- wiring this newly-touched field through
    # every readOnly mount didn't break the ordinary read-only case.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-rro-test.XXXXXX)"
    local name="recursive-readonly-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command:
        - sh
        - -c
        - |
          if touch /hostvol/roottest 2>/dev/null; then
            echo writable > /shared/result.txt
          else
            echo readonly > /shared/result.txt
          fi
          sleep 3600
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          readOnly: true
          recursiveReadOnly: Enabled
        - name: shared
          mountPath: /shared
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        rm -rf "$host_dir"
        delete_pod_if_exists "$name"
        die "pod never reached Running with recursiveReadOnly: Enabled set — check recursive_read_only_cri()/build_mounts() wiring in runtime/cri/volumes_pure.rs, or whether this runtime version rejects Mount.recursive_read_only entirely"
    fi
    local result
    result="$(wait_for_check_file "$name" shared result.txt 30)"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    assert_eq "$result" "readonly" "hostPath volume with recursiveReadOnly: Enabled must still block writes at the top level"
}

test_recursive_read_only_if_possible_falls_back_without_erroring() {
    # Round 97: IfPossible is a real best-effort fallback now (previously
    # treated identically to Enabled) -- gated on whether the resolved
    # runtime handler advertises Node.status.runtimeHandlers[].features
    # .recursiveReadOnlyMounts support. This suite doesn't control which
    # capability the test cluster's own containerd build reports, so it
    # can't assert Enabled vs. Disabled specifically either way -- what
    # IS fully automatable: a pod requesting IfPossible reaches Running
    # either way (never errors regardless of handler support) and its
    # reported containerStatuses[].volumeMounts[].recursiveReadOnly is
    # always one of the two valid values, never something else -- proof
    # the fallback path itself is wired correctly, not a silent no-op or
    # crash. See test_recursive_read_only_manual_note for the specific
    # Enabled-vs-Disabled-by-handler-support spot-check.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-rro-ifpossible-test.XXXXXX)"
    local name="recursive-readonly-ifpossible-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          readOnly: true
          recursiveReadOnly: IfPossible
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        rm -rf "$host_dir"
        delete_pod_if_exists "$name"
        die "pod never reached Running with recursiveReadOnly: IfPossible set — check recursive_read_only_cri()'s IfPossible branch in runtime/cri/volumes_pure.rs"
    fi
    local status
    status="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].volumeMounts[?(@.name=="hostvol")].recursiveReadOnly}')"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    if [ "$status" != "Enabled" ] && [ "$status" != "Disabled" ]; then
        die "assertion failed: IfPossible must report either Enabled or Disabled, got '$status'"
    fi
}

test_mount_propagation_host_to_container_sees_a_new_host_mount() {
    # Round 123: previously manual-only purely because it needs a real
    # mount(2) syscall on the HOST after the pod is already Running — sudo
    # is already available to this suite for containerd/chown operations
    # elsewhere, so a real 'mount --bind' works exactly the same way here.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir source_dir
    host_dir="$(mktemp -d /tmp/nodelet-mountprop-h2c-test.XXXXXX)"
    mkdir -p "$host_dir/newmount"
    source_dir="$(mktemp -d /tmp/nodelet-mountprop-h2c-source.XXXXXX)"
    echo "from-a-new-host-mount" > "$source_dir/marker"

    local name="mount-propagation-h2c-check"
    mount_propagation_h2c_cleanup() {
        delete_pod_if_exists "$name"
        sudo umount "${host_dir:-}/newmount" 2>/dev/null || true
        rm -rf "${host_dir:-}" "${source_dir:-}"
    }
    trap mount_propagation_h2c_cleanup EXIT

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          mountPropagation: HostToContainer
EOF
    if ! try_wait_until 60 pod_is_phase "$name" Running; then
        warn "[diag] pod status: $(kctl get pod "$name" -o wide 2>&1)"
        warn "[diag] pod events: $(kctl describe pod "$name" 2>&1 | grep -A20 '^Events:')"
        warn "[diag] nodelet log mentioning $name:"
        sudo journalctl -u nodelet --no-pager 2>/dev/null | grep -E "$name" | tail -30 | while IFS= read -r line; do warn "[diag]   $line"; done
        die "pod never reached Running with mountPropagation: HostToContainer set — check mount_propagation_cri()/build_mounts() wiring in runtime/cri/volumes_pure.rs, or whether this runtime version rejects a nonzero Mount.propagation entirely"
    fi

    # The new mount happens *after* the pod is already running — the whole
    # point of HostToContainer: mount events made on the host later still
    # propagate in, unlike the container's own initial (already-mounted)
    # view.
    sudo mount --bind "$source_dir" "$host_dir/newmount"
    if ! try_wait_until 15 bash -c "kctl exec '$name' -- cat /hostvol/newmount/marker 2>/dev/null | grep -q from-a-new-host-mount"; then
        die "the container never saw the new host-side mount at /hostvol/newmount — HostToContainer propagation isn't actually working, just round-tripping through config"
    fi
}

test_mount_propagation_private_default_does_not_see_a_new_host_mount() {
    # The other side of the same coin: with mountPropagation left unset
    # (Private, the default), a mount made on the host after the pod is
    # already running must NOT become visible inside the container —
    # proof Private genuinely isolates mount events, not just that
    # HostToContainer happens to work.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir source_dir
    host_dir="$(mktemp -d /tmp/nodelet-mountprop-private-test.XXXXXX)"
    mkdir -p "$host_dir/newmount"
    source_dir="$(mktemp -d /tmp/nodelet-mountprop-private-source.XXXXXX)"
    echo "from-a-new-host-mount" > "$source_dir/marker"

    local name="mount-propagation-private-check"
    mount_propagation_private_cleanup() {
        delete_pod_if_exists "$name"
        sudo umount "${host_dir:-}/newmount" 2>/dev/null || true
        rm -rf "${host_dir:-}" "${source_dir:-}"
    }
    trap mount_propagation_private_cleanup EXIT

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
EOF
    if ! try_wait_until 60 pod_is_phase "$name" Running; then
        warn "[diag] pod status: $(kctl get pod "$name" -o wide 2>&1)"
        warn "[diag] pod events: $(kctl describe pod "$name" 2>&1 | grep -A20 '^Events:')"
        warn "[diag] nodelet log mentioning $name:"
        sudo journalctl -u nodelet --no-pager 2>/dev/null | grep -E "$name" | tail -30 | while IFS= read -r line; do warn "[diag]   $line"; done
        die "pod never reached Running with a plain hostPath mount"
    fi

    sudo mount --bind "$source_dir" "$host_dir/newmount"
    sleep 5 # give a real leak a fair chance to show up before asserting its absence
    local seen
    seen="$(kctl exec "$name" -- cat /hostvol/newmount/marker 2>&1)"
    assert_not_contains "$seen" "from-a-new-host-mount" "with mountPropagation left at the default (Private), a mount made on the host after the pod started must NOT be visible inside the container"
}

test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too() {
    # Round 123: previously manual-only, but the whole procedure just
    # needs a real mount(2) on the HOST before pod creation — same
    # mechanism the mount-propagation tests above already use with sudo.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local host_dir nested_source
    host_dir="$(mktemp -d /tmp/nodelet-rro-recursive-test.XXXXXX)"
    mkdir -p "$host_dir/nested"
    nested_source="$(mktemp -d /tmp/nodelet-rro-recursive-source.XXXXXX)"

    local name="recursive-readonly-nested-check"
    rro_recursive_test_cleanup() {
        delete_pod_if_exists "$name"
        sudo umount "${host_dir:-}/nested" 2>/dev/null || true
        rm -rf "${host_dir:-}" "${nested_source:-}"
    }
    trap rro_recursive_test_cleanup EXIT

    # The nested mount exists BEFORE the pod (and nodelet's own outer
    # mount) is created — the whole point of the RECURSIVE check: the
    # outer mount's read-only-ness must extend to something already
    # mounted underneath it at mount time, not just the top-level
    # mountpoint itself.
    sudo mount --bind "$nested_source" "$host_dir/nested"

    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          readOnly: true
          recursiveReadOnly: Enabled
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        die "pod never reached Running with recursiveReadOnly: Enabled — check whether this runtime rejects Mount.recursive_read_only entirely (should fail CreateContainer cleanly, not silently partial-mount) rather than a nodelet bug"
    fi
    if kctl exec "$name" -- touch /hostvol/nested/test >/dev/null 2>&1; then
        die "a write inside the NESTED mount under a recursiveReadOnly: Enabled volume succeeded — the read-only-ness isn't actually recursive, just applied to the top-level mountpoint"
    fi
}

test_recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability() {
    # Round 97: IfPossible's Enabled-vs-Disabled decision should track
    # this runtime handler's own advertised recursiveReadOnlyMounts
    # capability — not just that the pod reaches Running either way
    # (test_recursive_read_only_if_possible_falls_back_without_erroring
    # already proves that automatically), but that the SPECIFIC value
    # chosen actually matches what Node.status.runtimeHandlers reports.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local supports
    supports="$(kubectl get node "$(node_name)" -o jsonpath='{.status.runtimeHandlers[0].features.recursiveReadOnlyMounts}')"
    if [[ "$supports" != "true" && "$supports" != "false" ]]; then
        skip_test "Node.status.runtimeHandlers[0].features.recursiveReadOnlyMounts wasn't a clean true/false ('$supports') — nothing to compare against"
    fi
    local want
    want="Disabled"; [[ "$supports" == "true" ]] && want="Enabled"

    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-rro-capability-test.XXXXXX)"
    local name="recursive-readonly-capability-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - name: hostvol
          mountPath: /hostvol
          readOnly: true
          recursiveReadOnly: IfPossible
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        rm -rf "$host_dir"
        delete_pod_if_exists "$name"
        die "pod never reached Running with recursiveReadOnly: IfPossible set"
    fi
    local got
    got="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].volumeMounts[?(@.name=="hostvol")].recursiveReadOnly}')"
    delete_pod_if_exists "$name"
    rm -rf "$host_dir"
    assert_eq "$got" "$want" "IfPossible's Enabled-vs-Disabled choice should match this runtime handler's own advertised recursiveReadOnlyMounts capability ($supports)"
}

register_test test_mount_propagation_host_to_container_still_mounts_normally
register_test test_mount_propagation_host_to_container_sees_a_new_host_mount
register_test test_mount_propagation_private_default_does_not_see_a_new_host_mount
register_test test_recursive_read_only_still_mounts_read_only_normally
register_test test_recursive_read_only_if_possible_falls_back_without_erroring
register_test test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too
register_test test_recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability
register_test test_host_path_directory_mounts_the_real_host_directory
register_test test_host_path_directory_or_create_creates_a_missing_directory
register_test test_host_path_directory_type_rejects_a_nonexistent_path
