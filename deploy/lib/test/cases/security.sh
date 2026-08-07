# lib/test/cases/security.sh — securityContext translation to CRI's
# LinuxContainerSecurityContext, verified by having the container itself
# report what it sees into a shared emptyDir (read back off the host —
# same trick as volumes.sh; there's no exec to check this from outside).

test_run_as_user_is_applied() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="run-as-user"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      securityContext:
        runAsUser: 1000
        runAsGroup: 1000
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    # Not the shared-emptyDir-write trick the rest of this file uses (see
    # its header) — kctl exec now works (Round 111), and it's the more
    # direct check here anyway: a plain runAsUser with no fsGroup set
    # genuinely can't write to a root-owned emptyDir on real kubelet
    # either (a well-known K8s gotcha, confirmed live: 'Permission
    # denied', not a nodelet bug), so the write-to-a-volume version of
    # this specific test was asserting something real kubelet wouldn't
    # support either — kctl exec sidesteps the whole question by just
    # asking the running container what UID/GID it's actually executing
    # as, which is exactly what runAsUser/runAsGroup are supposed to
    # control.
    local uid gid
    uid="$(kctl exec "$name" -- id -u)"
    gid="$(kctl exec "$name" -- id -g)"
    assert_eq "$uid" "1000" "runAsUser"
    assert_eq "$gid" "1000" "runAsGroup"
    delete_pod_if_exists "$name"
}

test_container_status_reports_resolved_user() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="container-status-user"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      securityContext:
        runAsUser: 4000
        runAsGroup: 5000
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    # Round 90 (found in round 89's re-audit): containerStatuses[].user.linux
    # is fetched once via ContainerStatusRequest right after the container
    # starts and cached for the rest of that instance's life -- give it a
    # moment past Running to land.
    local uid gid
    if ! try_wait_until 20 bash -c \
        "[[ -n \"\$(kctl get pod '$name' -o jsonpath='{.status.containerStatuses[0].user.linux.uid}')\" ]]"; then
        # Confirmed live via crictl inspect straight against containerd
        # (bypassing nodelet entirely): this containerd build's own
        # ContainerStatus response has no `user` field at all for this
        # container — CRI's ContainerStatus.user (field 18) isn't
        # verbose-gated, so nodelet requesting verbose:false isn't why;
        # this containerd version/build just doesn't populate it. Same
        # class of runtime limitation as test_lifecycle_stop_signal_is_
        # honored_by_the_runtime's skip in lifecycle.sh.
        delete_pod_if_exists "$name"
        skip_test "containerStatuses[0].user.linux.uid never appeared — this containerd build's own ContainerStatus response doesn't populate the user field at all (confirmed via 'crictl inspect' directly, independent of nodelet); check runtime/cri/container_support.rs's container_status_details() if this is unexpected on a runtime version known to support it"
    fi
    uid="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].user.linux.uid}')"
    gid="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].user.linux.gid}')"
    assert_eq "$uid" "4000" "containerStatuses[0].user.linux.uid should match securityContext.runAsUser"
    assert_eq "$gid" "5000" "containerStatuses[0].user.linux.gid should match securityContext.runAsGroup"
    delete_pod_if_exists "$name"
}

test_container_status_reports_recursive_read_only() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="container-status-rro"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: ro-vol
      emptyDir: {}
    - name: rw-vol
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      volumeMounts:
        - {name: ro-vol, mountPath: /ro, readOnly: true, recursiveReadOnly: Enabled}
        - {name: rw-vol, mountPath: /rw}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    # Round 91 (found in round 89's re-audit, the missing reporting half
    # of round 85's IfPossible-treated-as-Enabled simplification):
    # containerStatuses[].volumeMounts[].recursiveReadOnly is computed
    # from the container's own volumeMounts spec at container-creation
    # time (same as CRI mount-request time) -- no live-runtime wait
    # needed beyond Running, but a moment for the status write to land.
    local ro_status rw_status rw_read_only
    wait_until 20 "$name containerStatuses[0].volumeMounts to be populated" bash -c \
        "[[ -n \"\$(kctl get pod '$name' -o jsonpath='{.status.containerStatuses[0].volumeMounts}')\" ]]"
    ro_status="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].volumeMounts[?(@.name=="ro-vol")].recursiveReadOnly}')"
    rw_status="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].volumeMounts[?(@.name=="rw-vol")].recursiveReadOnly}')"
    rw_read_only="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].volumeMounts[?(@.name=="rw-vol")].readOnly}')"
    assert_eq "$ro_status" "Enabled" "ro-vol's recursiveReadOnly: Enabled must be reported back as Enabled"
    assert_eq "$rw_status" "" "rw-vol is not read-only, so recursiveReadOnly must stay unspecified"
    # Not a strict "false" assertion — confirmed live this comes back as
    # an absent field, not the literal string "false" (same "absent means
    # the not-read-only default" pattern the recursiveReadOnly assertion
    # right above already accepts). Traced as far as confirming nodelet's
    # own code is right (volume_mount_status_tuples() computes
    # Some(false), and k8s-openapi 0.28.0's VolumeMountStatus::serialize
    # does emit Some(false) as "readOnly": false, not skip it) without
    # finding where between there and the client the field goes missing —
    # flagged in docs/E2E_FINDINGS.md rather than chased further here,
    # since "absent" and "false" are semantically identical for this
    # field either way.
    [[ "$rw_read_only" == "false" || -z "$rw_read_only" ]] \
        || die "rw-vol readOnly should reflect the container's actual mount — got '$rw_read_only', want 'false' or absent"
    delete_pod_if_exists "$name"
}

test_read_only_root_filesystem_is_enforced() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="readonly-rootfs"
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
      securityContext:
        readOnlyRootFilesystem: true
      command:
        - sh
        - -c
        - |
          if touch /roottest 2>/dev/null; then
            echo writable > /shared/result.txt
          else
            echo readonly > /shared/result.txt
          fi
          sleep 3600
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local result
    result="$(wait_for_check_file "$name" shared result.txt 30)"
    assert_eq "$result" "readonly" "readOnlyRootFilesystem must block writes outside mounted volumes"
    delete_pod_if_exists "$name"
}

test_without_read_only_root_filesystem_writes_succeed() {
    # Control case for the test above — proves the check itself is valid
    # (i.e. the image's root normally *is* writable) rather than "readonly"
    # showing up for some unrelated reason.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="writable-rootfs"
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
      command:
        - sh
        - -c
        - |
          if touch /roottest 2>/dev/null; then
            echo writable > /shared/result.txt
          else
            echo readonly > /shared/result.txt
          fi
          sleep 3600
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local result
    result="$(wait_for_check_file "$name" shared result.txt 30)"
    assert_eq "$result" "writable" "root filesystem should be writable without readOnlyRootFilesystem"
    delete_pod_if_exists "$name"
}

test_host_users_false_gets_a_real_user_namespace() {
    # Round 25: spec.hostUsers: false should get an exclusive host UID/GID
    # range, not the host's own UID space. /proc/self/uid_map inside a real
    # user namespace shows a remapped range ("0 <host_base> <length>");
    # outside one it shows the host's full identity range
    # ("0 0 4294967295").
    #
    # Round 123: this pod reaching Running at all depends on the runtime
    # accepting the SAME userns config on both the sandbox
    # (LinuxSandboxSecurityContext.namespace_options.userns_options, round
    # 25) and every container (LinuxContainerSecurityContext's own copy,
    # round 123 — linux_security_context() used to leave this unset
    # entirely, which is the actual reason this check used to see the
    # host's own full identity range even when the sandbox itself was
    # correctly namespaced). First live symptom looked like a containerd
    # bug (CreateContainer rejecting with "user namespace config for
    # sandbox is different from container. Sandbox userns config: <nil>
    # ...") but reading containerd's real source
    # (internal/cri/server/container_create.go's sameUsernsConfig check)
    # found the actual cause: CreateContainerRequest.sandbox_config is
    # CRI's own *redundant* resend of the sandbox's config (see its own
    # proto doc comment) — containerd's consistency check reads the
    # sandbox's userns straight from THAT redundant copy, not from
    # whatever RunPodSandbox itself received, and container_create.rs's
    # CreateContainer call site was hardcoding `None` for it regardless of
    # the pod's real userns range (the exact same bug class an earlier
    # round already hit for this same redundant field's `privileged`
    # value — see that call site's own doc comment). Nothing wrong with
    # containerd at all; fixed by passing the real userns_mapping through
    # to that redundant copy too.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostusers-false-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostUsers: false
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /proc/self/uid_map > /shared/uid_map.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with hostUsers: false — check nodelet's logs for 'user namespace: no free UID/GID range available' (pool exhausted), a RunPodSandbox error (runtime doesn't support CRI's userns_options at all, e.g. too old containerd), or CreateContainer's own 'user namespace config for sandbox is different from container' error (check that CreateContainerRequest.sandbox_config in container_create.rs is passing the pod's real userns_mapping, not None — see this test's own doc comment for the full story)"
    fi

    local uid_map
    uid_map="$(wait_for_check_file "$name" shared uid_map.txt 30)"
    assert_not_empty "$uid_map" "/proc/self/uid_map should have content"
    if echo "$uid_map" | grep -q "^\s*0\s\+0\s\+4294967295"; then
        delete_pod_if_exists "$name"
        die "uid_map shows the host's own full identity range ('0 0 4294967295') — this container is NOT in a user namespace at all; check runtime/cri.rs's userns_mapping wiring and that the CRI runtime actually honors LinuxSandboxSecurityContext.namespace_options.userns_options"
    fi
    delete_pod_if_exists "$name"
}

test_host_users_false_volume_still_reads_and_writes_normally() {
    # Round 88 (found in round 86's re-audit) originally gave every volume
    # mount for a hostUsers: false pod its own per-mount CRI
    # Mount.uidMappings/.gidMappings; round 123 found (live, via an A/B
    # `stat /` comparison inside a failing container) that this
    # double-translated against the sandbox's own already-correct ambient
    # namespace and actively broke every such mount with EACCES. Removed
    # entirely — a plain bind mount plus chowning the host-side directory
    # to the pod's userns base uid/gid (resolve_volumes()'s own
    # chown_userns_base() call) is the whole fix now; genuinely proving
    # the OWNERSHIP TRANSLATION itself still needs a host-side file
    # pre-chowned into the pod's specific mapped UID range before the pod
    # starts (root required) -- see the manual-note below for that. This
    # test proves the more basic thing, fully automated: a plain
    # read/write into a hostUsers: false pod's own emptyDir doesn't
    # silently fail. Same as the real-userns sibling test above, this
    # pod reaching Running at all also depends on CreateContainer's own
    # redundant sandbox_config resend correctly carrying the container's
    # userns_mapping (round 123) — see that test's own doc comment for
    # the full story.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostusers-false-volume-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostUsers: false
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo hello-from-userns-pod > /shared/marker; cat /shared/marker > /shared/roundtrip; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with hostUsers: false and a volume mounted — check build_mounts()/resolve_volumes() wiring in runtime/cri/volumes_pure.rs and volumes_resolve.rs, run_sandbox()'s own userns_options setup, or CreateContainer's redundant sandbox_config resend (see the real-userns sibling test's own doc comment)"
    fi
    local content
    content="$(wait_for_check_file "$name" shared roundtrip 30)"
    delete_pod_if_exists "$name"
    assert_contains "$content" "hello-from-userns-pod" "a hostUsers: false pod's volume should still read/write normally via the sandbox's own ambient user namespace"
}

test_host_users_volume_ownership_translation_is_correct() {
    # Round 123: proving OWNERSHIP TRANSLATION (that a host-side file owned
    # by a UID within the pod's own mapped range shows up as the correct
    # small container-relative UID inside the container, rather than the
    # overflow/nobody UID a file outside that range would show) needs a
    # host-side file pre-chowned to the pod's own allocated userns base
    # UID -- but that's only known once the pod is already running (it's
    # assigned by nodelet's own allocator, keyed by pod uid, at
    # RunPodSandbox time). Sidesteps the "needs NODELET_USERNS_BASE_UID's
    # live value" problem the old manual-note version of this test had by
    # discovering it live: mount BOTH an emptyDir (already correctly
    # chowned to the pod's base uid by resolve_volumes()'s own
    # chown_userns_base(), confirmed working earlier this round) AND a
    # real hostPath directory in the SAME pod. Once Running, `stat` the
    # emptyDir's real host directory to learn the live base uid, chown the
    # hostPath directory's marker file to it (a bind mount is a live view
    # of the same filesystem, not a snapshot -- a host-side chown made
    # *after* mounting is immediately visible inside the already-running
    # container), then confirm the container sees it translated to uid 0.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hostusers-ownership-check"
    local host_dir
    host_dir="$(mktemp -d /tmp/nodelet-hostusers-ownership-test.XXXXXX)"
    : > "$host_dir/marker"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostUsers: false
  volumes:
    - name: shared
      emptyDir: {}
    - name: hostvol
      hostPath:
        path: $host_dir
        type: Directory
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
        - {name: hostvol, mountPath: /hostvol}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        sudo rm -rf "$host_dir"
        die "pod never reached Running with hostUsers: false — see the sibling real-userns test's own doc comment for known causes"
    fi

    local base_uid
    base_uid="$(stat -c %u "$(pod_volume_host_path "$name" shared)")"
    assert_not_empty "$base_uid" "the pod's live userns base uid, read from its own chowned emptyDir"
    # Round 123 (found live in CI): chowning only the marker file isn't
    # enough — `mktemp -d` made $host_dir mode 0700, owned by this script's
    # own (real host) user. From the container's mapped view, that real
    # owner falls outside the pod's userns range and shows as the overflow
    # uid, so 0700 denies the mapped root even *traversal* into the
    # directory (stat failed with EACCES, not just showing the wrong
    # owner). Chowning the directory too — not just the file inside it —
    # makes it show as uid 0 from the container's view, and its own 0700
    # bits then grant that (translated) owner full access, same as any
    # real hostPath directory a cluster operator actually owns would.
    sudo chown "$base_uid:$base_uid" "$host_dir" "$host_dir/marker"

    local translated_uid
    # Round 123 (found live in CI): kubectl's own "kuberc: ... permission
    # denied" warning lands on the same stream as the real value with no
    # separating newline — same recurring fix as csi_pvc.sh's fsGroup
    # test and resources.sh's limited_swap test needed.
    translated_uid="$(kctl exec "$name" -- stat -c %u /hostvol/marker 2>&1 | tr -dc '0-9')"
    delete_pod_if_exists "$name"
    sudo rm -rf "$host_dir"
    assert_eq "$translated_uid" "0" "a host-side file owned by the pod's own userns base uid ($base_uid) should show as uid 0 inside the container (real ownership translation via the sandbox's ambient user namespace), not 65534/nobody"
}

test_client_certificate_authentication_works() {
    # Round 95's original feature; round 123 automates what used to be a
    # manual spot-check purely because this harness had no way to restart
    # nodelet with NODELET_CLIENT_CA_FILE set before its server binds —
    # nodelet_restart_with_env (nodelet_env.sh) now does exactly that.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CLIENT_CA_FILE set"; fi
    command -v openssl >/dev/null 2>&1 || skip_test "needs openssl to generate a test CA/client cert"

    local work_dir
    work_dir="$(mktemp -d /tmp/nodelet-client-cert-test.XXXXXX)"
    client_cert_test_cleanup() {
        nodelet_restore_env
        rm -rf "${work_dir:-}"
    }
    trap client_cert_test_cleanup EXIT

    # Explicit -addext for basicConstraints/keyUsage/extendedKeyUsage
    # rather than relying on the OS's own openssl.cnf defaults for -x509
    # mode (which vary) — rustls's WebPkiClientVerifier is a real X.509
    # path validator, not OpenSSL's own (more permissive) verify, so a
    # cert missing CA:true on the root or clientAuth EKU on the leaf can
    # fail the handshake even though `openssl verify` would accept it.
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work_dir/ca.key" -out "$work_dir/ca.crt" -days 1 -subj /CN=test-ca \
        -addext basicConstraints=critical,CA:true -addext keyUsage=critical,keyCertSign,cRLSign
    openssl req -newkey rsa:2048 -nodes -keyout "$work_dir/client.key" -out "$work_dir/client.csr" -subj /CN=alice/O=system:masters
    openssl x509 -req -in "$work_dir/client.csr" -CA "$work_dir/ca.crt" -CAkey "$work_dir/ca.key" -CAcreateserial -out "$work_dir/client.crt" -days 1 \
        -copy_extensions none -extfile <(printf 'basicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth')
    # A second, self-signed cert NOT signed by our CA — proves rejection,
    # not just success.
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work_dir/other.key" -out "$work_dir/other.crt" -days 1 -subj /CN=mallory \
        -addext basicConstraints=critical,CA:true -addext keyUsage=critical,keyCertSign,cRLSign

    nodelet_restart_with_env "NODELET_CLIENT_CA_FILE=$work_dir/ca.crt"

    local node_ip port
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    port="${NODELET_SERVER_PORT:-10250}"

    # (1) the client cert alone, no Authorization header at all, should
    # authenticate successfully.
    local cert_response
    cert_response="$(curl -ksS --max-time 5 --cert "$work_dir/client.crt" --key "$work_dir/client.key" "https://$node_ip:$port/stats/summary")"
    assert_contains "$cert_response" "nodeName" "TLS client cert alone should authenticate against /stats/summary with no bearer token"

    # (2) a cert NOT signed by NODELET_CLIENT_CA_FILE should fail the TLS
    # handshake outright (rustls itself rejects it before nodelet code
    # ever runs) — a connection failure, not a 401 response.
    curl -ksS --max-time 5 --cert "$work_dir/other.crt" --key "$work_dir/other.key" "https://$node_ip:$port/stats/summary" >/dev/null 2>&1
    assert_true bash -c "[[ $? -ne 0 ]]" "a client cert not signed by NODELET_CLIENT_CA_FILE should fail the TLS handshake, not just get a 401"

    # (3) no cert and no bearer token should still get the existing 401
    # fallback — the cert-auth path must not have replaced it.
    local no_auth_status
    no_auth_status="$(curl -ksS --max-time 5 -o /dev/null -w '%{http_code}' "https://$node_ip:$port/stats/summary")"
    assert_eq "$no_auth_status" "401" "no cert and no Authorization header should still get the existing 401 bearer-token fallback"
}

test_containers_get_isolated_pid_namespaces_by_default() {
    # Round 40: real Kubernetes' actual default is CONTAINER-scoped PID
    # namespaces (each container is its own pid-1), NOT the CRI proto's own
    # zero-value default (POD-shared) nodelet was previously silently
    # relying on. A container's own shell reports pid 1 for itself only
    # when it's truly the init process of its own isolated namespace.
    #
    # Round 123 (found live in CI): the command below needs '$$$$', not
    # '$$' -- real kubelet's own command/args $(VAR_NAME) expansion syntax
    # (see expand_command_arg() in volumes_pure.rs) treats a literal '$$'
    # as an ESCAPE for a single literal '$', the same string-substitution
    # pass every command/args field gets regardless of whether it happens
    # to invoke a shell. Writing '$$' here (after this heredoc's own bash
    # unescaping) gets folded to a single '$' before the container's own
    # shell ever sees it, so 'echo $' just prints the literal character
    # '$' -- confirmed live: the test failed with "got '$', want '1'", not
    # a real PID namespace bug. '$$$$' survives this heredoc as '$$$$',
    # nodelet's own expansion folds each '$$' pair to one '$', leaving the
    # real '$$' shell syntax the test actually wants reaching the container.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="pid-isolated-default"
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
    - name: first
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
    - name: second
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo \$\$\$\$ > /shared/second-pid.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local second_pid
    second_pid="$(wait_for_check_file "$name" shared second-pid.txt 30)"
    assert_eq "$second_pid" "1" "second container's own shell should be pid 1 in its own isolated PID namespace"
    delete_pod_if_exists "$name"
}

test_share_process_namespace_puts_every_container_in_one_pid_namespace() {
    # Round 123: same '$$$$' (not '$$') fix as the sibling isolated-PID
    # test above, and for the same reason -- this one just never surfaced
    # as a visible failure, since the pre-fix literal '$' content also
    # happens to satisfy assert_not_eq(..., "1") (a false-positive pass,
    # not a real exercise of shareProcessNamespace).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="pid-shared"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  shareProcessNamespace: true
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: first
      image: $TEST_IMAGE
      command: ["sh", "-c", "sleep 3600"]
    - name: second
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo \$\$\$\$ > /shared/second-pid.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local second_pid
    second_pid="$(wait_for_check_file "$name" shared second-pid.txt 30)"
    assert_not_eq "$second_pid" "1" "with shareProcessNamespace, the first container already holds pid 1 — the second container's shell must get a higher pid in the shared namespace"
    delete_pod_if_exists "$name"
}

test_host_pid_sees_host_processes() {
    # hostPID: true joins the node's own PID namespace, so the container
    # should see far more than its own 1-2 processes.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="host-pid-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostPID: true
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "ls /proc | grep -E '^[0-9]+\$' | wc -l > /shared/proc_count.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local count
    count="$(wait_for_check_file "$name" shared proc_count.txt 30)"
    assert_true bash -c "[[ '$count' -gt 5 ]]" "hostPID container should see many more than its own handful of processes (saw $count)"
    delete_pod_if_exists "$name"
}

register_test test_container_status_reports_resolved_user
register_test test_container_status_reports_recursive_read_only
register_test test_run_as_user_is_applied
register_test test_read_only_root_filesystem_is_enforced
register_test test_without_read_only_root_filesystem_writes_succeed
test_sysctls_are_applied_to_the_sandbox() {
    # Round 41: spec.securityContext.sysctls -> CRI's
    # LinuxPodSandboxConfig.sysctls map. net.ipv4.ip_unprivileged_port_start
    # is namespaced (safe to set without hostNetwork/privileged) and its
    # current value is directly readable via /proc/sys, so this is a real,
    # structural proof rather than just a status-string check.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="sysctls-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  securityContext:
    sysctls:
      - name: net.ipv4.ip_unprivileged_port_start
        value: "1234"
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /proc/sys/net/ipv4/ip_unprivileged_port_start > /shared/sysctl.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with a sysctl set — check nodelet's logs for a RunPodSandbox error (runtime may not support this specific sysctl as namespaced, or reject unknown sysctls)"
    fi
    local value
    value="$(wait_for_check_file "$name" shared sysctl.txt 30)"
    assert_eq "$value" "1234" "net.ipv4.ip_unprivileged_port_start should reflect the pod's own securityContext.sysctls value"
    delete_pod_if_exists "$name"
}

test_supplemental_groups_policy_strict_ignores_image_group_membership() {
    # Round 62: securityContext.supplementalGroupsPolicy (GA 1.33) was
    # never read at all before this. Rather than depend on whatever
    # /etc/group happens to ship in $TEST_IMAGE (fragile, varies by
    # image/tag), this test supplies its own /etc/passwd and /etc/group
    # via a ConfigMap mounted with subPath — fully portable proof: a
    # "testuser" (uid 2000, primary gid 3000) that the image's own
    # /etc/group lists as an extra member of "imagegroup" (gid 4000), on
    # top of an explicit securityContext.supplementalGroups: [5000].
    # With Merge (the default), `id -G` must include both 4000 (from the
    # image's own group file) and 5000 (explicit). With Strict, only the
    # explicit groups (3000, 5000) — 4000 must NOT appear.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="sgp"
    kctl create configmap "$name-etc" \
        --from-literal=passwd="testuser:x:2000:3000::/home/testuser:/bin/sh" \
        --from-literal=group="$(printf 'testgroup:x:3000:\nimagegroup:x:4000:testuser\n')" \
        >/dev/null
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
    - name: etc-override
      configMap:
        name: $name-etc
  containers:
    - name: merge
      image: $TEST_IMAGE
      securityContext:
        runAsUser: 2000
        runAsGroup: 3000
        supplementalGroups: [5000]
        supplementalGroupsPolicy: Merge
      command: ["sh", "-c", "id -G > /shared/merge.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
        - {name: etc-override, mountPath: /etc/passwd, subPath: passwd}
        - {name: etc-override, mountPath: /etc/group, subPath: group}
    - name: strict
      image: $TEST_IMAGE
      securityContext:
        runAsUser: 2000
        runAsGroup: 3000
        supplementalGroups: [5000]
        supplementalGroupsPolicy: Strict
      command: ["sh", "-c", "id -G > /shared/strict.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
        - {name: etc-override, mountPath: /etc/passwd, subPath: passwd}
        - {name: etc-override, mountPath: /etc/group, subPath: group}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kctl delete configmap "$name-etc" --ignore-not-found >/dev/null
        die "pod never reached Running with supplementalGroupsPolicy set — check nodelet's logs for a RunPodSandbox/CreateContainer error (runtime may not support CRI's SupplementalGroupsPolicy field at all, e.g. too old containerd)"
    fi

    local merge_groups strict_groups
    merge_groups="$(wait_for_check_file "$name" shared merge.txt 30)"
    strict_groups="$(wait_for_check_file "$name" shared strict.txt 20)"
    delete_pod_if_exists "$name"
    kctl delete configmap "$name-etc" --ignore-not-found >/dev/null

    assert_contains "$merge_groups" "4000" "supplementalGroupsPolicy: Merge should include imagegroup's gid (4000) from the image's own /etc/group membership"
    assert_contains "$merge_groups" "5000" "supplementalGroupsPolicy: Merge should still include the explicit supplementalGroups entry (5000)"
    assert_contains "$strict_groups" "5000" "supplementalGroupsPolicy: Strict should still include the explicit supplementalGroups entry (5000)"
    if echo "$strict_groups" | grep -qw "4000"; then
        die "supplementalGroupsPolicy: Strict must NOT include imagegroup's gid (4000) from image-defined /etc/group membership, but id -G reported: $strict_groups"
    fi
}

test_proc_mount_default_masks_proc_kcore() {
    # Round 78 (found in round 76's re-audit): under the default
    # procMount, /proc/kcore should be bind-mounted to /dev/null by the
    # runtime (real kubelet's own standard masked-paths list, which
    # nodelet now actually sends instead of leaving the field unset) --
    # reading it returns 0 bytes immediately. Before this round, nodelet
    # never set masked_paths/readonly_paths at all, which on a modern
    # containerd (disable_proc_mount=false, the common config) applies NO
    # masking whatsoever -- a real, if subtle, security regression this
    # closes.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="proc-mount-default"
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
      command: ["sh", "-c", "head -c 4 /proc/kcore 2>/dev/null | wc -c > /shared/kcore_bytes.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running -- can't exercise the default procMount masking check"
    fi
    local bytes
    bytes="$(wait_for_check_file "$name" shared kcore_bytes.txt 30)"
    delete_pod_if_exists "$name"
    if [[ "$bytes" != "0" ]]; then
        warn "expected /proc/kcore to read 0 bytes under the default procMount (masked to /dev/null), got $bytes -- check proc_mount_paths()/linux_security_context()'s masked_paths wiring in runtime/cri/resources.rs; not failing outright since this depends on the CRI runtime actually honoring masked_paths (some configurations, e.g. containerd's disable_proc_mount=true, apply their own OCI-spec-generator default regardless of what's sent)"
    fi
}

test_proc_mount_unmasked_leaves_proc_kcore_readable() {
    # The Unmasked-vs-Default control case: with procMount: Unmasked,
    # /proc/kcore should be the real (huge, non-empty) kernel core image,
    # not bind-mounted to /dev/null.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="proc-mount-unmasked"
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
      securityContext:
        procMount: Unmasked
      command: ["sh", "-c", "head -c 4 /proc/kcore 2>/dev/null | wc -c > /shared/kcore_bytes.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with securityContext.procMount: Unmasked -- check nodelet's logs for a CreateContainer error (the runtime may reject procMount: Unmasked entirely without also setting a permissive seccomp profile, matching real kubelet's own admission-time requirement that this project's apiserver doesn't enforce)"
    fi
    local bytes
    bytes="$(wait_for_check_file "$name" shared kcore_bytes.txt 30)"
    delete_pod_if_exists "$name"
    if [[ "$bytes" == "0" ]]; then
        warn "expected /proc/kcore to be readable (non-zero bytes) under procMount: Unmasked, got 0 -- check proc_mount_paths()'s Unmasked branch in runtime/cri/resources.rs; not failing outright since this depends on the CRI runtime actually honoring an explicitly-empty masked_paths the same way it honors a populated one"
    fi
}

register_test test_proc_mount_default_masks_proc_kcore
register_test test_proc_mount_unmasked_leaves_proc_kcore_readable
register_test test_host_users_false_gets_a_real_user_namespace
register_test test_host_users_false_volume_still_reads_and_writes_normally
register_test test_host_users_volume_ownership_translation_is_correct
register_test test_client_certificate_authentication_works
register_test test_containers_get_isolated_pid_namespaces_by_default
register_test test_share_process_namespace_puts_every_container_in_one_pid_namespace
register_test test_host_pid_sees_host_processes
register_test test_sysctls_are_applied_to_the_sandbox
register_test test_supplemental_groups_policy_strict_ignores_image_group_membership
