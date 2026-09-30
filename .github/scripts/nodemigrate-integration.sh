#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}"
BIN_DIR="$ROOT/target/release"
NK="$BIN_DIR/notk8s"
MIGRATE="$BIN_DIR/nodemigrate"
LIBRARY_MODE="${NODEMIGRATE_INTEGRATION_LIBRARY:-false}"
if [[ "$LIBRARY_MODE" == true ]]; then
    SOURCE_DIST="${NODEMIGRATE_SOURCE_DIST:-kubernetes}"
else
    SOURCE_DIST="${NODEMIGRATE_SOURCE_DIST:?set NODEMIGRATE_SOURCE_DIST to k3s or kubernetes}"
fi
LOG="${NODEMIGRATE_TEST_LOG:-/tmp/nodemigrate-integration.log}"
WORK="${NODEMIGRATE_WORK_DIR:-/var/lib/nodemigrate-ci}"
STATIC_PATH="$WORK/static-volume"
CHECKPOINT_DIR="$WORK/checkpoints"
SOURCE_KUBECONFIG=""
CURRENT_KUBECONFIG=""
MIGRATION_STARTED_AT=""
TARGET_WATCH_PID=""
TARGET_WATCH_STOP_FILE=""
TARGET_WATCH_LOG=""

if [[ "$LIBRARY_MODE" != true ]]; then
    exec > >(tee -a "$LOG") 2>&1
fi

capture_cilium_envoy_process_owners() {
    local envoy_pid current_pid parent_pid depth
    while IFS= read -r envoy_pid; do
        [[ "$envoy_pid" =~ ^[0-9]+$ ]] || continue
        echo "Cilium Envoy process ownership chain for PID $envoy_pid:"
        current_pid="$envoy_pid"
        depth=0
        while [[ "$current_pid" =~ ^[0-9]+$ && "$current_pid" -gt 1 && "$depth" -lt 8 ]]; do
            [[ -r "/proc/$current_pid/status" ]] || break
            printf 'PID %s cmdline: ' "$current_pid"
            tr '\0' ' ' < "/proc/$current_pid/cmdline" || true
            printf '\nPID %s cgroup:\n' "$current_pid"
            cat "/proc/$current_pid/cgroup" || true
            parent_pid="$(awk '$1 == "PPid:" { print $2 }' "/proc/$current_pid/status")"
            [[ "$parent_pid" =~ ^[0-9]+$ && "$parent_pid" != "$current_pid" ]] || break
            current_pid="$parent_pid"
            depth=$((depth + 1))
        done
    done < <(ps -eo pid=,comm= | awk '$2 == "cilium-envoy" || $2 == "cilium-envoy-st" { print $1 }')
}

capture_cilium_agent_logs() {
    local pod kubeconfig="${KUBECONFIG:-${CURRENT_KUBECONFIG:-$SOURCE_KUBECONFIG}}"
    while IFS= read -r pod; do
        [[ -n "$pod" ]] || continue
        echo "Cilium agent logs for $pod (current):"
        KUBECONFIG="$kubeconfig" kubectl logs -n kube-system "$pod" \
            -c cilium-agent --tail=300 || true
        echo "Cilium agent logs for $pod (previous):"
        KUBECONFIG="$kubeconfig" kubectl logs -n kube-system "$pod" \
            -c cilium-agent --previous --tail=300 || true
        echo "Cilium config init logs for $pod (current and previous):"
        KUBECONFIG="$kubeconfig" kubectl logs -n kube-system "$pod" \
            -c config --tail=100 || true
        KUBECONFIG="$kubeconfig" kubectl logs -n kube-system "$pod" \
            -c config --previous --tail=100 || true
    done < <(KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
        -l k8s-app=cilium -o name 2>/dev/null || true)
}

# Record only the API-requested security settings and selected OCI runtime
# fields for Cilium's agent. Do not print the full CRI inspect response: it can
# contain environment values and other credentials unrelated to this probe.
capture_cilium_agent_cri_security() {
    local kubeconfig="${1:-${KUBECONFIG:-${CURRENT_KUBECONFIG:-$SOURCE_KUBECONFIG}}}"
    local container_json="" pod_records pod_record pod_uid pod_name agent_ids container_id
    local runtime_endpoint candidate_json
    local -a runtime_endpoints=()
    if [[ -n "${NODEMIGRATE_CRI_ENDPOINT:-}" ]]; then
        runtime_endpoints=("$NODEMIGRATE_CRI_ENDPOINT")
    else
        runtime_endpoints=(
            unix:///run/containerd/containerd.sock
            unix:///run/k3s/containerd/containerd.sock
        )
    fi
    pod_records="$(KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
        -l k8s-app=cilium -o json 2>/dev/null | jq -c '
          [.items[]? | {
            uid: .metadata.uid,
            name: .metadata.name,
            hostNetwork: (.spec.hostNetwork // false),
            agent: ([.spec.containers[]? | select(.name == "cilium-agent") | {
              securityContext: {
                privileged: (.securityContext.privileged // false),
                allowPrivilegeEscalation: .securityContext.allowPrivilegeEscalation,
                procMount: (.securityContext.procMount // "Default"),
                readOnlyRootFilesystem: (.securityContext.readOnlyRootFilesystem // false),
                capabilities: .securityContext.capabilities
              }
            }][0] // null)
          }]')" || return 0
    for runtime_endpoint in "${runtime_endpoints[@]}"; do
        candidate_json="$(crictl --runtime-endpoint "$runtime_endpoint" \
            ps -a -o json 2>/dev/null)" || continue
        if jq -e --argjson pods "$pod_records" '
          . as $cri
          | any($pods[]?;
              .uid as $uid
              | any($cri.containers[]?;
                  (.labels["nodelet.dev/pod-uid"] // .labels["io.kubernetes.pod.uid"]) == $uid
                  and (.labels["nodelet.dev/container-name"] // .labels["io.kubernetes.container.name"]) == "cilium-agent"))
        ' <<< "$candidate_json" >/dev/null; then
            container_json="$candidate_json"
            break
        fi
    done
    if [[ -z "$container_json" ]]; then
        echo "Unable to find Cilium agent containers through CRI endpoints: ${runtime_endpoints[*]}"
        return 0
    fi
    while IFS= read -r pod_record; do
        [[ -n "$pod_record" ]] || continue
        pod_uid="$(jq -r '.uid' <<< "$pod_record")"
        pod_name="$(jq -r '.name' <<< "$pod_record")"
        agent_ids="$(jq -r --arg uid "$pod_uid" '
            .containers[]?
            | select((.labels["nodelet.dev/pod-uid"] // .labels["io.kubernetes.pod.uid"]) == $uid)
            | select((.labels["nodelet.dev/container-name"] // .labels["io.kubernetes.container.name"]) == "cilium-agent")
            | .id // empty
        ' <<< "$container_json")"
        if [[ -z "$agent_ids" ]]; then
            printf 'Cilium agent CRI security pod=%s api=%s runtime=container-not-found endpoints=%s\n' \
                "$pod_name" "$(jq -cS . <<< "$pod_record")" "${runtime_endpoints[*]}"
            continue
        fi
        while IFS= read -r container_id; do
            [[ -n "$container_id" ]] || continue
            printf 'Cilium agent CRI security pod=%s api=%s endpoint=%s runtime=' \
                "$pod_name" "$(jq -cS . <<< "$pod_record")" "$runtime_endpoint"
            crictl --runtime-endpoint "$runtime_endpoint" \
                inspect "$container_id" 2>/dev/null | jq -cS '
                  .info.runtimeSpec as $spec
                  | {
                      id: .status.id,
                      readonlyRootfs: ($spec.root.readonly // false),
                      maskedPaths: ($spec.linux.maskedPaths // []),
                      readonlyPaths: ($spec.linux.readonlyPaths // []),
                      capabilities: ($spec.process.capabilities // {}),
                      noNewPrivileges: ($spec.process.noNewPrivileges // false),
                      procMounts: [
                        $spec.mounts[]?
                        | select((.destination // "") == "/proc" or
                                 ((.destination // "") | startswith("/proc/sys")) or
                                 (.destination // "") == "/host/proc/sys/net")
                        | {destination, source, options}
                      ]
                    }
                ' || echo '{"error":"CRI inspect did not expose the expected runtime fields"}'
        done <<< "$agent_ids"
    done < <(jq -c '.[]' <<< "$pod_records")
}

capture_cilium_init_container_diagnostics() {
    local kubeconfig="${KUBECONFIG:-${CURRENT_KUBECONFIG:-$SOURCE_KUBECONFIG}}"
    local container_json pod_records pod_uid pod_name init_containers container_id container_name task_row task_pid proc_file
    container_json="$(crictl --runtime-endpoint unix:///run/containerd/containerd.sock ps -a -o json 2>/dev/null)" || {
        echo "Unable to list CRI containers for Cilium init diagnostics" >&2
        return 0
    }
    pod_records="$(KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
        -l k8s-app=cilium -o json 2>/dev/null \
        | jq -r '.items[]? | [.metadata.uid, .metadata.name] | @tsv')" || return 0
    while IFS=$'\t' read -r pod_uid pod_name; do
        [[ -n "$pod_uid" && -n "$pod_name" ]] || continue
        init_containers="$(jq -r --arg uid "$pod_uid" \
            '.containers[]? | select(.labels["nodelet.dev/pod-uid"] == $uid and .labels["nodelet.dev/init"] == "true") | [.id, .labels["nodelet.dev/container-name"] // "unknown"] | @tsv' \
            <<< "$container_json")"
        while IFS=$'\t' read -r container_id container_name; do
            [[ -n "$container_id" ]] || continue
            echo "Cilium init container diagnostics pod=$pod_name name=$container_name id=$container_id" >&2
            crictl --runtime-endpoint unix:///run/containerd/containerd.sock \
                inspect "$container_id" >&2 || true
            echo "Cilium init container logs pod=$pod_name name=$container_name id=$container_id" >&2
            crictl --runtime-endpoint unix:///run/containerd/containerd.sock \
                logs --tail=500 "$container_id" >&2 || true
            task_row="$(ctr -n k8s.io tasks ls 2>/dev/null | awk -v id="$container_id" '$1 == id { print; found = 1 } END { if (!found) print "no live containerd task" }')"
            echo "Cilium init task pod=$pod_name name=$container_name id=$container_id: $task_row" >&2
            task_pid="$(awk '$2 ~ /^[0-9]+$/ { print $2; exit }' <<< "$task_row")"
            if [[ "$task_pid" =~ ^[0-9]+$ && "$task_pid" -gt 1 ]]; then
                for proc_file in status cgroup wchan; do
                    echo "Cilium init process /proc/$task_pid/$proc_file:" >&2
                    cat "/proc/$task_pid/$proc_file" >&2 || true
                done
                printf 'Cilium init process cmdline: ' >&2
                tr '\0' ' ' < "/proc/$task_pid/cmdline" >&2 || true
                printf '\n' >&2
            fi
        done <<< "$init_containers"
    done <<< "$pod_records"
}

probe_webhook_route() {
    local kubeconfig="${1:?missing probe kubeconfig}"
    local service_json cluster_ip endpoint_json address port status
    service_json="$(KUBECONFIG="$kubeconfig" kubectl get service cert-manager-webhook \
        -n cert-manager -o json 2>/dev/null)" || return 0
    cluster_ip="$(jq -r '.spec.clusterIP // empty' <<< "$service_json")"
    if [[ -n "$cluster_ip" && "$cluster_ip" != None ]]; then
        status="$(curl -k --connect-timeout 2 --max-time 3 -sS -o /dev/null \
            -w '%{http_code}' "https://$cluster_ip:443/healthz" 2>&1 || true)"
        echo "Webhook ClusterIP TCP/HTTPS probe $cluster_ip:443: $status"
    else
        echo "Webhook ClusterIP probe unavailable: Service has no ClusterIP"
    fi
    endpoint_json="$(KUBECONFIG="$kubeconfig" kubectl get endpointslices \
        -n cert-manager -l kubernetes.io/service-name=cert-manager-webhook \
        -o json 2>/dev/null)" || return 0
    while IFS=$'\t' read -r address port; do
        [[ -n "$address" && "$port" =~ ^[0-9]+$ ]] || continue
        status="$(curl -k --connect-timeout 2 --max-time 3 -sS -o /dev/null \
            -w '%{http_code}' "https://$address:$port/healthz" 2>&1 || true)"
        echo "Webhook ready EndpointSlice TCP/HTTPS probe $address:$port: $status"
    done < <(jq -r '
        .items[]? as $slice
        | $slice.ports[]? as $port
        | select($port.port != null)
        | $slice.endpoints[]? as $endpoint
        | select($endpoint.conditions.ready != false)
        | $endpoint.addresses[]?
        | [., $port.port] | @tsv
    ' <<< "$endpoint_json")
}

# The migration watcher runs while target APIs and optional add-ons are still
# converging. Keep kubectl stderr out of jq's JSON input, and record failed
# requests as structured diagnostic data instead of emitting jq parse errors.
diagnostic_kubectl_json() {
    local kubeconfig="${1:?missing diagnostic kubeconfig}"
    local filter="${2:?missing diagnostic jq filter}"
    local error_file response error
    local -a jq_args=()
    shift 2
    if [[ "${1:-}" == --arg ]]; then
        jq_args+=("$1" "$2" "$3")
        shift 3
    fi
    error_file="$(mktemp)" || return 0
    if response="$(KUBECONFIG="$kubeconfig" kubectl "$@" -o json 2>"$error_file")"; then
        if ! jq -cS "${jq_args[@]}" "$filter" <<< "$response" 2>/dev/null; then
            printf '%s\n' '{"diagnosticError":"kubectl returned invalid JSON"}'
        fi
    else
        error="$(<"$error_file")"
        jq -cn --arg error "${error:-kubectl request failed without stderr}" \
            '{kubectlError: $error}'
    fi
    rm -f "$error_file"
}

watch_migration_target_state() {
    local kubeconfig="${1:?missing target kubeconfig}"
    local ready_service="${2:?missing target API service}"
    local stop_file="${3:?missing watcher stop file}"
    local output_file="${4:?missing watcher output file}"
    local previous_snapshot="" snapshot now last_capture=0
    local cache_key mounted_ca_b64 ca_fingerprint
    declare -A mounted_ca_cache=()
    : > "$output_file"
    while [[ ! -e "$stop_file" ]]; do
        if systemctl is-active --quiet "$ready_service" \
            && KUBECONFIG="$kubeconfig" kubectl --request-timeout=2s \
                get --raw=/readyz >/dev/null 2>&1; then
            snapshot="$(
                echo 'system pods:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        phase: .status.phase,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        containers: [.status.containerStatuses[]? | {name, ready, restartCount, state}],
                        initContainers: [.status.initContainerStatuses[]? | {name, ready, restartCount, state}],
                        podIP: .status.podIP,
                        nodeName: .spec.nodeName
                    }]' get pods -n kube-system -l 'k8s-app in (cilium,cilium-envoy,kube-dns)' || true
                echo 'target node scheduling and readiness:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        uid: .metadata.uid,
                        creationTimestamp: .metadata.creationTimestamp,
                        deletionTimestamp: .metadata.deletionTimestamp,
                        unschedulable: (.spec.unschedulable // false),
                        taints: .spec.taints,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        addresses: .status.addresses
                    }]' get nodes || true
                echo 'cert-manager pods:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        phase: .status.phase,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        containers: [.status.containerStatuses[]? | {name, ready, restartCount, state}],
                        podIP: .status.podIP,
                        nodeName: .spec.nodeName
                    }]' get pods -n cert-manager || true
                echo 'cert-manager services and endpoints:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        kind,
                        name: .metadata.name,
                        clusterIP: .spec.clusterIP,
                        selector: .spec.selector,
                        servicePorts: .spec.ports,
                        endpointPorts: .ports,
                        subsets,
                        endpoints: [.endpoints[]? | {addresses, conditions}]
                    }]' get services,endpoints,endpointslices -n cert-manager || true
                echo 'kubernetes API service:'
                diagnostic_kubectl_json "$kubeconfig" \
                    '{clusterIP: .spec.clusterIP, clusterIPs: .spec.clusterIPs, ports: .spec.ports}' \
                    get service kubernetes -n default || true
                echo 'kubernetes API endpoints:'
                diagnostic_kubectl_json "$kubeconfig" \
                    '[.subsets[]? | {addresses, notReadyAddresses, ports}]' \
                    get endpoints kubernetes -n default || true
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        ports,
                        endpoints: [.endpoints[]? | {addresses, conditions, nodeName, targetRef}]
                    }]' get endpointslices -n default \
                    -l kubernetes.io/service-name=kubernetes || true
                echo 'nodeproxy service state:'
                systemctl is-active nodeproxy 2>&1 || true
                echo 'kube-proxy DaemonSet readiness:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        selector: .spec.selector,
                        desired: .status.desiredNumberScheduled,
                        current: .status.currentNumberScheduled,
                        ready: .status.numberReady,
                        available: .status.numberAvailable,
                        misscheduled: .status.numberMisscheduled
                    }]' get daemonsets -n kube-system -l k8s-app=kube-proxy || true
                echo 'kube-proxy Pods:'
                diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                        name: .metadata.name,
                        phase: .status.phase,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        containers: [.status.containerStatuses[]? | {name, ready, restartCount, state}],
                        podIP: .status.podIP,
                        nodeName: .spec.nodeName
                    }]' get pods -n kube-system -l k8s-app=kube-proxy || true
                echo 'cert-manager webhook host-network probes:'
                probe_webhook_route "$kubeconfig"
                echo 'namespace service-account CA bundles match target API CA:'
                target_ca_b64="$(KUBECONFIG="$kubeconfig" kubectl config view \
                    --raw --flatten --minify -o json \
                    | jq -r '.clusters[0].cluster["certificate-authority-data"] // empty' \
                    || true)"
                if [[ -n "$target_ca_b64" ]]; then
                    diagnostic_kubectl_json "$kubeconfig" \
                        '[.items[]? | select(.metadata.name == "kube-root-ca.crt") | {
                              namespace: .metadata.namespace,
                              matchesTargetApiCa: ((.data["ca.crt"] // "" | @base64) == $ca)
                            }]' --arg ca "$target_ca_b64" get configmaps -A || true
                    echo 'target API CA fingerprint:'
                    printf '%s' "$target_ca_b64" | base64 -d 2>/dev/null | sha256sum || true
                else
                    echo 'target admin kubeconfig did not expose a flattened API CA'
                fi
            )"
            now="$(date +%s)"
            if [[ "$snapshot" != "$previous_snapshot" || $((now - last_capture)) -ge 30 ]]; then
                {
                    echo "Target cluster state at $(date -u +%FT%TZ):"
                    printf '%s\n' "$snapshot"
                    echo 'target Node API identity and resource versions:'
                    diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                            name: .metadata.name,
                            uid: .metadata.uid,
                            resourceVersion: .metadata.resourceVersion,
                            creationTimestamp: .metadata.creationTimestamp,
                            deletionTimestamp: .metadata.deletionTimestamp
                        }]' get nodes || true
                    echo 'node Lease identity and renewal state:'
                    diagnostic_kubectl_json "$kubeconfig" '[.items[]? | {
                            name: .metadata.name,
                            uid: .metadata.uid,
                            resourceVersion: .metadata.resourceVersion,
                            holderIdentity: .spec.holderIdentity,
                            renewTime: .spec.renewTime,
                            leaseDurationSeconds: .spec.leaseDurationSeconds,
                            leaseTransitions: .spec.leaseTransitions,
                            ownerReferences: .metadata.ownerReferences
                        }]' get leases -n kube-node-lease || true
                    echo "Target CoreDNS logs:"
                    KUBECONFIG="$kubeconfig" kubectl logs -n kube-system \
                        -l k8s-app=kube-dns --all-containers --tail=100 2>&1 || true
                    echo "Target Cilium agent logs:"
                    capture_cilium_agent_cri_security "$kubeconfig"
                    KUBECONFIG="$kubeconfig" kubectl logs -n kube-system \
                        -l k8s-app=cilium -c cilium-agent --tail=100 2>&1 || true
                    echo "Target cert-manager webhook logs:"
                    KUBECONFIG="$kubeconfig" kubectl logs -n cert-manager \
                        -l app.kubernetes.io/component=webhook --all-containers --tail=100 2>&1 || true
                    echo "Target events:"
                    KUBECONFIG="$kubeconfig" kubectl get events -A \
                        --sort-by=.lastTimestamp 2>&1 | tail -n 80 || true
                } >> "$output_file"
                capture_mounted_service_account_ca "$kubeconfig" >> "$output_file"
                previous_snapshot="$snapshot"
                last_capture="$now"
            fi
        fi
        sleep 5
    done
}

capture_mounted_service_account_ca() {
    local kubeconfig="${1:?missing target kubeconfig}"
    local pod_ns pod_name pod_uid container_name cache_key mounted_ca_b64 ca_fingerprint
    echo 'Mounted service-account CA fingerprints for Cilium and cert-manager:'
    while IFS=$'\t' read -r pod_ns pod_name pod_uid container_name; do
        [[ -n "$pod_name" && -n "$pod_uid" && -n "$container_name" ]] || continue
        cache_key="$pod_uid/$container_name"
        if [[ -z "${mounted_ca_cache[$cache_key]+present}" ]]; then
            if mounted_ca_b64="$(KUBECONFIG="$kubeconfig" \
                kubectl --request-timeout=5s exec -n "$pod_ns" "$pod_name" \
                    -c "$container_name" -- \
                    cat /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
                    2>/dev/null | base64 -w0)"; then
                ca_fingerprint="$(printf '%s' "$mounted_ca_b64" | base64 -d 2>/dev/null \
                    | sha256sum | awk '{print $1}')"
                mounted_ca_cache[$cache_key]="sha256:$ca_fingerprint"
            else
                # Cilium and cert-manager commonly use distroless images without
                # `cat`. Record the unavailable probe once per Pod UID/container
                # instead of emitting an exec error every watcher interval.
                mounted_ca_cache[$cache_key]=unavailable
            fi
        fi
        printf '%s/%s container=%s ca=%s\n' \
            "$pod_ns" "$pod_name" "$container_name" "${mounted_ca_cache[$cache_key]}"
    done < <(KUBECONFIG="$kubeconfig" kubectl get pods -A -o json 2>/dev/null \
        | jq -r '
            .items[]?
            | select((.metadata.namespace == "kube-system" and
                      (.metadata.labels["k8s-app"] // "") == "cilium") or
                     (.metadata.namespace == "cert-manager" and
                      (.metadata.labels["app.kubernetes.io/component"] // "") == "webhook"))
            | .metadata.namespace as $ns
            | .metadata.name as $pod
            | .metadata.uid as $uid
            | .spec.containers[]?.name
            | [$ns, $pod, $uid, .] | @tsv
          ')
}

stop_target_forward_watch() {
    [[ -n "$TARGET_WATCH_PID" ]] || return 0
    touch "$TARGET_WATCH_STOP_FILE"
    wait "$TARGET_WATCH_PID" 2>/dev/null || true
    TARGET_WATCH_PID=""
    echo "Target state captured by the migration watcher:"
    if [[ -s "$TARGET_WATCH_LOG" ]]; then
        cat "$TARGET_WATCH_LOG"
    else
        echo "The target API did not become ready while the migration was running."
    fi
}

capture_cilium_datapath() {
    local kubeconfig="${1:-${KUBECONFIG:-${CURRENT_KUBECONFIG:-$SOURCE_KUBECONFIG}}}"
    local pod
    pod="$(KUBECONFIG="$kubeconfig" kubectl -n kube-system get pods \
        -l k8s-app=cilium -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
    if [[ -z "$pod" ]]; then
        echo "Cilium agent Pod is unavailable for datapath capture"
        return 0
    fi
    echo "Cilium datapath from pod/$pod:"
    KUBECONFIG="$kubeconfig" kubectl -n kube-system get pod "$pod" \
        -o jsonpath='pod-uid={.metadata.uid} clean-cilium-state-init-exit-code={.status.initContainerStatuses[?(@.name=="clean-cilium-state")].state.terminated.exitCode}{"\n"}' \
        2>&1 || true
    KUBECONFIG="$kubeconfig" kubectl -n kube-system get configmap cilium-config \
        -o go-template='clean-cilium-state={{index .data "clean-cilium-state"}} clean-cilium-bpf-state={{index .data "clean-cilium-bpf-state"}}{{"\n"}}' \
        2>&1 || true
    KUBECONFIG="$kubeconfig" kubectl -n kube-system exec "$pod" -c cilium-agent -- \
        cilium-dbg status --verbose 2>&1 || true
    echo "Cilium Kubernetes Service datapath from pod/$pod:"
    KUBECONFIG="$kubeconfig" kubectl -n kube-system exec "$pod" -c cilium-agent -- \
        cilium-dbg service list 2>&1 || true
    echo "Cilium BPF load-balancer map from pod/$pod:"
    KUBECONFIG="$kubeconfig" kubectl -n kube-system exec "$pod" -c cilium-agent -- \
        cilium-dbg bpf lb list 2>&1 || true
    echo "Cilium endpoint state from pod/$pod:"
    KUBECONFIG="$kubeconfig" kubectl -n kube-system exec "$pod" -c cilium-agent -- \
        cilium-dbg endpoint list 2>&1 || true
}

probe_api_clusterip_from_pod() {
    local kubeconfig="${1:?missing probe kubeconfig}"
    local stage="${2:?missing probe stage}"
    local cluster_ip pod_name phase deadline
    cluster_ip="$(KUBECONFIG="$kubeconfig" kubectl get service kubernetes \
        -n default -o jsonpath='{.spec.clusterIP}')"
    [[ "$cluster_ip" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || {
        echo "Kubernetes Service has an invalid ClusterIP at stage=$stage: $cluster_ip" >&2
        return 1
    }
    pod_name="nodemigrate-api-route-$stage"
    KUBECONFIG="$kubeconfig" kubectl delete pod "$pod_name" -n kube-system \
        --ignore-not-found --wait=true --timeout=30s >/dev/null
    KUBECONFIG="$kubeconfig" kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $pod_name
  namespace: kube-system
  labels:
    app.kubernetes.io/name: nodemigrate-api-route-probe
spec:
  restartPolicy: Never
  containers:
  - name: probe
    image: busybox:1.36.1
    imagePullPolicy: IfNotPresent
    command: ["sh", "-ec", "nc -z -w 5 $cluster_ip 443"]
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
EOF
    deadline=$((SECONDS + 90))
    while (( SECONDS < deadline )); do
        phase="$(KUBECONFIG="$kubeconfig" kubectl get pod "$pod_name" \
            -n kube-system -o jsonpath='{.status.phase}' 2>/dev/null || true)"
        case "$phase" in
            Succeeded)
                echo "PASS Pod-to-Service API TCP probe at stage=$stage clusterIP=$cluster_ip"
                KUBECONFIG="$kubeconfig" kubectl delete pod "$pod_name" \
                    -n kube-system --wait=true --timeout=30s >/dev/null
                return 0
                ;;
            Failed)
                KUBECONFIG="$kubeconfig" kubectl logs "$pod_name" -n kube-system >&2 || true
                KUBECONFIG="$kubeconfig" kubectl describe pod "$pod_name" -n kube-system >&2 || true
                KUBECONFIG="$kubeconfig" kubectl delete pod "$pod_name" \
                    -n kube-system --wait=true --timeout=30s >/dev/null || true
                echo "Pod-to-Service API TCP probe failed at stage=$stage clusterIP=$cluster_ip" >&2
                return 1
                ;;
        esac
        sleep 2
    done
    KUBECONFIG="$kubeconfig" kubectl describe pod "$pod_name" -n kube-system >&2 || true
    KUBECONFIG="$kubeconfig" kubectl delete pod "$pod_name" \
        -n kube-system --wait=true --timeout=30s >/dev/null || true
    echo "Pod-to-Service API TCP probe timed out at stage=$stage clusterIP=$cluster_ip" >&2
    return 1
}

restart_cilium_agent_pod() {
    local kubeconfig="${1:?missing probe kubeconfig}"
    local old_pod_json old_pod_name old_pod_uid replacement_json attempt
    old_pod_json="$(KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
        -l k8s-app=cilium -o json | jq -ce '
          if (.items | length) == 1 then .items[0] else empty end
        ')" || {
        echo "expected exactly one K3s Cilium agent Pod before restart" >&2
        return 1
    }
    old_pod_name="$(jq -er '.metadata.name' <<< "$old_pod_json")"
    old_pod_uid="$(jq -er '.metadata.uid' <<< "$old_pod_json")"
    echo "Restarting Cilium agent Pod $old_pod_name UID=$old_pod_uid without changing Node identity"
    KUBECONFIG="$kubeconfig" kubectl delete pod "$old_pod_name" -n kube-system \
        --wait=true --timeout=120s

    for attempt in $(seq 1 90); do
        replacement_json="$(KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
            -l k8s-app=cilium -o json 2>/dev/null | jq -c --arg old_uid "$old_pod_uid" '
              .items[]?
              | select(.metadata.uid != $old_uid and .status.phase == "Running")
              | select(any(.status.conditions[]?; .type == "Ready" and .status == "True"))
              | {name: .metadata.name, uid: .metadata.uid}
            ' | head -n 1)" || replacement_json=""
        if [[ -n "$replacement_json" ]]; then
            echo "Replacement Cilium agent Pod $(jq -r '.name' <<< "$replacement_json") UID=$(jq -r '.uid' <<< "$replacement_json") is Ready"
            return 0
        fi
        sleep 2
    done
    echo "Cilium agent Pod did not become Ready with a new UID after deletion" >&2
    KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system -l k8s-app=cilium -o wide >&2 || true
    return 1
}

stop_source_cilium_sandboxes_for_probe() {
    local endpoint="${NODEMIGRATE_CRI_ENDPOINT:-unix:///run/k3s/containerd/containerd.sock}"
    local pod_json sandbox_ids container_json container_ids id
    local -a cilium_sandboxes=()
    local -a cilium_containers=()
    pod_json="$(crictl --runtime-endpoint "$endpoint" pods -o json)" || {
        echo "could not list K3s CRI pod sandboxes for the cutover diagnostic" >&2
        return 1
    }
    sandbox_ids="$(jq -er '
      if (.items | type) != "array" then error("CRI response has no items array") else
        [.items[]?
         | select((.metadata.namespace // "") == "kube-system")
         | select((.id? | type) == "string" and (.id | length) > 0)
         | select((.metadata.name // "" | startswith("cilium"))
                  or (.labels["k8s-app"] // "" | IN("cilium", "cilium-envoy")))
         | .id]
        | if length == 0 then error("no source Cilium pod sandboxes were found")
          else .[] end
      end
    ' <<< "$pod_json")" || return 1
    mapfile -t cilium_sandboxes <<< "$sandbox_ids"

    container_json="$(crictl --runtime-endpoint "$endpoint" ps -o json)" || {
        echo "could not list running K3s Cilium containers for the cutover diagnostic" >&2
        return 1
    }
    container_ids="$(jq -er '
      if (.containers | type) != "array" then error("CRI response has no containers array") else
        [.containers[]?
         | select((.labels["io.kubernetes.pod.namespace"] // "") == "kube-system")
         | select((.metadata.name // "" | startswith("cilium"))
                  or (.labels["k8s-app"] // "" | IN("cilium", "cilium-envoy")))
         | select((.id? | type) == "string" and (.id | length) > 0)
         | .id]
        | if length == 0 then error("no running source Cilium containers were found")
          else .[] end
      end
    ' <<< "$container_json")" || return 1
    mapfile -t cilium_containers <<< "$container_ids"

    for id in "${cilium_containers[@]}"; do
        crictl --runtime-endpoint "$endpoint" stop "$id" || return 1
        crictl --runtime-endpoint "$endpoint" rm "$id" || return 1
    done

    for id in "${cilium_sandboxes[@]}"; do
        crictl --runtime-endpoint "$endpoint" stopp "$id" || return 1
        crictl --runtime-endpoint "$endpoint" rmp "$id" || return 1
    done
    echo "PASS stopped and removed ${#cilium_containers[@]} source Cilium container(s) and ${#cilium_sandboxes[@]} sandbox(es); ordinary workloads were retained"
}

capture_cni_host_diagnostics() {
    local config file
    for config in /etc/containerd/config.toml \
        /var/lib/rancher/k3s/agent/etc/containerd/config.toml; do
        [[ -f "$config" ]] || continue
        echo "Containerd CNI settings from $config:"
        awk '
            /^\[plugins\..*\.cni\]$/ { printing = 1; print; next }
            printing && /^\[/ { printing = 0 }
            printing { print }
        ' "$config"
    done
    for dir in /etc/cni/net.d /opt/cni/bin \
        /var/lib/rancher/k3s/agent/etc/cni/net.d \
        /var/lib/rancher/k3s/data/current/bin; do
        [[ -e "$dir" ]] || continue
        echo "CNI directory inventory: $dir"
        ls -la "$dir" || true
        if [[ -d "$dir" && "$dir" == */net.d ]]; then
            while IFS= read -r -d '' file; do
                echo "CNI config summary: $file"
                jq -c '(.plugins // [ . ]) | map({type, name, log_file})' \
                    "$file" 2>/dev/null || echo "CNI config is not JSON: $file"
                sha256sum "$file" || true
            done < <(find "$dir" -maxdepth 1 -type f \
                \( -name '*.conf' -o -name '*.conflist' -o -name '*.json' \) \
                -print0 2>/dev/null)
        fi
    done
    for dir in /var/run/cilium /run/cilium; do
        [[ -e "$dir" ]] || continue
        echo "Cilium runtime socket inventory: $dir"
        find "$dir" -maxdepth 3 -type s -printf '%p\n' 2>/dev/null || true
    done
    capture_cilium_datapath
}

watch_cilium_mount_cgroup_logs() {
    local container_json containers container_id container_name state output
    declare -A last_logs=()
    while true; do
        container_json="$(crictl --runtime-endpoint unix:///run/containerd/containerd.sock ps -a -o json 2>/dev/null)" || container_json='{"containers":[]}'
        containers="$(jq -r '
            .containers[]?
            | select(.labels["nodelet.dev/container-name"] == "mount-cgroup")
            | select(.labels["nodelet.dev/pod-namespace"] == "kube-system")
            | select((.labels["nodelet.dev/pod-name"] // "") | startswith("cilium-"))
            | [.id, .labels["nodelet.dev/container-name"], .state] | @tsv
        ' <<< "$container_json")"
        while IFS=$'\t' read -r container_id container_name state; do
            [[ -n "$container_id" ]] || continue
            if [[ -z "${last_logs[$container_id]+present}" ]]; then
                echo "Watching Cilium init container pod identity by CRI labels name=$container_name id=$container_id state=$state" >&2
                crictl --runtime-endpoint unix:///run/containerd/containerd.sock \
                    inspect "$container_id" >&2 || true
                last_logs[$container_id]=""
            fi
            output="$(crictl --runtime-endpoint unix:///run/containerd/containerd.sock \
                logs --tail=100 "$container_id" 2>&1 || true)"
            if [[ -n "$output" && "${last_logs[$container_id]}" != "$output" ]]; then
                echo "Cilium mount-cgroup logs id=$container_id state=$state" >&2
                printf '%s\n' "$output" >&2
                last_logs[$container_id]="$output"
            fi
        done <<< "$containers"
        sleep 0.5
    done
}

diagnostics() {
    status=$?
    if [[ $status -ne 0 ]]; then
        stop_target_forward_watch || true
        echo "Migration integration failed at $(date -u +%FT%TZ), exit=$status"
        echo "source=$SOURCE_DIST kubeconfig=${CURRENT_KUBECONFIG:-unset}"
        if [[ "$SOURCE_DIST" == k3s ]]; then
            local audit_log="${NODEMIGRATE_K3S_AUDIT_LOG:-/var/lib/rancher/k3s/server/logs/nodemigrate-audit.log}"
            if [[ -f "$audit_log" ]]; then
                echo "K3s node mutation audit log ($audit_log):"
                cat "$audit_log" || true
            else
                echo "K3s node mutation audit log is missing: $audit_log"
            fi
            echo "K3s service identity and state:"
            systemctl show k3s -p ActiveState -p SubState -p MainPID -p InvocationID \
                -p ExecMainStatus -p Result --no-pager || true
            systemctl status k3s --no-pager --full || true
            if [[ -n "$MIGRATION_STARTED_AT" ]]; then
                echo "K3s service journal since migration start ($MIGRATION_STARTED_AT):"
                journalctl -b -u k3s --since "$MIGRATION_STARTED_AT" \
                    --no-pager -o short-iso-precise || true
                echo "Kernel journal since migration start ($MIGRATION_STARTED_AT):"
                journalctl -b -k --since "$MIGRATION_STARTED_AT" \
                    --no-pager -o short-iso-precise || true
            fi
        fi
        if [[ -n "$CURRENT_KUBECONFIG" && -f "$CURRENT_KUBECONFIG" ]]; then
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get nodes -o wide || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe nodes || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pods,pvc,pv -A -o wide || true
            echo "Aggregated metrics API diagnostics:"
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get apiservices v1beta1.metrics.k8s.io -o yaml || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe apiservice v1beta1.metrics.k8s.io || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe deployment metrics-server -n kube-system || true
            while IFS= read -r metrics_pod; do
                [[ -n "$metrics_pod" ]] || continue
                KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pod -n kube-system "$metrics_pod" || true
                KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n kube-system "$metrics_pod" \
                    --all-containers --tail=200 || true
                KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n kube-system "$metrics_pod" \
                    --all-containers --previous --tail=200 || true
            done < <(KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pods -n kube-system -o json \
                | jq -r '.items[] | select(.metadata.name | startswith("metrics-server-")) | .metadata.name' 2>/dev/null || true)
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get statefulset migration-stateful \
                -n migration-apps -o json | jq '{
                  metadata: {generation: .metadata.generation},
                  spec: {replicas: .spec.replicas, updateStrategy: .spec.updateStrategy},
                  status: {
                    observedGeneration: .status.observedGeneration,
                    replicas: .status.replicas,
                    readyReplicas: .status.readyReplicas,
                    updatedReplicas: .status.updatedReplicas,
                    currentRevision: .status.currentRevision,
                    updateRevision: .status.updateRevision
                  }
                }' || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe statefulset migration-stateful \
                -n migration-apps || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pod migration-stateful-0 \
                -n migration-apps || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pvc state-migration-stateful-0 \
                -n migration-apps || true
            stateful_volume="$(KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pvc state-migration-stateful-0 \
                -n migration-apps -o jsonpath='{.spec.volumeName}' 2>/dev/null || true)"
            if [[ -n "$stateful_volume" ]]; then
                echo "StatefulSet PersistentVolume spec: $stateful_volume"
                KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pv "$stateful_volume" -o yaml || true
            fi
            echo "Target node labels relevant to PersistentVolume topology:"
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get nodes -o custom-columns='NAME:.metadata.name,LABELS:.metadata.labels' || true
            echo "Hostpath CSI driver logs:"
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n default csi-hostpathplugin-0 \
                --all-containers --tail=500 || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n default csi-hostpath-socat-0 \
                --all-containers --tail=200 || true
            for pod in migration-seed-static migration-seed-csi migration-standalone; do
                KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pod -n migration-apps "$pod" || true
            done
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pods -n migration-apps \
                -l app=migration-nginx || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get gatewayclass,gateway,httproute -A -o yaml || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get events -A --sort-by=.lastTimestamp | tail -n 100 || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n traefik deployment/traefik \
                --all-containers --tail=500 || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n kube-system deployment/cilium-operator \
                --all-containers --tail=100 || true
            echo "CoreDNS readiness for current kubeconfig:"
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pods -n kube-system \
                -l k8s-app=kube-dns -o wide || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe pods -n kube-system \
                -l k8s-app=kube-dns || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl logs -n kube-system \
                -l k8s-app=kube-dns --all-containers --tail=200 || true
            capture_cilium_agent_logs
            capture_cilium_agent_cri_security "$CURRENT_KUBECONFIG"
            capture_cilium_init_container_diagnostics
        fi
        for cni_path in /etc/cni/net.d /opt/cni/bin \
            /var/lib/rancher/k3s/agent/etc/cni/net.d /var/lib/rancher/k3s/data/current/bin; do
            if [[ -e "$cni_path" ]]; then
                echo "CNI diagnostic path: $cni_path"
                ls -la "$cni_path" || true
            fi
        done
        KUBECONFIG="$CURRENT_KUBECONFIG" capture_cni_host_diagnostics || true
        echo "Cilium Envoy host-process ownership:"
        capture_cilium_envoy_process_owners || true
        if [[ -n "$MIGRATION_STARTED_AT" ]]; then
            echo "CiliumNode API requests since migration start ($MIGRATION_STARTED_AT):"
            journalctl -b -u nodeapiserver --since "$MIGRATION_STARTED_AT" \
                --no-pager -o cat 2>/dev/null \
                | grep -Ei 'ciliumnode|/apis/cilium\.io/v2/ciliumnodes' || true
        fi
        journalctl -b -u k3s -u kubelet -u containerd -u nodestore -u nodeapiserver \
            -u nodecontroller -u nodescheduler -u nodelet -u kube-apiserver \
            --no-pager -n 500 || true
        if [[ -n "$MIGRATION_STARTED_AT" ]]; then
            echo "Nodelet volume/runtime logs since migration start ($MIGRATION_STARTED_AT):"
            journalctl -b -u nodelet --since "$MIGRATION_STARTED_AT" --no-pager -n 1500 || true
            echo "Scheduler volume-binding logs since migration start ($MIGRATION_STARTED_AT):"
            journalctl -b -u nodescheduler --since "$MIGRATION_STARTED_AT" --no-pager -n 1000 || true
        fi
        echo "Target API server diagnostics:"
        systemctl status nodeapiserver --no-pager || true
        journalctl -b -u nodeapiserver --no-pager -n 1000 || true
        systemctl status nodestore --no-pager || true
        journalctl -b -u nodestore --no-pager -n 1000 || true
        echo "Target nodeproxy diagnostics:"
        systemctl status nodeproxy --no-pager || true
        journalctl -b -u nodeproxy --no-pager -n 1000 || true
    fi
    exit "$status"
}
if [[ "$LIBRARY_MODE" != true ]]; then
    trap diagnostics EXIT
fi

need_root() {
    [[ "$(id -u)" == 0 ]] || { echo "run this integration script as root" >&2; exit 2; }
    if [[ "${NODEMIGRATE_K3S_CILIUM_RESTART_PROBE:-false}" == true ]]; then
        [[ "$SOURCE_DIST" == k3s && "${NODEMIGRATE_CILIUM_KPR:-false}" == true ]] || {
            echo "the Cilium restart probe requires K3s with Cilium KPR enabled" >&2
            exit 2
        }
        [[ -x "$NK" ]] || {
            echo "build target/release/notk8s for the K3s Cilium restart probe" >&2
            exit 2
        }
    elif [[ ! -x "$NK" || ! -x "$MIGRATE" ]]; then
        echo "build target/release/notk8s and target/release/nodemigrate first" >&2
        exit 2
    fi
    [[ "$SOURCE_DIST" == k3s || "$SOURCE_DIST" == kubernetes ]] || {
        echo "unsupported source distribution: $SOURCE_DIST" >&2
        exit 2
    }
}

install_tools() {
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq apt-transport-https ca-certificates conntrack curl \
        ebtables ethtool gpg jq socat
    mkdir -p /etc/apt/keyrings
    local stable minor
    stable="$(curl -fsSL https://dl.k8s.io/release/stable.txt)"
    minor="$(sed -E 's/^(v[0-9]+\.[0-9]+)\..*/\1/' <<<"$stable")"
    curl -fsSL "https://pkgs.k8s.io/core:/stable:/${minor}/deb/Release.key" \
        | gpg --dearmor --yes -o /etc/apt/keyrings/kubernetes-apt-keyring.gpg
    printf 'deb [signed-by=/etc/apt/keyrings/kubernetes-apt-keyring.gpg] https://pkgs.k8s.io/core:/stable:/%s/deb/ /\n' \
        "$minor" > /etc/apt/sources.list.d/kubernetes.list
    apt-get update -qq
    apt-get install -y -qq kubectl

    local helm_version=v3.17.3
    if ! command -v helm >/dev/null 2>&1; then
        curl -fsSL "https://get.helm.sh/helm-${helm_version}-linux-amd64.tar.gz" -o /tmp/helm.tgz
        tar -xzf /tmp/helm.tgz -C /tmp
        install -m0755 /tmp/linux-amd64/helm /usr/local/bin/helm
    fi
    helm version --short
    kubectl version --client
    NODEMIGRATE_KUBECTL_IMAGE="registry.k8s.io/kubectl:$(kubectl version --client -o json | jq -r '.clientVersion.gitVersion')"
    export NODEMIGRATE_KUBECTL_IMAGE
}

install_containerd() {
    export NOTK8S_COMBINED_PREBUILT="$NK"
    export NODEBOOTSTRAP_COMBINED_SELF="$NK"
    export NOTK8S_BUILD_LAYOUT=combined
    export NODEBOOTSTRAP_REPO_ROOT="$ROOT"
    "$NK" bootstrap containerd
    systemctl is-active --quiet containerd
    ctr version
}

install_source() {
    install_containerd
    local cilium_kpr="${NODEMIGRATE_CILIUM_KPR:-false}"
    [[ "$cilium_kpr" == false || "$cilium_kpr" == true ]] || {
        echo "NODEMIGRATE_CILIUM_KPR must be false or true, got '$cilium_kpr'" >&2
        return 2
    }
    if [[ "$SOURCE_DIST" == k3s ]]; then
        apt-get install -y -qq containernetworking-plugins
        local cni_bridge cni_plugin_dir
        cni_bridge="$(dpkg -L containernetworking-plugins | awk '/\/bridge$/ { print }' | tail -n 1)"
        [[ -n "$cni_bridge" && -x "$cni_bridge" ]] \
            || { echo "containernetworking-plugins did not install its bridge binary" >&2; return 1; }
        cni_plugin_dir="${cni_bridge%/*}"
        [[ -x "$cni_plugin_dir/loopback" ]] \
            || { echo "containernetworking-plugins did not install loopback binary" >&2; return 1; }
        if [[ "$cni_plugin_dir" != /opt/cni/bin ]]; then
            install -d /opt/cni/bin
            for plugin in "$cni_plugin_dir"/*; do
                ln -sfn "$plugin" "/opt/cni/bin/${plugin##*/}"
            done
        fi
        local k3s_version="${K3S_VERSION:-v1.35.0+k3s1}"
        local kube_proxy_flag=""
        if [[ "$cilium_kpr" == true ]]; then
            kube_proxy_flag="--disable-kube-proxy"
        fi
        install -d -m 0755 /etc/rancher/k3s
        cat > /etc/rancher/k3s/nodemigrate-audit-policy.yaml <<'EOF'
apiVersion: audit.k8s.io/v1
kind: Policy
omitStages:
  - RequestReceived
rules:
  - level: Metadata
    verbs: [create, update, patch, delete, deletecollection]
    resources:
      - group: ""
        resources: [nodes, nodes/status]
  - level: Metadata
    verbs: [create, update, patch, delete, deletecollection]
    namespaces: [kube-node-lease]
    resources:
      - group: coordination.k8s.io
        resources: [leases]
  - level: None
EOF
        local k3s_release_dir="${RUNNER_TEMP:-/tmp}/nodemigrate-k3s-${k3s_version//+/\_}"
        local k3s_release_ref="${k3s_version//+/\%2B}"
        mkdir -p "$k3s_release_dir"
        gh release download "$k3s_version" --repo k3s-io/k3s \
            --pattern k3s --pattern sha256sum-amd64.txt --dir "$k3s_release_dir"
        (
            cd "$k3s_release_dir"
            grep -E '^[[:xdigit:]]{64}  k3s$' sha256sum-amd64.txt \
                | sha256sum --check --status
        ) || {
            echo "official K3s binary checksum validation failed for $k3s_version" >&2
            return 1
        }
        install -m 0755 "$k3s_release_dir/k3s" /usr/local/bin/k3s
        gh api -H 'Accept: application/vnd.github.raw+json' \
            "repos/k3s-io/k3s/contents/install.sh?ref=$k3s_release_ref" \
            > /tmp/install-k3s.sh
        INSTALL_K3S_SKIP_DOWNLOAD=true \
        INSTALL_K3S_VERSION="$k3s_version" \
        INSTALL_K3S_EXEC="server $kube_proxy_flag --flannel-backend=none --disable-network-policy --disable=traefik --cluster-cidr=10.42.0.0/16 --write-kubeconfig-mode=644 --kube-apiserver-arg=audit-policy-file=/etc/rancher/k3s/nodemigrate-audit-policy.yaml --kube-apiserver-arg=audit-log-path=/var/lib/rancher/k3s/server/logs/nodemigrate-audit.log" \
            sh /tmp/install-k3s.sh
        SOURCE_KUBECONFIG=/etc/rancher/k3s/k3s.yaml
    else
        local stable minor
        stable="$(curl -fsSL https://dl.k8s.io/release/stable.txt)"
        minor="$(sed -E 's/^(v[0-9]+\.[0-9]+)\..*/\1/' <<<"$stable")"
        apt-get install -y -qq kubelet kubeadm
        [[ -x /opt/cni/bin/bridge && -x /opt/cni/bin/loopback ]] || {
            echo "kubelet's kubernetes-cni dependency did not install bridge and loopback" >&2
            return 1
        }
        local cri_tools_version="${CRI_TOOLS_VERSION:-${minor}.0}"
        local cri_tools_arch
        case "$(uname -m)" in
            x86_64) cri_tools_arch=amd64 ;;
            aarch64|arm64) cri_tools_arch=arm64 ;;
            *) echo "unsupported crictl architecture: $(uname -m)" >&2; return 1 ;;
        esac
        curl -fsSL "https://github.com/kubernetes-sigs/cri-tools/releases/download/${cri_tools_version}/crictl-${cri_tools_version}-linux-${cri_tools_arch}.tar.gz" \
            -o /tmp/crictl.tgz
        tar -xzf /tmp/crictl.tgz -C /usr/local/bin crictl
        chmod 0755 /usr/local/bin/crictl
        echo "cri-tools version=$cri_tools_version architecture=$cri_tools_arch"
        crictl --version
        apt-mark hold kubelet kubeadm kubectl
        swapoff -a
        sed -i.bak '/\sswap\s/s/^/#/' /etc/fstab
        local -a kubeadm_proxy_args=()
        if [[ "$cilium_kpr" == true ]]; then
            kubeadm_proxy_args+=(--skip-phases=addon/kube-proxy)
        fi
        kubeadm init \
            --kubernetes-version "$stable" \
            --pod-network-cidr=10.42.0.0/16 \
            --service-cidr=10.96.0.0/12 \
            --cri-socket=unix:///run/containerd/containerd.sock \
            "${kubeadm_proxy_args[@]}"
        SOURCE_KUBECONFIG=/etc/kubernetes/admin.conf
        export KUBECONFIG="$SOURCE_KUBECONFIG"
        kubectl taint nodes --all node-role.kubernetes.io/control-plane- || true
        echo "upstream kubernetes version=$stable channel=$minor"
    fi
    export KUBECONFIG="$SOURCE_KUBECONFIG"
    CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
    kubectl wait --for=condition=Ready node --all --timeout=180s || true
    kubectl get nodes -o wide
}

install_cilium() {
    local version="${CILIUM_VERSION:-1.20.2}"
    local kube_proxy_replacement="${NODEMIGRATE_CILIUM_KPR:-false}"
    local cni_conf_path=/etc/cni/net.d
    local cni_bin_path=/opt/cni/bin
    local api_host="${NODEMIGRATE_CILIUM_API_HOST:-}"
    if [[ -z "$api_host" ]]; then
        # K3s can take a few seconds to register its first Node after the API
        # becomes reachable. Do not treat that normal startup window as a
        # missing-CNI error; Cilium itself is what will make the Node Ready.
        local deadline=$((SECONDS + 180))
        while (( SECONDS < deadline )); do
            api_host="$(kubectl get nodes -o json | jq -r '.items[0].status.addresses[]? | select(.type == "InternalIP") | .address' 2>/dev/null | head -n1 || true)"
            [[ -n "$api_host" ]] && break
            sleep 2
        done
    fi
    [[ -n "$api_host" ]] || { echo "could not determine the source API node IP for Cilium" >&2; return 1; }
    helm repo add cilium https://helm.cilium.io/ --force-update
    helm repo update cilium
    helm upgrade --install cilium cilium/cilium \
        --version "$version" --namespace kube-system \
        --set ipam.mode=kubernetes \
        --set cni.confPath="$cni_conf_path" \
        --set cni.binPath="$cni_bin_path" \
        --set kubeProxyReplacement="$kube_proxy_replacement" \
        --set operator.replicas=1 \
        --set k8sServiceHost="$api_host" \
        --set k8sServicePort=6443 \
        --wait --timeout 10m
    kubectl rollout status daemonset/cilium -n kube-system --timeout=10m
    kubectl rollout status deployment/cilium-operator -n kube-system --timeout=10m
    kubectl wait --for=condition=Ready node --all --timeout=5m
    if [[ "$kube_proxy_replacement" == true ]] \
        && kubectl get daemonset kube-proxy -n kube-system >/dev/null 2>&1; then
        echo "Cilium KPR is enabled but kube-proxy DaemonSet is still installed" >&2
        return 1
    fi
    echo "Cilium kube-proxy replacement=$kube_proxy_replacement"
}

patch_hostpath_statefulset() {
    local patch_type="${1:?missing StatefulSet patch type}"
    local patch="${2:?missing StatefulSet patch body}"
    local output attempt
    # The StatefulSet controller writes status while the fixture changes the
    # CSI Pod template. kubectl patch uses a GET/PATCH sequence, so a status
    # write between those requests can legitimately make the patch stale. Each
    # retry invokes kubectl again, which reads the latest object; only retry
    # the API's optimistic-concurrency conflict and surface every other error.
    for attempt in {1..8}; do
        if output="$(kubectl patch statefulset csi-hostpathplugin -n default \
            --type="$patch_type" -p "$patch" 2>&1)"; then
            printf '%s\n' "$output"
            return 0
        fi
        if [[ "$output" != *"the object has been modified"* ]]; then
            printf '%s\n' "$output" >&2
            return 1
        fi
        if (( attempt == 8 )); then
            printf 'hostpath CSI StatefulSet kept changing during patch (%s attempts): %s\n' \
                "$attempt" "$output" >&2
            return 1
        fi
        printf 'hostpath CSI StatefulSet changed during patch; retrying with a fresh read (%s/8)\n' \
            "$attempt" >&2
        sleep "0.$attempt"
    done
}

install_hostpath_driver() {
    local kubelet_data_dir="${1:?missing kubelet data directory}"
    local skip_snapshot_crds="${2:-false}"
    if [[ -n "${NODEMIGRATE_HOSTPATH_SETUP:-}" ]]; then
        cp -- "$NODEMIGRATE_HOSTPATH_SETUP" /tmp/nodemigrate-hostpath-setup.sh
    else
        git -C "$ROOT" fetch --no-tags --depth=1 origin archive-shell-scripts-0.7.1
        git -C "$ROOT" show FETCH_HEAD:deploy/lib/e2e-full-setup.sh > /tmp/nodemigrate-hostpath-setup.sh
    fi
    if ! grep -q '^# ── DRA: ' /tmp/nodemigrate-hostpath-setup.sh; then
        echo "archived e2e setup no longer marks the end of its CSI setup section" >&2
        return 1
    fi
    # The full e2e helper also deploys DRA and requires nodelet's DRA
    # registration. Source K3s/kubelet stages only need the real CSI driver.
    sed -i '/^# ── DRA: /,$d' /tmp/nodemigrate-hostpath-setup.sh
    if [[ "$skip_snapshot_crds" == true ]]; then
        # These CRDs are part of the migrated API state. Keep them intact so
        # this redeployment exercises their imported discovery and does not
        # mutate the objects whose source-to-target identity is being checked.
        sed -i '\|external-snapshotter/v8\.6\.0/client/config/crd/snapshot\.storage\.k8s\.io_|d' \
            /tmp/nodemigrate-hostpath-setup.sh
    fi
    mkdir -p "$kubelet_data_dir/plugins" "$kubelet_data_dir/plugins_registry"
    watch_cilium_mount_cgroup_logs >&2 &
    local cilium_log_watcher_pid=$!
    # Forward migration imports the CSI StatefulSet and its durable /csi-data-dir
    # hostPath from the source cluster. Reapplying the upstream manifest here
    # would reset that volume to the driver's emptyDir default before its
    # existing volume catalog can be checked, orphaning every imported CSI PV.
    # Keep the imported driver and only apply the local staging mount below.
    if [[ "$kubelet_data_dir" == /var/lib/nodelet ]] \
        && kubectl get statefulset csi-hostpathplugin -n default >/dev/null 2>&1; then
        echo "Using the imported hostpath CSI deployment and preserved volume catalog"
    elif ! NODELET_DATA_DIR="$kubelet_data_dir" timeout 600 bash /tmp/nodemigrate-hostpath-setup.sh; then
        kill "$cilium_log_watcher_pid" 2>/dev/null || true
        wait "$cilium_log_watcher_pid" 2>/dev/null || true
        echo "Hostpath CSI setup failed; collecting nodelet and pod teardown diagnostics" >&2
        systemctl status nodelet --no-pager >&2 || true
        journalctl -u nodelet -b --no-pager -n 1000 >&2 || true
        echo "Nodeproxy service diagnostics:" >&2
        systemctl status nodeproxy --no-pager >&2 || true
        journalctl -u nodeproxy -b --no-pager -n 1000 >&2 || true
        echo "Cilium Envoy socket directory contents:" >&2
        find /var/run/cilium/envoy/sockets -maxdepth 2 -ls >&2 || true
        echo "Unix socket listeners:" >&2
        ss -xlpn >&2 || true
        echo "Cilium Envoy processes:" >&2
        ps -eo pid,ppid,stat,comm,args | awk '/[c]ilium-envoy|[e]nvoy/ {print}' >&2 || true
        capture_cilium_envoy_process_owners >&2 || true
        crictl --runtime-endpoint unix:///run/containerd/containerd.sock pods -o json >&2 || true
        crictl --runtime-endpoint unix:///run/containerd/containerd.sock ps -a -o json >&2 || true
        for container_id in $(crictl --runtime-endpoint unix:///run/containerd/containerd.sock ps -a --name cilium-envoy -q 2>/dev/null); do
            echo "Inspecting failed Cilium Envoy container $container_id" >&2
            crictl --runtime-endpoint unix:///run/containerd/containerd.sock inspect "$container_id" >&2 || true
            echo "Cilium Envoy container logs $container_id" >&2
            crictl --runtime-endpoint unix:///run/containerd/containerd.sock logs --tail=500 "$container_id" >&2 || true
        done
        kubectl get pods -A -o wide >&2 || true
        kubectl get pods -n kube-system -l k8s-app=cilium -o yaml >&2 || true
        capture_cilium_agent_logs >&2 || true
        capture_cilium_agent_cri_security "${KUBECONFIG:-$CURRENT_KUBECONFIG}" >&2 || true
        capture_cilium_init_container_diagnostics >&2 || true
        kubectl get pods -A -l app.kubernetes.io/instance=hostpath.csi.k8s.io -o yaml >&2 || true
        kubectl get events -A --sort-by=.metadata.creationTimestamp >&2 || true
        return 1
    fi
    kill "$cilium_log_watcher_pid" 2>/dev/null || true
    wait "$cilium_log_watcher_pid" 2>/dev/null || true

    # The upstream hostpath CSI driver keeps both its volume catalog and
    # volume payloads under /csi-data-dir. Its default pod-local backing is
    # lost when the source distribution stops the CSI StatefulSet, so a
    # re-created target driver cannot stage the imported source handles. Keep
    # this test provider's state on the node disk, which remains present while
    # nodemigrate replaces the runtime on that node.
    local plugin_json state_volume state_volume_index state_patch
    plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    state_volume="$(jq -r '
        [.spec.template.spec.containers[] | select(.name == "hostpath")
         | .volumeMounts[] | select(.mountPath == "/csi-data-dir") | .name][0] // empty
    ' <<<"$plugin_json")"
    [[ -n "$state_volume" ]] || {
        echo "hostpath CSI driver has no /csi-data-dir volume mount" >&2
        return 1
    }
    state_volume_index="$(jq -r --arg name "$state_volume" '
        [.spec.template.spec.volumes | to_entries[] | select(.value.name == $name) | .key][0] // empty
    ' <<<"$plugin_json")"
    [[ "$state_volume_index" =~ ^[0-9]+$ ]] || {
        echo "hostpath CSI /csi-data-dir volume is not declared in the StatefulSet" >&2
        return 1
    }
    state_patch="$(jq -cn \
        --argjson index "$state_volume_index" \
        --arg name "$state_volume" \
        '[{op:"replace",path:("/spec/template/spec/volumes/" + ($index|tostring)),value:{name:$name,hostPath:{path:"/var/lib/nodemigrate-csi-hostpath-data",type:"DirectoryOrCreate"}}}]')"
    patch_hostpath_statefulset json "$state_patch"
    kubectl rollout status statefulset/csi-hostpathplugin -n default --timeout=5m
    kubectl get statefulset csi-hostpathplugin -n default -o json | jq -e \
        --arg name "$state_volume" \
        '.spec.template.spec.volumes[] | select(.name == $name)
         | .hostPath.path == "/var/lib/nodemigrate-csi-hostpath-data"' >/dev/null || {
            echo "hostpath CSI state directory is not backed by the node disk" >&2
            return 1
        }

    if [[ "$kubelet_data_dir" == /var/lib/nodelet ]]; then
        # Nodelet deliberately keeps the source kubelet's global staging path
        # during this same-node migration. Mount that host directory into the
        # replacement CSI Pod with bidirectional propagation so the driver
        # sees the existing NodeStageVolume mount and can publish it again.
        local stage_volume=nodemigrate-source-csi-stage
        local plugin_json stage_container_index stage_patch
        plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
        stage_container_index="$(jq -r '
            [.spec.template.spec.containers | to_entries[] | select(.value.name == "hostpath") | .key][0] // empty
        ' <<<"$plugin_json")"
        [[ "$stage_container_index" =~ ^[0-9]+$ ]] || {
            echo "hostpath CSI StatefulSet has no hostpath container" >&2
            return 1
        }
        stage_patch="$(jq -cn --arg name "$stage_volume" --argjson container "$stage_container_index" '
            [
              {op:"add",path:"/spec/template/spec/volumes/-",value:{name:$name,hostPath:{path:"/var/lib/kubelet/plugins/kubernetes.io/csi",type:"DirectoryOrCreate"}}},
              {op:"add",path:("/spec/template/spec/containers/" + ($container|tostring) + "/volumeMounts/-"),value:{name:$name,mountPath:"/var/lib/kubelet/plugins/kubernetes.io/csi",mountPropagation:"Bidirectional"}}
            ]')"
        patch_hostpath_statefulset json "$stage_patch"
        kubectl rollout status statefulset/csi-hostpathplugin -n default --timeout=5m
        kubectl get statefulset csi-hostpathplugin -n default -o json | jq -e \
            --arg name "$stage_volume" \
            '.spec.template.spec.volumes[] | select(.name == $name)
             | .hostPath.path == "/var/lib/kubelet/plugins/kubernetes.io/csi"' >/dev/null || {
                echo "replacement hostpath CSI Pod cannot see the preserved kubelet staging directory" >&2
                return 1
            }
        kubectl get statefulset csi-hostpathplugin -n default -o json | jq -e \
            --arg name "$stage_volume" \
            '.spec.template.spec.containers[] | select(.name == "hostpath")
             | .volumeMounts[] | select(.name == $name)
             | .mountPath == "/var/lib/kubelet/plugins/kubernetes.io/csi"
               and .mountPropagation == "Bidirectional"' >/dev/null || {
                echo "replacement hostpath CSI container does not receive the preserved staging mount" >&2
                return 1
            }

        # Nodelet creates per-Pod target paths under its own data root before
        # calling NodePublishVolume. The hostpath CSI process runs in a
        # separate container, so it must see that same root to create the
        # target directory and bind mount the staged volume into it.
        local nodelet_root_volume=nodemigrate-nodelet-root
        plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
        stage_container_index="$(jq -r '
            [.spec.template.spec.containers | to_entries[] | select(.value.name == "hostpath") | .key][0] // empty
        ' <<<"$plugin_json")"
        if ! jq -e --arg name "$nodelet_root_volume" \
            'any(.spec.template.spec.volumes[]?; .name == $name)' <<<"$plugin_json" >/dev/null; then
            patch_hostpath_statefulset json \
                "$(jq -cn --arg name "$nodelet_root_volume" \
                    '[{op:"add",path:"/spec/template/spec/volumes/-",value:{name:$name,hostPath:{path:"/var/lib/nodelet",type:"DirectoryOrCreate"}}}]')"
            plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
        fi
        if ! jq -e --arg name "$nodelet_root_volume" \
            '.spec.template.spec.containers[] | select(.name == "hostpath")
             | any(.volumeMounts[]?; .name == $name)' <<<"$plugin_json" >/dev/null; then
            patch_hostpath_statefulset json \
                "$(jq -cn --arg name "$nodelet_root_volume" --argjson container "$stage_container_index" '
                    [{op:"add",path:("/spec/template/spec/containers/" + ($container|tostring) + "/volumeMounts/-"),value:{name:$name,mountPath:"/var/lib/nodelet",mountPropagation:"Bidirectional"}}]')"
        fi
        kubectl rollout status statefulset/csi-hostpathplugin -n default --timeout=5m
        plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
        jq -e --arg name "$nodelet_root_volume" '
            any(.spec.template.spec.volumes[]?; .name == $name and .hostPath.path == "/var/lib/nodelet") and
            any(.spec.template.spec.containers[] | select(.name == "hostpath")
                | .volumeMounts[]?; .name == $name and .mountPath == "/var/lib/nodelet"
                    and .mountPropagation == "Bidirectional")
        ' <<<"$plugin_json" >/dev/null || {
            echo "replacement hostpath CSI container cannot access Nodelet's volume target root" >&2
            return 1
        }
    fi
    kubectl get storageclass csi-hostpath-sc
}

ensure_csi_can_access_nodelet_root() {
    local plugin_json container_index root_volume=nodemigrate-nodelet-root
    plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    container_index="$(jq -r '
        [.spec.template.spec.containers | to_entries[] | select(.value.name == "hostpath") | .key][0] // empty
    ' <<<"$plugin_json")"
    [[ "$container_index" =~ ^[0-9]+$ ]] || {
        echo "hostpath CSI StatefulSet has no hostpath container" >&2
        return 1
    }
    if ! jq -e --arg name "$root_volume" \
        'any(.spec.template.spec.volumes[]?; .name == $name)' <<<"$plugin_json" >/dev/null; then
        patch_hostpath_statefulset json \
            "$(jq -cn --arg name "$root_volume" \
                '[{op:"add",path:"/spec/template/spec/volumes/-",value:{name:$name,hostPath:{path:"/var/lib/nodelet",type:"DirectoryOrCreate"}}}]')"
        plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    fi
    if ! jq -e --arg name "$root_volume" \
        '.spec.template.spec.containers[] | select(.name == "hostpath")
         | any(.volumeMounts[]?; .name == $name)' <<<"$plugin_json" >/dev/null; then
        patch_hostpath_statefulset json \
            "$(jq -cn --arg name "$root_volume" --argjson container "$container_index" '
                [{op:"add",path:("/spec/template/spec/containers/" + ($container|tostring) + "/volumeMounts/-"),value:{name:$name,mountPath:"/var/lib/nodelet",mountPropagation:"Bidirectional"}}]')"
    fi
    kubectl rollout status statefulset/csi-hostpathplugin -n default --timeout=5m
    plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    jq -e --arg name "$root_volume" '
        any(.spec.template.spec.volumes[]?; .name == $name and .hostPath.path == "/var/lib/nodelet") and
        any(.spec.template.spec.containers[] | select(.name == "hostpath")
            | .volumeMounts[]?; .name == $name and .mountPath == "/var/lib/nodelet"
                and .mountPropagation == "Bidirectional")
    ' <<<"$plugin_json" >/dev/null || {
        echo "hostpath CSI container cannot access Nodelet's volume target root" >&2
        return 1
    }
}

pin_hostpath_driver_to_node() {
    local node="${1:?missing hostpath CSI topology node}" node_json topology_value plugin_json patch selector node_selector
    node_json="$(kubectl get node "$node" -o json)"
    topology_value="$(jq -r '.metadata.labels["topology.hostpath.csi/node"] // empty' <<<"$node_json")"
    [[ -n "$topology_value" ]] || {
        echo "CSI topology node $node is not labeled topology.hostpath.csi/node" >&2
        return 1
    }
    plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    selector="$(jq -r '.spec.selector.matchLabels | to_entries | map("\(.key)=\(.value)") | join(",")' <<<"$plugin_json")"
    node_selector="$(jq -c '.spec.template.spec.nodeSelector // {}' <<<"$plugin_json")"
    patch="$(jq -cn --arg node "$node" --arg topology "$topology_value" --argjson current "$node_selector" \
        '{spec:{template:{spec:{nodeSelector:($current + {"kubernetes.io/hostname":$node,"topology.hostpath.csi/node":$topology})}}}}')"
    patch_hostpath_statefulset merge "$patch"
    kubectl rollout status statefulset/csi-hostpathplugin -n default --timeout=5m
    kubectl get pods -n default -l "$selector" -o json | jq -e --arg node "$node" '
      any(.items[]; .spec.nodeName == $node and .status.phase == "Running" and
        all(.status.containerStatuses[]?; .ready == true))
    ' >/dev/null || {
        echo "CSI hostpath driver did not become Ready on PV topology node $node" >&2
        kubectl get pods -n default -l "$selector" -o wide >&2 || true
        return 1
    }
    echo "PASS: hostpath CSI driver is Ready on topology node $node"
}

ensure_hostpath_topology_label() {
    local node="${1:?missing hostpath CSI topology node}" node_json
    node_json="$(kubectl get node "$node" -o json)"
    if [[ -z "$(jq -r '.metadata.labels["topology.hostpath.csi/node"] // empty' <<<"$node_json")" ]]; then
        # This isolated hostpath provider reports a node-specific topology
        # value. Seed it before provisioning, when no PV exists from which to
        # derive the provider's placement, and preserve it through migration.
        kubectl label node "$node" "topology.hostpath.csi/node=$node" --overwrite
    fi
}

pin_hostpath_driver_to_fixture_volumes() {
    local claim pv_json topology_nodes topology_value node
    local claims=(state-migration-stateful-0 migration-csi-pvc)
    local -a topology_values=()
    for claim in "${claims[@]}"; do
        pv_json="$(kubectl get pvc "$claim" -n migration-apps -o json)"
        local pv_name
        pv_name="$(jq -er '.spec.volumeName | select(length > 0)' <<<"$pv_json")"
        pv_json="$(kubectl get pv "$pv_name" -o json)"
        while IFS= read -r node; do
            [[ -n "$node" ]] && topology_values+=("$node")
        done < <(jq -r '
          [.spec.nodeAffinity.required.nodeSelectorTerms[]?.matchExpressions[]?
           | select(.key == "topology.hostpath.csi/node" and .operator == "In")
           | .values[]?] | unique[]
        ' <<<"$pv_json")
    done
    topology_nodes="$(printf '%s\n' "${topology_values[@]}" | LC_ALL=C sort -u)"
    [[ -n "$topology_nodes" ]] || {
        echo "fixture CSI PVs have no topology.hostpath.csi/node placement" >&2
        return 1
    }
    [[ "$(wc -l <<<"$topology_nodes")" -eq 1 ]] || {
        echo "fixture hostpath CSI volumes span multiple topology nodes: $topology_nodes" >&2
        return 1
    }
    topology_value="$topology_nodes"
    node="$(kubectl get nodes -o json | jq -er --arg topology "$topology_value" '
      [.items[]
      | select(.metadata.labels["topology.hostpath.csi/node"] == $topology)
      | .metadata.name][0] // empty
    ')" || {
        echo "no Node advertises hostpath CSI topology value $topology_value" >&2
        return 1
    }
    pin_hostpath_driver_to_node "$node"
    echo "PASS: hostpath CSI driver and fixture PVs use topology node $node"
}

apply_fixture_manifest_with_conflict_retry() {
    local manifest_file="$1"
    local attempt output
    for attempt in 1 2 3 4 5; do
        if output="$(kubectl apply -f "$manifest_file" 2>&1)"; then
            printf '%s\n' "$output"
            return 0
        fi
        printf '%s\n' "$output" >&2
        if [[ "$output" != *"the object has been modified"* || "$attempt" == 5 ]]; then
            return 1
        fi
        echo "Fixture apply conflicted with a concurrent controller update; retry $attempt/5" >&2
        sleep "$attempt"
    done
}

install_workloads() {
    local stage=source
    if [[ -z "${NODEMIGRATE_KUBECTL_IMAGE:-}" ]]; then
        local kubectl_version
        kubectl_version="$(kubectl version --client -o yaml \
            | awk '$1 == "gitVersion:" { print $2; exit }')"
        [[ -n "$kubectl_version" ]] || {
            echo "could not determine the kubectl image version for fixture Jobs" >&2
            return 1
        }
        NODEMIGRATE_KUBECTL_IMAGE="registry.k8s.io/kubectl:$kubectl_version"
        export NODEMIGRATE_KUBECTL_IMAGE
    fi
    kubectl label nodes --all operator.example/pool=blue --overwrite
    kubectl annotate nodes --all nodemigrate.io/source-uid=operator-node-value --overwrite
    kubectl taint nodes --all operator.example/dedicated=migration:PreferNoSchedule --overwrite
    helm repo add jetstack https://charts.jetstack.io --force-update
    helm repo add traefik https://traefik.github.io/charts --force-update
    helm repo update
    local gateway_api_version="${GATEWAY_API_VERSION:-v1.6.1}"
    kubectl apply -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/${gateway_api_version}/standard-install.yaml"
    kubectl wait --for=condition=Established crd/gatewayclasses.gateway.networking.k8s.io --timeout=2m
    kubectl wait --for=condition=Established crd/httproutes.gateway.networking.k8s.io --timeout=2m
    helm upgrade --install cert-manager jetstack/cert-manager \
        --version "${CERT_MANAGER_VERSION:-v1.21.2}" \
        --namespace cert-manager --create-namespace --set crds.enabled=true \
        --wait --timeout 10m
    helm upgrade --install traefik traefik/traefik \
        --namespace traefik --create-namespace \
        --set service.type=ClusterIP --set ingressClass.enabled=true \
        --set providers.kubernetesGateway.enabled=true
    # Do not create Gateway API resources until the controller is ready to
    # reconcile them; otherwise fixture setup can time out on stale status.
    kubectl rollout status -n traefik deployment/traefik --timeout=5m

    # Exercise a user CRD with two served API versions and a durable custom
    # resource. Keep its schema stable across versions so conversion strategy
    # None can be checked through both discovery endpoints.
    kubectl apply -f - <<'YAML'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: migrationrecords.migration.nodemigrate.io
spec:
  group: migration.nodemigrate.io
  scope: Cluster
  names:
    plural: migrationrecords
    singular: migrationrecord
    kind: MigrationRecord
    listKind: MigrationRecordList
  conversion:
    strategy: None
  versions:
  - name: v1alpha1
    served: true
    storage: false
    subresources:
      status: {}
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            required: [marker]
            properties:
              marker:
                type: string
          status:
            type: object
            properties:
              migrationStage:
                type: string
  - name: v1
    served: true
    storage: true
    subresources:
      status: {}
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            required: [marker]
            properties:
              marker:
                type: string
          status:
            type: object
            properties:
              migrationStage:
                type: string
YAML
    kubectl wait --for=condition=Established crd/migrationrecords.migration.nodemigrate.io --timeout=2m
    kubectl apply -f - <<'YAML'
apiVersion: migration.nodemigrate.io/v1
kind: MigrationRecord
metadata:
  name: migration-record-0
spec:
  marker: durable-custom-resource-data
YAML

    mkdir -p "$STATIC_PATH"
    local fixture_manifest
    fixture_manifest="$(mktemp)"
    cat > "$fixture_manifest" <<'YAML'
apiVersion: v1
kind: Namespace
metadata:
  name: migration-apps
---
apiVersion: v1
kind: Service
metadata:
  name: migration-external-db
  namespace: migration-apps
spec:
  ports:
  - name: postgres
    port: 5432
    protocol: TCP
    targetPort: 5432
---
apiVersion: v1
kind: Endpoints
metadata:
  name: migration-external-db
  namespace: migration-apps
subsets:
- addresses:
  - ip: 192.0.2.20
  ports:
  - name: postgres
    port: 5432
    protocol: TCP
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: migration-external-db-v4
  namespace: migration-apps
  labels:
    kubernetes.io/service-name: migration-external-db
    endpointslice.kubernetes.io/managed-by: nodemigrate-test-fixture
addressType: IPv4
endpoints:
- addresses: [192.0.2.20]
  conditions:
    ready: true
ports:
- name: postgres
  port: 5432
  protocol: TCP
---
apiVersion: coordination.k8s.io/v1
kind: Lease
metadata:
  name: migration-application-lock
  namespace: migration-apps
spec:
  holderIdentity: nodemigrate-fixture
  leaseDurationSeconds: 60
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: migration-user-metadata
  namespace: migration-apps
  annotations:
    nodemigrate.io/source-uid: user-value
data:
  migration-marker: user-config-data
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: migration-user-binary
  namespace: migration-apps
binaryData:
  payload.bin: AAECAw==
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: migration-user-immutable
  namespace: migration-apps
immutable: true
data:
  immutable-marker: mounted-immutable-config
---
apiVersion: v1
kind: Secret
metadata:
  name: migration-user-secret
  namespace: migration-apps
type: Opaque
data:
  migration-secret: bWlncmF0aW9uLXNlY3JldC12YWx1ZQ==
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: migration-reader
  namespace: migration-apps
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: migration-config-reader
  namespace: migration-apps
rules:
- apiGroups: [""]
  resources: ["configmaps"]
  resourceNames: ["migration-user-metadata"]
  verbs: ["get"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: migration-node-reader
rules:
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["get"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: migration-node-reader
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: migration-node-reader
subjects:
- kind: ServiceAccount
  name: migration-reader
  namespace: migration-apps
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: migration-config-reader
  namespace: migration-apps
subjects:
- kind: ServiceAccount
  name: migration-reader
  namespace: migration-apps
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: migration-config-reader
---
apiVersion: v1
kind: ResourceQuota
metadata:
  name: migration-quota
  namespace: migration-apps
spec:
  hard:
    pods: "50"
    requests.storage: 20Gi
---
apiVersion: v1
kind: LimitRange
metadata:
  name: migration-limits
  namespace: migration-apps
spec:
  limits:
  - type: Container
    min:
      cpu: 1m
      memory: 1Mi
    max:
      cpu: "4"
      memory: 4Gi
---
apiVersion: scheduling.k8s.io/v1
kind: PriorityClass
metadata:
  name: migration-priority
value: 100000
globalDefault: false
description: "Priority fixture for node migration verification"
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: migration-cron
  namespace: migration-apps
spec:
  schedule: "0 0 1 1 *"
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers:
          - name: check
            image: busybox:1.36.1
            command: ["sh", "-c", "echo migration-cron-ran"]
            resources:
              requests:
                cpu: 1m
                memory: 1Mi
---
apiVersion: batch/v1
kind: Job
metadata:
  name: migration-job
  namespace: migration-apps
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
      - name: check
        image: busybox:1.36.1
        command: ["sh", "-c", "echo migration-job-ran"]
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
---
apiVersion: v1
kind: PodTemplate
metadata:
  name: migration-pod-template
  namespace: migration-apps
template:
  metadata:
    labels:
      app: migration-pod-template
  spec:
    restartPolicy: Never
    containers:
    - name: check
      image: busybox:1.36.1
      command: ["sh", "-c", "echo pod-template-workload-running"]
      resources:
        requests:
          cpu: 1m
          memory: 1Mi
---
apiVersion: v1
kind: ReplicationController
metadata:
  name: migration-replication-controller
  namespace: migration-apps
spec:
  replicas: 1
  selector:
    app: migration-replication-controller
  template:
    metadata:
      labels:
        app: migration-replication-controller
    spec:
      containers:
      - name: check
        image: busybox:1.36.1
        command: ["sh", "-c", "echo replication-controller-workload-running; sleep 36000"]
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: migration-daemon
  namespace: migration-apps
spec:
  selector:
    matchLabels:
      app: migration-daemon
  template:
    metadata:
      labels:
        app: migration-daemon
    spec:
      tolerations:
      - operator: Exists
      containers:
      - name: check
        image: busybox:1.36.1
        command: ["sh", "-c", "sleep 36000"]
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
---
apiVersion: v1
kind: Pod
metadata:
  name: migration-standalone
  namespace: migration-apps
spec:
  restartPolicy: Never
  priorityClassName: migration-priority
  containers:
  - name: app
    image: busybox:1.36.1
    command: [sh, -c, 'echo standalone-workload-running; sleep 36000']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    env:
    - name: MIGRATION_CONFIG
      valueFrom:
        configMapKeyRef:
          name: migration-user-metadata
          key: migration-marker
    - name: MIGRATION_SECRET
      valueFrom:
        secretKeyRef:
          name: migration-user-secret
          key: migration-secret
    volumeMounts:
    - name: immutable-config
      mountPath: /etc/migration/immutable
      readOnly: true
  volumes:
  - name: immutable-config
    configMap:
      name: migration-user-immutable
---
apiVersion: v1
kind: Service
metadata:
  name: migration-stateful
  namespace: migration-apps
spec:
  clusterIP: None
  selector:
    app: migration-stateful
  ports:
  - name: http
    port: 80
    targetPort: 80
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: migration-stateful
  namespace: migration-apps
spec:
  serviceName: migration-stateful
  replicas: 1
  selector:
    matchLabels:
      app: migration-stateful
  template:
    metadata:
      labels:
        app: migration-stateful
    spec:
      containers:
      - name: app
        image: nginx:1.27.5
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        ports:
        - name: http
          containerPort: 80
        volumeMounts:
        - name: state
          mountPath: /state
  volumeClaimTemplates:
  - metadata:
      name: state
    spec:
      accessModes: [ReadWriteOnce]
      storageClassName: csi-hostpath-sc
      resources:
        requests:
          storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-static-pvc
  namespace: migration-apps
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: migration-manual
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-csi-pvc
  namespace: migration-apps
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: csi-hostpath-sc
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: migration-seed-static
  namespace: migration-apps
spec:
  restartPolicy: Never
  tolerations:
  - key: node-role.kubernetes.io/control-plane
    operator: Exists
    effect: NoSchedule
  initContainers:
  - name: seed-static-volume
    image: busybox:1.36.1
    command: [sh, -c, 'echo static-persistent-data > /static/marker']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    volumeMounts:
    - name: static
      mountPath: /static
  containers:
  - name: hold
    image: busybox:1.36.1
    command: [sh, -c, 'sleep 36000']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    volumeMounts:
    - name: static
      mountPath: /static
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
---
apiVersion: v1
kind: Pod
metadata:
  name: migration-seed-csi
  namespace: migration-apps
spec:
  restartPolicy: Never
  tolerations:
  - key: node-role.kubernetes.io/control-plane
    operator: Exists
    effect: NoSchedule
  initContainers:
  - name: seed-csi-volume
    image: busybox:1.36.1
    command: [sh, -c, 'echo csi-persistent-data > /csi/marker']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    volumeMounts:
    - name: csi
      mountPath: /csi
  containers:
  - name: hold
    image: busybox:1.36.1
    command: [sh, -c, 'sleep 36000']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    volumeMounts:
    - name: csi
      mountPath: /csi
  volumes:
  - name: csi
    persistentVolumeClaim:
      claimName: migration-csi-pvc
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  replicas: 1
  selector:
    matchLabels:
      app: migration-nginx
  template:
    metadata:
      labels:
        app: migration-nginx
    spec:
      containers:
      - name: nginx
        image: nginx:1.27.5
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        ports:
        - containerPort: 80
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  minAvailable: 1
  selector:
    matchLabels:
      app: migration-nginx
---
apiVersion: v1
kind: Service
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  selector:
    app: migration-nginx
  ports:
  - port: 80
    targetPort: 80
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: migration-emptydir-nonroot
  namespace: migration-apps
spec:
  replicas: 1
  selector:
    matchLabels:
      app: migration-emptydir-nonroot
  template:
    metadata:
      labels:
        app: migration-emptydir-nonroot
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 1000
        runAsGroup: 1000
      containers:
      - name: write-test
        image: busybox:1.36.1
        command: [sh, -c, 'printf emptydir-write-ok > /tmp/marker; exec sleep 36000']
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        securityContext:
          readOnlyRootFilesystem: true
          allowPrivilegeEscalation: false
        readinessProbe:
          exec:
            command: [sh, -c, 'test "$(cat /tmp/marker)" = emptydir-write-ok']
          periodSeconds: 2
        volumeMounts:
        - name: temporary
          mountPath: /tmp
      volumes:
      - name: temporary
        emptyDir: {}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  ingressClassName: traefik
  rules:
  - host: migration.test
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: migration-nginx
            port:
              number: 80
---
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: migration-traefik
spec:
  controllerName: traefik.io/gateway-controller
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: migration-traefik
  namespace: migration-apps
spec:
  gatewayClassName: migration-traefik
  listeners:
  - name: http
    protocol: HTTP
    port: 8080
    allowedRoutes:
      namespaces:
        from: Same
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: migration-nginx
  namespace: migration-apps
spec:
  parentRefs:
  - name: migration-traefik
  hostnames:
  - migration-gateway.test
  rules:
  - matches:
    - path:
        type: PathPrefix
        value: /
    backendRefs:
    - name: migration-nginx
      port: 80
---
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: migration-selfsigned
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: migration-test
  namespace: migration-apps
spec:
  secretName: migration-test-tls
  issuerRef:
    name: migration-selfsigned
    kind: ClusterIssuer
  dnsNames:
  - migration.test
YAML
    if ! apply_fixture_manifest_with_conflict_retry "$fixture_manifest"; then
        rm -f "$fixture_manifest"
        return 1
    fi
    rm -f "$fixture_manifest"
    kubectl create namespace migration-policy-client --dry-run=client -o yaml | kubectl apply -f -
    kubectl label namespace migration-policy-client \
        nodemigrate.io/policy-client=true --overwrite
    kubectl apply -f - <<'YAML'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: migration-nginx-ingress
  namespace: migration-apps
spec:
  podSelector:
    matchLabels:
      app: migration-nginx
  policyTypes: [Ingress]
  ingress:
  - from:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: traefik
    - namespaceSelector:
        matchLabels:
          nodemigrate.io/policy-client: "true"
    ports:
    - protocol: TCP
      port: 80
YAML
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=10m
    if [[ -n "${NODEMIGRATE_STATIC_NODE:-}" ]]; then
        [[ "$NODEMIGRATE_STATIC_NODE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || {
            echo "invalid static PV node name: $NODEMIGRATE_STATIC_NODE" >&2
            return 1
        }
        local static_node_hostname
        static_node_hostname="$(kubectl get node "$NODEMIGRATE_STATIC_NODE" \
            -o jsonpath='{.metadata.labels.kubernetes\.io/hostname}')"
        [[ -n "$static_node_hostname" ]] || {
            echo "static PV node $NODEMIGRATE_STATIC_NODE has no kubernetes.io/hostname label" >&2
            return 1
        }
        echo "static hostPath PV owner=$NODEMIGRATE_STATIC_NODE hostname=$static_node_hostname"
        kubectl apply -f - <<YAML
apiVersion: v1
kind: PersistentVolume
metadata:
  name: migration-static-pv
spec:
  capacity:
    storage: 1Gi
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: migration-manual
  hostPath:
    path: /var/lib/nodemigrate-ci/static-volume
    type: Directory
  nodeAffinity:
    required:
      nodeSelectorTerms:
      - matchExpressions:
        - key: kubernetes.io/hostname
          operator: In
          values: ["$static_node_hostname"]
YAML
    else
        kubectl apply -f - <<'YAML'
apiVersion: v1
kind: PersistentVolume
metadata:
  name: migration-static-pv
spec:
  capacity:
    storage: 1Gi
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: migration-manual
  hostPath:
    path: /var/lib/nodemigrate-ci/static-volume
    type: Directory
YAML
    fi
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-seed-static --timeout=10m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-seed-csi --timeout=10m
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl rollout status -n migration-apps deployment/migration-emptydir-nonroot --timeout=5m
    kubectl wait --for=condition=Accepted gatewayclasses.gateway.networking.k8s.io/migration-traefik --timeout=2m
    kubectl wait -n migration-apps --for=condition=Programmed gateways.gateway.networking.k8s.io/migration-traefik --timeout=2m
    wait_for_httproute_condition migration-apps migration-nginx Accepted
    wait_for_httproute_condition migration-apps migration-nginx ResolvedRefs
    kubectl get networkpolicy migration-nginx-ingress -n migration-apps -o json | jq -e '
        .spec.podSelector.matchLabels.app == "migration-nginx" and
        .spec.policyTypes == ["Ingress"] and
        ([.spec.ingress[0].from[]?.namespaceSelector.matchLabels | select(."kubernetes.io/metadata.name" == "traefik" or ."nodemigrate.io/policy-client" == "true")] | length) == 2
    ' >/dev/null || {
        echo "NetworkPolicy state changed at stage $stage" >&2
        return 1
    }
    local policy_allow_job="migration-policy-allow-$stage"
    local policy_deny_job="migration-policy-deny-$stage"
    kubectl apply -f - <<YAML
apiVersion: batch/v1
kind: Job
metadata:
  name: $policy_allow_job
  namespace: migration-policy-client
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
      - name: probe
        image: busybox:1.36.1
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        command: [sh, -ec]
        args:
        - >-
          wget -qO- -T 5 http://migration-nginx.migration-apps.svc.cluster.local
          | grep -q "Welcome to nginx!"
YAML
    kubectl wait -n migration-policy-client --for=condition=Complete \
        "job/$policy_allow_job" --timeout=2m
    # The command exits successfully only when wget fetched nginx's page and
    # grep found its welcome text. grep -q suppresses output, so Job completion
    # is the behavior assertion; its logs intentionally contain no page body.
    kubectl apply -f - <<YAML
apiVersion: batch/v1
kind: Job
metadata:
  name: $policy_deny_job
  namespace: migration-apps
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
      - name: probe
        image: busybox:1.36.1
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        command: [sh, -ec]
        args:
        - >-
          nslookup migration-nginx.migration-apps.svc.cluster.local >/dev/null;
          if wget -qO- -T 5 http://migration-nginx.migration-apps.svc.cluster.local;
          then echo network-policy-unexpectedly-allowed; exit 1;
          else echo network-policy-denied; fi
YAML
    kubectl wait -n migration-apps --for=condition=Complete \
        "job/$policy_deny_job" --timeout=2m
    kubectl logs -n migration-apps "job/$policy_deny_job" | grep -q network-policy-denied || {
        echo "NetworkPolicy did not deny the unselected client namespace at stage $stage" >&2
        return 1
    }
    kubectl delete job -n migration-policy-client "$policy_allow_job" --wait=true
    kubectl delete job -n migration-apps "$policy_deny_job" --wait=true
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-standalone --timeout=5m
    # Verify that the migrated ConfigMap and Secret still feed a real container.
    kubectl exec -n migration-apps migration-standalone -- sh -ec \
        'test "$MIGRATION_CONFIG" = user-config-data && test "$MIGRATION_SECRET" = migration-secret-value && test "$(cat /etc/migration/immutable/immutable-marker)" = mounted-immutable-config' || {
        echo "ConfigMap or Secret workload consumption failed at stage $stage" >&2
        return 1
    }
    kubectl get resourcequota migration-quota -n migration-apps -o json | jq -e \
        '.spec.hard.pods == "50" and .spec.hard["requests.storage"] == "20Gi"' >/dev/null || {
        echo "ResourceQuota state changed at stage $stage" >&2
        return 1
    }
    kubectl get limitrange migration-limits -n migration-apps -o json | jq -e \
        '.spec.limits | any(.type == "Container" and .min.cpu == "1m" and .max.cpu == "4")' >/dev/null || {
        echo "LimitRange state changed at stage $stage" >&2
        return 1
    }
    kubectl get priorityclass migration-priority -o json | jq -e \
        '.value == 100000 and ((.globalDefault // false) == false)' >/dev/null || {
        echo "PriorityClass state changed at stage $stage" >&2
        return 1
    }
    kubectl rollout status -n migration-apps statefulset/migration-stateful --timeout=5m
    kubectl exec -n migration-apps migration-stateful-0 -- sh -c \
        'echo stateful-persistent-data > /state/marker'
    kubectl wait -n migration-apps --for=condition=Complete job/migration-job --timeout=5m
    kubectl patch -n migration-apps deployment migration-nginx --type=merge \
        -p '{"spec":{"template":{"metadata":{"annotations":{"migration.nodemigrate/revision":"second"}}}}}'
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl patch -n migration-apps statefulset migration-stateful --type=merge \
        -p '{"spec":{"template":{"metadata":{"annotations":{"migration.nodemigrate/revision":"second"}}}}}'
    kubectl rollout status -n migration-apps statefulset/migration-stateful --timeout=5m
    kubectl get replicasets -n migration-apps -l app=migration-nginx -o json \
        | jq -e '.items | length >= 2' >/dev/null
    kubectl get controllerrevisions.apps -n migration-apps -o json | jq -e '
      [.items[] | select(any(.metadata.ownerReferences[]?;
        .kind == "StatefulSet" and .name == "migration-stateful"))] | length >= 2
    ' >/dev/null
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager --timeout=5m
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager-webhook --timeout=5m
    kubectl wait -n cert-manager --for=condition=Available deployment/cert-manager-cainjector --timeout=5m
    kubectl wait --for=condition=Established crd/certificates.cert-manager.io --timeout=2m
    kubectl wait --for=condition=Established crd/clusterissuers.cert-manager.io --timeout=2m
    kubectl wait -n migration-apps --for=condition=Ready certificate/migration-test --timeout=5m
    kubectl delete pod -n migration-apps migration-seed-static migration-seed-csi --wait=true

    # Exercise a legacy Secret-backed ServiceAccount credential. Kubernetes
    # binds its JWT to the issuing cluster and current ServiceAccount UID, so
    # nodemigrate must reissue its token data at each destination checkpoint.
    kubectl create serviceaccount migration-token-user -n migration-apps
    kubectl create rolebinding migration-token-user-config-reader -n migration-apps \
        --role=migration-config-reader --serviceaccount=migration-apps:migration-token-user
    local legacy_token legacy_ca legacy_account_uid
    legacy_token="$(kubectl create token migration-token-user -n migration-apps --duration=8760h)"
    legacy_ca="$(kubectl get configmap kube-root-ca.crt -n migration-apps -o jsonpath='{.data.ca\.crt}')"
    legacy_account_uid="$(kubectl get serviceaccount migration-token-user -n migration-apps -o jsonpath='{.metadata.uid}')"
    [[ -n "$legacy_token" && -n "$legacy_ca" && -n "$legacy_account_uid" ]] || {
        echo "could not prepare the legacy ServiceAccount token fixture" >&2
        return 1
    }
    kubectl create secret generic migration-legacy-token -n migration-apps \
        --type=kubernetes.io/service-account-token \
        --from-literal=token="$legacy_token" \
        --from-literal=namespace=migration-apps \
        --from-literal=fixture=legacy-secret-data-preserved \
        --from-literal=ca.crt="$legacy_ca" \
        --dry-run=client -o json \
        | jq --arg account migration-token-user --arg uid "$legacy_account_uid" '
            .metadata.annotations = {
              "kubernetes.io/service-account.name": $account,
              "kubernetes.io/service-account.uid": $uid
            }
          ' \
        | kubectl apply -f -
}

wait_for_httproute_condition() {
    local namespace="${1:?missing HTTPRoute namespace}"
    local route="${2:?missing HTTPRoute name}"
    local condition="${3:?missing HTTPRoute condition}"
    local deadline=$((SECONDS + 120))
    while (( SECONDS < deadline )); do
        if kubectl get httproute "$route" -n "$namespace" -o json \
            | jq -e --arg condition "$condition" '
                [.status.parents[]?.conditions[]? | select(.type == $condition and .status == "True")]
                | length > 0
            ' >/dev/null; then
            return 0
        fi
        sleep 2
    done
    kubectl get httproute "$route" -n "$namespace" -o yaml >&2 || true
    echo "HTTPRoute $namespace/$route did not reach condition $condition=True" >&2
    return 1
}

verify_ingress_spec() {
    local stage="${1:?missing Ingress verification stage}"
    local ingress_json ingress_class_json
    ingress_json="$(kubectl get ingress migration-nginx -n migration-apps -o json)"
    ingress_class_json="$(kubectl get ingressclass traefik -o json)"
    jq -e '
      .spec.ingressClassName == "traefik" and
      ([.spec.rules[]? | select(.host == "migration.test") | .http.paths[]? |
        select(.path == "/" and .pathType == "Prefix" and
          .backend.service.name == "migration-nginx" and
          .backend.service.port.number == 80)] | length == 1)
    ' <<<"$ingress_json" >/dev/null || {
        echo "Ingress migration-nginx lost its expected class, host rule, path, or backend at stage $stage" >&2
        jq '{apiVersion, kind, metadata: {name: .metadata.name, namespace: .metadata.namespace}, spec}' \
            <<<"$ingress_json" >&2 || true
        return 1
    }
    jq -e '.spec.controller == "traefik.io/ingress-controller"' \
        <<<"$ingress_class_json" >/dev/null || {
        echo "IngressClass traefik has an unexpected controller at stage $stage" >&2
        jq '{apiVersion, kind, metadata: {name: .metadata.name, annotations: .metadata.annotations}, spec}' \
            <<<"$ingress_class_json" >&2 || true
        return 1
    }
    echo "PASS Ingress spec and IngressClass controller at stage=$stage"
}

verify_system_addon_rollouts() {
    local name
    for name in coredns local-path-provisioner metrics-server; do
        if ! kubectl get deployment "$name" -n kube-system >/dev/null 2>&1; then
            continue
        fi
        kubectl rollout status "deployment/$name" -n kube-system --timeout=5m
        kubectl get replicasets -n kube-system -o json | jq -e --arg owner "$name" '
          any(.items[];
            any(.metadata.ownerReferences[]?;
              .kind == "Deployment" and .name == $owner and .controller == true
            )
          )
        ' >/dev/null || {
            echo "system add-on Deployment $name has no current ReplicaSet" >&2
            return 1
        }
    done
}

verify_csi_node_registration() {
    local plugin_json selector plugin_pods node_names node node_json csinode_json deadline
    local claim pvc_json pv_name pv_json required_topology_nodes required_node
    local -a topology_values=()
    for claim in state-migration-stateful-0 migration-csi-pvc; do
        pvc_json="$(kubectl get pvc "$claim" -n migration-apps -o json)"
        pv_name="$(jq -er '.spec.volumeName | select(length > 0)' <<<"$pvc_json")"
        pv_json="$(kubectl get pv "$pv_name" -o json)"
        jq -e '.spec.csi.driver == "hostpath.csi.k8s.io" and (.spec.csi.volumeHandle | length > 0)' \
            <<<"$pv_json" >/dev/null || {
                echo "fixture PV $pv_name for PVC $claim is not backed by the expected hostpath CSI handle" >&2
                return 1
            }
        while IFS= read -r required_node; do
            [[ -n "$required_node" ]] && topology_values+=("$required_node")
        done < <(jq -r '
          [.spec.nodeAffinity.required.nodeSelectorTerms[]?.matchExpressions[]?
           | select(.key == "topology.hostpath.csi/node" and .operator == "In")
           | .values[]?] | unique[]
        ' <<<"$pv_json")
    done
    required_topology_nodes="$(printf '%s\n' "${topology_values[@]}" | LC_ALL=C sort -u)"
    [[ -n "$required_topology_nodes" ]] || {
        echo "fixture CSI PVs do not declare their required topology node" >&2
        return 1
    }
    plugin_json="$(kubectl get statefulset csi-hostpathplugin -n default -o json)"
    selector="$(jq -er '
      .spec.selector.matchLabels
      | to_entries
      | map("\(.key)=\(.value)")
      | join(",")
      | select(length > 0)
    ' <<<"$plugin_json")"
    plugin_pods="$(kubectl get pods -n default -l "$selector" -o json)"
    node_names="$(jq -r '
      [.items[]
       | select(.status.phase == "Running")
       | select((.status.containerStatuses // []) | length > 0)
       | select(all(.status.containerStatuses[]; .ready == true))
       | .spec.nodeName]
      | unique
      | .[]
    ' <<<"$plugin_pods")"
    [[ -n "$node_names" ]] || {
        echo "hostpath CSI StatefulSet has no Ready plugin Pod on any Node" >&2
        kubectl get pods -n default -l "$selector" -o wide >&2 || true
        return 1
    }
    while IFS= read -r required_node; do
        [[ -n "$required_node" ]] || continue
        grep -Fxq "$required_node" <<<"$node_names" || {
            echo "no Ready hostpath CSI plugin runs on PV-required node $required_node" >&2
            kubectl get pods -n default -l "$selector" -o wide >&2 || true
            return 1
        }
    done <<<"$required_topology_nodes"
    while IFS= read -r node; do
        [[ -n "$node" ]] || continue
        node_json="$(kubectl get node "$node" -o json)"
        deadline=$((SECONDS + 120))
        while true; do
            if csinode_json="$(kubectl get csinode "$node" -o json 2>/dev/null)" \
                && jq -e --arg node "$node" --arg uid "$(jq -r '.metadata.uid' <<<"$node_json")" '
                  any(.spec.drivers[]?; .name == "hostpath.csi.k8s.io") and
                  any(.metadata.ownerReferences[]?;
                    .kind == "Node" and .name == $node and .uid == $uid
                  )
                ' <<<"$csinode_json" >/dev/null; then
                break
            fi
            if (( SECONDS >= deadline )); then
                echo "hostpath CSI driver did not register a CSINode owned by Node $node" >&2
                kubectl get csinode "$node" -o yaml >&2 || true
                return 1
            fi
            sleep 2
        done
    done <<<"$node_names"
    echo "PASS: hostpath CSI registered on every Node running a plugin Pod and regenerated its CSINode owner reference"
}

capture_helm_release_state() {
    local output="${1:?missing Helm state output path}"
    local releases_json release name namespace chart app_version revision status
    local values_digest manifest_digest
    releases_json="$(helm list --all-namespaces --output json)"
    : > "$output"
    while IFS=$'\t' read -r name namespace chart app_version revision status; do
        [[ -n "$name" && -n "$namespace" ]] || continue
        values_digest="$(helm get values "$name" -n "$namespace" --all -o json \
            | jq -S -c . | sha256sum | awk '{print $1}')"
        manifest_digest="$(helm get manifest "$name" -n "$namespace" \
            | sha256sum | awk '{print $1}')"
        jq -cS -n \
            --arg name "$name" \
            --arg namespace "$namespace" \
            --arg chart "$chart" \
            --arg appVersion "$app_version" \
            --arg revision "$revision" \
            --arg status "$status" \
            --arg valuesSha256 "$values_digest" \
            --arg manifestSha256 "$manifest_digest" \
            '{name:$name, namespace:$namespace, chart:$chart,
              appVersion:$appVersion, revision:$revision, status:$status,
              valuesSha256:$valuesSha256, manifestSha256:$manifestSha256}' \
            >> "$output"
    done < <(jq -r '.[] | [.name, .namespace, .chart, .app_version,
        (.revision | tostring), .status] | @tsv' <<< "$releases_json")
    LC_ALL=C sort -o "$output" "$output"
}

record_fixture_storage_specs() {
    local stage="$1" claim pvc_json pv_name
    for claim in state-migration-stateful-0 migration-csi-pvc migration-static-pvc; do
        pvc_json="$(kubectl get pvc "$claim" -n migration-apps -o json 2>/dev/null)" || continue
        echo "Storage fixture PVC spec at stage=$stage: migration-apps/$claim"
        jq '{metadata: {name: .metadata.name, uid: .metadata.uid, ownerReferences: .metadata.ownerReferences}, spec: .spec, status: .status}' <<<"$pvc_json"
        pv_name="$(jq -r '.spec.volumeName // empty' <<<"$pvc_json")"
        if [[ -n "$pv_name" ]]; then
            echo "Storage fixture PV spec at stage=$stage: $pv_name"
            kubectl get pv "$pv_name" -o yaml || true
        fi
    done
}

exercise_statefulset_scaling() {
    local stage="$1"
    local claim_uid claim_uid_after ordinal_one_pv ordinal_one_claim_uid original_min_ready_seconds
    claim_uid="$(kubectl get pvc state-migration-stateful-0 -n migration-apps \
        -o jsonpath='{.metadata.uid}')"
    [[ -n "$claim_uid" ]] || {
        echo "StatefulSet ordinal 0 PVC has no UID at stage=$stage" >&2
        return 1
    }
    original_min_ready_seconds="$(kubectl get statefulset migration-stateful -n migration-apps \
        -o json | jq -r '.spec.minReadySeconds // 0')"

    kubectl patch statefulset migration-stateful -n migration-apps --type=merge \
        -p '{"spec":{"minReadySeconds":1}}'
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl get statefulset migration-stateful -n migration-apps -o json | jq -e '
      .spec.minReadySeconds == 1 and
      (.status.observedGeneration // 0) == .metadata.generation and
      (.status.readyReplicas // 0) == 1
    ' >/dev/null || {
        echo "StatefulSet controller did not reconcile the spec update at stage=$stage" >&2
        return 1
    }
    kubectl patch statefulset migration-stateful -n migration-apps --type=merge \
        -p "{\"spec\":{\"minReadySeconds\":$original_min_ready_seconds}}"
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl get statefulset migration-stateful -n migration-apps -o json | jq -e \
        --argjson minReadySeconds "$original_min_ready_seconds" '
          (.spec.minReadySeconds // 0) == $minReadySeconds and
          (.status.observedGeneration // 0) == .metadata.generation
        ' >/dev/null || {
            echo "StatefulSet did not return to its original spec after the update probe at stage=$stage" >&2
            return 1
        }

    kubectl scale statefulset/migration-stateful -n migration-apps --replicas=2
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-stateful-1 --timeout=5m
    kubectl wait -n migration-apps \
        --for=jsonpath='{.status.phase}'=Bound pvc/state-migration-stateful-1 --timeout=5m
    kubectl get statefulset migration-stateful -n migration-apps -o json | jq -e '
      .spec.replicas == 2 and
      (.status.readyReplicas // 0) == 2 and
      (.status.observedGeneration // 0) == .metadata.generation
    ' >/dev/null || {
        echo "StatefulSet did not reconcile both replicas at stage=$stage" >&2
        return 1
    }
    kubectl exec -n migration-apps migration-stateful-1 -- sh -c \
        'echo ordinal-one-transient-data > /state/marker'
    [[ "$(kubectl exec -n migration-apps migration-stateful-1 -- cat /state/marker)" == ordinal-one-transient-data ]] || {
        echo "StatefulSet ordinal 1 could not read its own claim-template data at stage=$stage" >&2
        return 1
    }
    ordinal_one_pv="$(kubectl get pvc state-migration-stateful-1 -n migration-apps \
        -o jsonpath='{.spec.volumeName}')"
    ordinal_one_claim_uid="$(kubectl get pvc state-migration-stateful-1 -n migration-apps \
        -o jsonpath='{.metadata.uid}')"
    [[ -n "$ordinal_one_pv" && -n "$ordinal_one_claim_uid" ]] || {
        echo "StatefulSet ordinal 1 PVC has no stable bound PV identity at stage=$stage" >&2
        return 1
    }

    # Scaling a StatefulSet down removes the Pod, but its claim-template PVC
    # and PV are durable workload data. Keep both through migration; deleting
    # this PVC would trigger the hostpath CSI PV's Delete reclaim policy.
    kubectl scale statefulset/migration-stateful -n migration-apps --replicas=1
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl wait -n migration-apps --for=delete pod/migration-stateful-1 --timeout=5m
    [[ "$(kubectl get pvc state-migration-stateful-1 -n migration-apps -o jsonpath='{.metadata.uid}')" == "$ordinal_one_claim_uid" ]] || {
        echo "StatefulSet scale-down changed ordinal 1 PVC identity at stage=$stage" >&2
        return 1
    }
    [[ "$(kubectl get pvc state-migration-stateful-1 -n migration-apps -o jsonpath='{.spec.volumeName}')" == "$ordinal_one_pv" ]] || {
        echo "StatefulSet scale-down changed ordinal 1 PV binding at stage=$stage" >&2
        return 1
    }
    kubectl get pv "$ordinal_one_pv" >/dev/null || {
        echo "StatefulSet scale-down removed ordinal 1 PV $ordinal_one_pv at stage=$stage" >&2
        return 1
    }

    kubectl scale statefulset/migration-stateful -n migration-apps --replicas=2
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-stateful-1 --timeout=5m
    [[ "$(kubectl exec -n migration-apps migration-stateful-1 -- cat /state/marker)" == ordinal-one-transient-data ]] || {
        echo "StatefulSet ordinal 1 data changed after scale-down/up at stage=$stage" >&2
        return 1
    }
    kubectl scale statefulset/migration-stateful -n migration-apps --replicas=1
    kubectl rollout status statefulset/migration-stateful -n migration-apps --timeout=5m
    kubectl wait -n migration-apps --for=delete pod/migration-stateful-1 --timeout=5m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-stateful-0 --timeout=5m
    claim_uid_after="$(kubectl get pvc state-migration-stateful-0 -n migration-apps \
        -o jsonpath='{.metadata.uid}')"
    [[ "$claim_uid_after" == "$claim_uid" ]] || {
        echo "StatefulSet ordinal 0 PVC identity changed during scale at stage=$stage" >&2
        return 1
    }
    [[ "$(kubectl exec -n migration-apps migration-stateful-0 -- cat /state/marker)" == stateful-persistent-data ]] || {
        echo "StatefulSet ordinal 0 data changed during scale at stage=$stage" >&2
        return 1
    }
    echo "PASS StatefulSet scale preserves ordinal 0/1 PVCs, PV bindings and data at stage=$stage"
}

verify_legacy_service_account_token() {
    local stage="$1"
    local stage_dir="$CHECKPOINT_DIR/$stage"
    local secret token account_uid secret_uid kubeconfig_path error
    secret="$(kubectl get secret migration-legacy-token -n migration-apps -o json)"
    token="$(jq -r '.data.token | @base64d' <<<"$secret")"
    local fixture_data
    fixture_data="$(jq -r '.data.fixture | @base64d' <<<"$secret")"
    account_uid="$(kubectl get serviceaccount migration-token-user -n migration-apps -o jsonpath='{.metadata.uid}')"
    secret_uid="$(jq -r '.metadata.annotations["kubernetes.io/service-account.uid"] // empty' <<<"$secret")"
    [[ -n "$token" && "$fixture_data" == legacy-secret-data-preserved \
        && -n "$account_uid" && "$secret_uid" == "$account_uid" ]] || {
        echo "legacy ServiceAccount token Secret identity is stale at stage $stage" >&2
        return 1
    }
    kubeconfig_path="$stage_dir/legacy-token.kubeconfig"
    trap 'rm -f "$kubeconfig_path"' RETURN
    kubectl config view --raw --flatten --minify -o json \
        | jq --arg token "$token" '
            .users = [{name: "migration-token-user", user: {token: $token}}]
            | .contexts[0].context.user = "migration-token-user"
          ' > "$kubeconfig_path"
    chmod 0600 "$kubeconfig_path"
    if ! KUBECONFIG="$kubeconfig_path" kubectl get configmap migration-user-metadata \
        -n migration-apps -o name >/dev/null; then
        echo "legacy ServiceAccount token could not read its authorized ConfigMap at stage $stage" >&2
        return 1
    fi
    if error="$(KUBECONFIG="$kubeconfig_path" kubectl get secret migration-user-secret \
        -n migration-apps -o name 2>&1)"; then
        echo "legacy ServiceAccount token unexpectedly read a forbidden Secret at stage $stage" >&2
        return 1
    fi
    trap - RETURN
    rm -f "$kubeconfig_path"
    grep -q 'Forbidden' <<<"$error" || {
        echo "legacy ServiceAccount token denial was not an RBAC Forbidden at stage $stage: $error" >&2
        return 1
    }
    echo "PASS legacy ServiceAccount token identity and RBAC at stage=$stage"
}

verify_authentication_and_authorization_reviews() {
    local stage="$1"
    local token review response
    token="$(kubectl create token migration-reader -n migration-apps --duration=10m)" || {
        echo "could not mint a short-lived migration-reader token at stage $stage" >&2
        return 1
    }
    [[ -n "$token" ]] || {
        echo "TokenRequest returned an empty token at stage $stage" >&2
        return 1
    }
    review="$(jq -cn --arg token "$token" \
        '{apiVersion:"authentication.k8s.io/v1",kind:"TokenReview",spec:{token:$token}}')"
    response="$(printf '%s\n' "$review" \
        | kubectl create --raw=/apis/authentication.k8s.io/v1/tokenreviews -f -)" || {
        echo "TokenReview API request failed at stage $stage" >&2
        return 1
    }
    jq -e '
      .status.authenticated == true and
      .status.user.username == "system:serviceaccount:migration-apps:migration-reader" and
      ((.status.user.groups // []) | index("system:serviceaccounts:migration-apps") != null)
    ' <<<"$response" >/dev/null || {
        echo "TokenReview did not authenticate the migration-reader identity at stage $stage" >&2
        jq '{status: .status}' <<<"$response" >&2
        return 1
    }

    local allowed_review denied_review
    allowed_review="$(jq -cn '
      {apiVersion:"authorization.k8s.io/v1",kind:"SubjectAccessReview",spec:{
        user:"system:serviceaccount:migration-apps:migration-reader",
        groups:["system:serviceaccounts","system:serviceaccounts:migration-apps","system:authenticated"],
        resourceAttributes:{namespace:"migration-apps",verb:"get",group:"",resource:"configmaps",name:"migration-user-metadata"}
      }}')"
    denied_review="$(jq -cn '
      {apiVersion:"authorization.k8s.io/v1",kind:"SubjectAccessReview",spec:{
        user:"system:serviceaccount:migration-apps:migration-reader",
        groups:["system:serviceaccounts","system:serviceaccounts:migration-apps","system:authenticated"],
        resourceAttributes:{namespace:"migration-apps",verb:"get",group:"",resource:"secrets",name:"migration-user-secret"}
      }}')"
    response="$(printf '%s\n' "$allowed_review" \
        | kubectl create --raw=/apis/authorization.k8s.io/v1/subjectaccessreviews -f -)" || {
        echo "allowed SubjectAccessReview request failed at stage $stage" >&2
        return 1
    }
    jq -e '.status.allowed == true and .status.denied != true' <<<"$response" >/dev/null || {
        echo "SubjectAccessReview denied migration-reader's named ConfigMap access at stage $stage" >&2
        jq '{status: .status}' <<<"$response" >&2
        return 1
    }
    response="$(printf '%s\n' "$denied_review" \
        | kubectl create --raw=/apis/authorization.k8s.io/v1/subjectaccessreviews -f -)" || {
        echo "denied SubjectAccessReview request failed at stage $stage" >&2
        return 1
    }
    jq -e '.status.allowed == false and .status.denied != true' <<<"$response" >/dev/null || {
        echo "SubjectAccessReview did not deny migration-reader's Secret access at stage $stage" >&2
        jq '{status: .status}' <<<"$response" >&2
        return 1
    }
    echo "PASS TokenReview and allowed/denied SubjectAccessReview behavior at stage=$stage"
}

verify_custom_resource_status_subresource() {
    local stage="$1"
    local patch
    patch="$(jq -cn --arg stage "$stage" '{status:{migrationStage:$stage}}')"
    kubectl patch migrationrecord migration-record-0 \
        --subresource=status --type=merge -p "$patch" >/dev/null || {
        echo "could not write MigrationRecord /status at stage $stage" >&2
        return 1
    }
    kubectl get --raw \
        '/apis/migration.nodemigrate.io/v1/migrationrecords/migration-record-0/status' \
        | jq -e --arg stage "$stage" '
          .apiVersion == "migration.nodemigrate.io/v1" and
          .spec.marker == "durable-custom-resource-data" and
          .status.migrationStage == $stage
        ' >/dev/null || {
        echo "MigrationRecord /status GET failed or returned changed data at stage $stage" >&2
        return 1
    }
    kubectl get migrationrecord migration-record-0 -o json | jq -e \
        --arg stage "$stage" '
          .apiVersion == "migration.nodemigrate.io/v1" and
          .spec.marker == "durable-custom-resource-data" and
          .status.migrationStage == $stage
        ' >/dev/null || {
        echo "MigrationRecord /status readback failed or changed its durable spec at stage $stage" >&2
        return 1
    }
    echo "PASS CRD /status update/readback at stage=$stage"
}

verify_ephemeral_container_subresource() {
    local stage="$1"
    local pod_name="migration-ephemeral-probe-${stage}"
    local container_name="migration-debug-${stage}"
    local marker="migration-ephemeral-${stage}"
    local patch pod_json marker_output deadline running=false
    kubectl apply -f - <<YAML || return 1
apiVersion: v1
kind: Pod
metadata:
  name: $pod_name
  namespace: migration-apps
  labels:
    app: migration-ephemeral-probe
spec:
  restartPolicy: Never
  containers:
  - name: probe
    image: busybox:1.36.1
    command: ["sh", "-c", "sleep 600"]
    volumeMounts:
    - name: marker
      mountPath: /marker
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
  volumes:
  - name: marker
    emptyDir: {}
YAML
    if ! kubectl wait -n migration-apps --for=condition=Ready "pod/$pod_name" --timeout=5m; then
        echo "ephemeral-container probe Pod did not become Ready at stage $stage" >&2
        kubectl delete pod -n migration-apps "$pod_name" --ignore-not-found --wait=true >/dev/null 2>&1 || true
        return 1
    fi
    patch="$(jq -cn --arg name "$container_name" --arg marker "$marker" '
      {spec:{ephemeralContainers:[{
        name:$name,
        image:"busybox:1.36.1",
        command:["sh","-c",("echo " + $marker + " > /marker/result; sleep 600")],
        volumeMounts:[{name:"marker",mountPath:"/marker"}]
      }]}}
    ')"
    if ! kubectl patch pod "$pod_name" -n migration-apps \
        --subresource=ephemeralcontainers --type=merge -p "$patch" >/dev/null; then
        echo "pods/ephemeralcontainers update failed at stage $stage" >&2
        kubectl delete pod -n migration-apps "$pod_name" --ignore-not-found --wait=true >/dev/null 2>&1 || true
        return 1
    fi
    deadline=$((SECONDS + 120))
    while (( SECONDS < deadline )); do
        pod_json="$(kubectl get pod "$pod_name" -n migration-apps -o json)" || break
        if jq -e --arg container "$container_name" '
          any(.spec.ephemeralContainers[]?; .name == $container) and
          any(.status.ephemeralContainerStatuses[]?;
            .name == $container and (.state.running | type == "object"))
        ' <<<"$pod_json" >/dev/null; then
            running=true
            break
        fi
        sleep 2
    done
    marker_output=""
    if [[ "$running" == true ]]; then
        marker_output="$(kubectl exec "$pod_name" -n migration-apps -c probe -- cat /marker/result 2>&1)" || true
    fi
    kubectl delete pod -n migration-apps "$pod_name" --wait=true >/dev/null || {
        echo "ephemeral-container probe Pod cleanup failed at stage $stage" >&2
        return 1
    }
    [[ "$running" == true && "$marker_output" == *"$marker"* ]] || {
        echo "ephemeral container did not run and write its marker at stage $stage" >&2
        printf '%s\n' "$pod_json" | jq '{spec: .spec.ephemeralContainers, status: .status.ephemeralContainerStatuses}' >&2 || true
        printf '%s\n' "$marker_output" >&2
        return 1
    }
    echo "PASS pods/ephemeralcontainers execution and marker readback at stage=$stage"
}

api_ca_fingerprint() {
    local kubeconfig="${1:?missing kubeconfig}"
    local encoded
    encoded="$(KUBECONFIG="$kubeconfig" kubectl config view --raw --minify -o json \
        | jq -er '.clusters[0].cluster["certificate-authority-data"] // empty')" || return 1
    [[ -n "$encoded" ]] || return 1
    printf '%s' "$encoded" | base64 -d | sha256sum | awk '{print $1}'
}

verify_api_ca_continuity() {
    local stage="$1"
    local kubeconfig="$2"
    local baseline="$CHECKPOINT_DIR/source/apiserver-ca.sha256"
    local actual
    actual="$(api_ca_fingerprint "$kubeconfig")" || {
        echo "could not read API CA from kubeconfig at stage=$stage" >&2
        return 1
    }
    if [[ "$stage" == source ]]; then
        printf '%s\n' "$actual" > "$baseline"
        chmod 0600 "$baseline"
        echo "Recorded source API CA fingerprint=$actual"
        return 0
    fi
    local expected
    expected="$(cat "$baseline")" || return 1
    [[ "$actual" == "$expected" ]] || {
        echo "API CA changed at stage=$stage (source=$expected destination=$actual)" >&2
        return 1
    }
    echo "PASS source API CA continuity at stage=$stage"
}

verify_stage() {
    local stage="$1"
    local stage_dir="$CHECKPOINT_DIR/$stage"
    mkdir -p "$stage_dir"
    chmod 0700 "$CHECKPOINT_DIR" "$stage_dir"
    CURRENT_KUBECONFIG="$2"
    export KUBECONFIG="$CURRENT_KUBECONFIG"
    echo "Verifying stage=$stage distro=$SOURCE_DIST kubeconfig=$CURRENT_KUBECONFIG"
    verify_api_ca_continuity "$stage" "$CURRENT_KUBECONFIG"
    if [[ "$stage" == nodestore ]]; then
        local kube_proxy_daemonset=false nodeproxy_active=false
        if kubectl get daemonset kube-proxy -n kube-system >/dev/null 2>&1; then
            kube_proxy_daemonset=true
        fi
        if systemctl is-active --quiet nodeproxy; then
            nodeproxy_active=true
        fi
        if [[ "$kube_proxy_daemonset" == true && "$nodeproxy_active" == true ]]; then
            echo "kube-proxy DaemonSet and nodeproxy are competing Service datapaths" >&2
            return 1
        fi
        if [[ "${NODEMIGRATE_CILIUM_KPR:-false}" == true ]]; then
            [[ "$kube_proxy_daemonset" == false && "$nodeproxy_active" == false ]] || {
                echo "Cilium KPR owns Service routing, but another proxy is installed" >&2
                return 1
            }
            echo "PASS Cilium eBPF owns Service routing without kube-proxy or nodeproxy"
        elif [[ "$kube_proxy_daemonset" == true ]]; then
            echo "PASS kube-proxy DaemonSet owns Service routing without nodeproxy"
        else
            [[ "$nodeproxy_active" == true ]] || {
                echo "neither kube-proxy, Cilium KPR, nor nodeproxy owns Service routing" >&2
                return 1
            }
            echo "PASS nodeproxy owns Service routing without kube-proxy"
        fi
    fi
    capture_cilium_agent_cri_security "$CURRENT_KUBECONFIG"
    verify_ingress_spec "$stage"
    local expected_ca_b64 expected_namespace_count deadline trust_bundles
    expected_ca_b64="$(kubectl config view --raw --flatten --minify -o json \
        | jq -r '.clusters[0].cluster["certificate-authority-data"] // empty')"
    [[ -n "$expected_ca_b64" ]] || {
        echo "active kubeconfig has no certificate authority data at stage $stage" >&2
        return 1
    }
    expected_namespace_count="$(kubectl get namespaces -o json \
        | jq '[.items[] | select(.status.phase != "Terminating")] | length')"
    deadline=$((SECONDS + 120))
    trust_bundles=""
    while (( SECONDS < deadline )); do
        if kubectl get configmaps -A -o json | jq -e \
            --arg ca "$expected_ca_b64" --argjson count "$expected_namespace_count" '
              [.items[] | select(.metadata.name == "kube-root-ca.crt")] as $bundles
              | ($bundles | length) == $count
                and all($bundles[]; ((.data["ca.crt"] // "") | @base64) == $ca)
            ' >/dev/null 2>&1; then
            trust_bundles="match"
            break
        fi
        sleep 2
    done
    if [[ "$trust_bundles" != match ]]; then
        echo "namespace kube-root-ca.crt bundles do not match the active API CA at stage $stage" >&2
        kubectl get configmaps -A -o json | jq -cS --arg ca "$expected_ca_b64" '
          [.items[] | select(.metadata.name == "kube-root-ca.crt") | {
            namespace: .metadata.namespace,
            matchesTargetApiCa: ((.data["ca.crt"] // "" | @base64) == $ca)
          }]
        ' >&2 || true
        return 1
    fi
    echo "PASS: every namespace trust bundle matches the active API CA at stage $stage"
    if ! kubectl wait --for=condition=Ready node --all --timeout=5m; then
        echo "Node readiness failed at stage=$stage; collecting replacement and lease diagnostics" >&2
        kubectl get nodes -o wide >&2 || true
        kubectl get nodes -o yaml >&2 || true
        kubectl get leases -n kube-node-lease -o yaml >&2 || true
        kubectl get events -A --field-selector involvedObject.kind=Node \
            --sort-by=.metadata.creationTimestamp >&2 || true
        for service in k3s kubelet; do
            systemctl status "$service" --no-pager --full >&2 || true
            journalctl -u "$service" -b --no-pager -n 400 >&2 || true
        done
        return 1
    fi
    if ! kubectl rollout status daemonset/cilium -n kube-system --timeout=5m; then
        echo "Cilium did not become Ready before workload checks at stage $stage" >&2
        kubectl get daemonset cilium -n kube-system -o wide >&2 || true
        kubectl get pods -n kube-system -l k8s-app=cilium -o wide >&2 || true
        return 1
    fi
    if ! kubectl rollout status deployment/coredns -n kube-system --timeout=5m; then
        echo "CoreDNS did not become Ready before workload checks at stage $stage" >&2
        kubectl get deployment coredns -n kube-system -o wide >&2 || true
        kubectl get pods -n kube-system -l k8s-app=kube-dns -o wide >&2 || true
        return 1
    fi
    probe_api_clusterip_from_pod "$CURRENT_KUBECONFIG" "$stage"
    verify_csi_node_registration
    if [[ -n "${NODEMIGRATE_EXPECTED_NODES:-}" ]]; then
        local expected_nodes actual_nodes
        expected_nodes="$(tr ',' '\n' <<<"$NODEMIGRATE_EXPECTED_NODES" | LC_ALL=C sort | paste -sd, -)"
        actual_nodes="$(kubectl get nodes -o json | jq -r '[.items[].metadata.name] | sort | join(",")')"
        [[ "$actual_nodes" == "$expected_nodes" ]] || {
            echo "node membership differs at stage $stage: expected=$expected_nodes actual=$actual_nodes" >&2
            return 1
        }
        local expected_control_planes expected_workers actual_control_planes actual_workers
        expected_control_planes="${NODEMIGRATE_EXPECTED_CONTROL_PLANES:-3}"
        expected_workers="${NODEMIGRATE_EXPECTED_WORKERS:-2}"
        read -r actual_control_planes actual_workers < <(kubectl get nodes -o json | jq -r '
          [
            ([.items[] | select((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master")))] | length),
            ([.items[] | select(((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master"))) | not)] | length)
          ] | @tsv
        ')
        [[ "$actual_control_planes" == "$expected_control_planes" && "$actual_workers" == "$expected_workers" ]] || {
            echo "node roles differ at stage $stage: expected control-planes=$expected_control_planes workers=$expected_workers actual control-planes=$actual_control_planes workers=$actual_workers" >&2
            return 1
        }
    fi
    kubectl get nodes -o json | jq -e '
      all(.items[];
        .metadata.labels["operator.example/pool"] == "blue" and
        .metadata.annotations["nodemigrate.io/source-uid"] == "operator-node-value" and
        any(.spec.taints[]?; .key == "operator.example/dedicated" and .value == "migration" and .effect == "PreferNoSchedule")
      )
    ' >/dev/null || {
        echo "nodemigrate changed user Node labels, annotations, or taints at stage $stage" >&2
        return 1
    }
    kubectl rollout status -n migration-apps deployment/migration-emptydir-nonroot --timeout=5m
    local emptydir_pod marker_output marker_ready=false
    for _ in $(seq 1 30); do
        emptydir_pod="$(kubectl get pods -n migration-apps -l app=migration-emptydir-nonroot \
            -o json | jq -r '
              [.items[] | select(.metadata.deletionTimestamp == null and
                .status.phase == "Running" and
                any(.status.conditions[]?; .type == "Ready" and .status == "True") and
                ((.status.containerStatuses // []) | length) > 0 and
                all(.status.containerStatuses[];
                  .ready == true and (.state.running | type == "object") and
                  ((.containerID // "") | length) > 0))]
              | sort_by(.metadata.creationTimestamp) | last.metadata.name // empty
            ' )"
        if [[ -n "$emptydir_pod" ]]; then
            if marker_output="$(kubectl exec -n migration-apps "$emptydir_pod" -- cat /tmp/marker 2>&1)"; then
                [[ "$marker_output" == emptydir-write-ok ]] || {
                    echo "non-root emptyDir marker has unexpected contents at stage $stage in Pod $emptydir_pod: $marker_output" >&2
                    return 1
                }
                marker_ready=true
                break
            fi
            case "$marker_output" in
                *"CONTAINER_EXITED"*|*"unable to upgrade connection"*|*"container not found"*)
                    echo "emptyDir probe Pod $emptydir_pod changed during runtime exec at stage $stage; retrying after Pod status refresh" >&2
                    ;;
                *)
                    echo "non-root emptyDir exec failed at stage $stage in Pod $emptydir_pod: $marker_output" >&2
                    return 1
                    ;;
            esac
        fi
        sleep 2
    done
    [[ "$marker_ready" == true ]] || {
        echo "no stable Ready non-root emptyDir probe Pod could be read at stage $stage" >&2
        kubectl get pods -n migration-apps -l app=migration-emptydir-nonroot -o wide >&2 || true
        return 1
    }
    echo "PASS non-root read-only-rootfs emptyDir write at stage=$stage"
    local nginx_pod eviction_response pdb_deadline
    pdb_deadline=$((SECONDS + 120))
    while (( SECONDS < pdb_deadline )); do
        if kubectl get pods -n migration-apps -l app=migration-nginx -o json 2>/dev/null | jq -e '
          [.items[] | select(.metadata.deletionTimestamp == null and
            .status.phase == "Running" and
            any(.status.conditions[]?; .type == "Ready" and .status == "True"))] | length == 1
        ' >/dev/null \
            && kubectl get poddisruptionbudget migration-nginx -n migration-apps -o json 2>/dev/null | jq -e '
          .spec.minAvailable == 1 and
          .spec.selector.matchLabels.app == "migration-nginx" and
          (.status.currentHealthy // 0) == 1 and
          (.status.desiredHealthy // 0) == 1 and
          (.status.disruptionsAllowed // 0) == 0
        ' >/dev/null; then
            break
        fi
        sleep 2
    done
    kubectl get poddisruptionbudget migration-nginx -n migration-apps -o json | jq -e '
      .spec.minAvailable == 1 and
      .spec.selector.matchLabels.app == "migration-nginx" and
      (.status.currentHealthy // 0) == 1 and
      (.status.desiredHealthy // 0) == 1 and
      (.status.disruptionsAllowed // 0) == 0
    ' >/dev/null || {
        echo "PodDisruptionBudget did not protect its healthy nginx pod at stage $stage" >&2
        kubectl describe poddisruptionbudget migration-nginx -n migration-apps >&2 || true
        return 1
    }
    kubectl get pods -n migration-apps -l app=migration-nginx -o json | jq -e '
      [.items[] | select(.metadata.deletionTimestamp == null and
        .status.phase == "Running" and
        any(.status.conditions[]?; .type == "Ready" and .status == "True"))] | length == 1
    ' >/dev/null || {
        echo "nginx must have exactly one non-terminating Ready Pod for the minAvailable=1 eviction check at stage $stage" >&2
        kubectl get pods -n migration-apps -l app=migration-nginx -o wide >&2 || true
        return 1
    }
    nginx_pod="$(kubectl get pods -n migration-apps -l app=migration-nginx -o json \
        | jq -r '[.items[] | select(.metadata.deletionTimestamp == null and
            .status.phase == "Running" and
            any(.status.conditions[]?; .type == "Ready" and .status == "True"))
            | .metadata.name][0] // empty')"
    [[ -n "$nginx_pod" ]] || {
        echo "No running nginx pod is available for the eviction subresource check at stage $stage" >&2
        return 1
    }
    eviction_request="$(jq -cn --arg name "$nginx_pod" \
        '{apiVersion:"policy/v1",kind:"Eviction",metadata:{name:$name,namespace:"migration-apps"}}')"
    if eviction_response="$(printf '%s\n' "$eviction_request" \
        | kubectl create --raw "/api/v1/namespaces/migration-apps/pods/$nginx_pod/eviction" -f - 2>&1)"; then
        echo "Pod eviction unexpectedly succeeded despite minAvailable=1 at stage $stage: $eviction_response" >&2
        echo "PDB state at failed eviction:" >&2
        kubectl get poddisruptionbudget migration-nginx -n migration-apps -o yaml >&2 || true
        echo "Selected Pod state at failed eviction: $nginx_pod" >&2
        kubectl get pod "$nginx_pod" -n migration-apps -o yaml >&2 || true
        echo "All matching Pods at failed eviction:" >&2
        kubectl get pods -n migration-apps -l app=migration-nginx -o wide >&2 || true
        return 1
    fi
    grep -Eiq 'too[[:space:]]*many[[:space:]]*requests|429' <<< "$eviction_response" || {
        echo "Eviction did not fail with the PDB-protected 429 response at stage $stage: $eviction_response" >&2
        return 1
    }
    kubectl wait -n migration-apps --for=condition=Ready "pod/$nginx_pod" --timeout=2m
    kubectl rollout status daemonset/cilium -n kube-system --timeout=5m
    verify_system_addon_rollouts
    kubectl rollout status -n migration-apps daemonset/migration-daemon --timeout=5m
    local expected_daemon_nodes actual_daemon_nodes
    expected_daemon_nodes="$(kubectl get nodes -o json | jq '.items | length')"
    actual_daemon_nodes="$(kubectl get daemonset migration-daemon -n migration-apps -o json \
        | jq -r '[.status.desiredNumberScheduled, .status.numberReady] | @tsv')"
    [[ "$actual_daemon_nodes" == "$expected_daemon_nodes"$'\t'"$expected_daemon_nodes" ]] || {
        echo "DaemonSet migration-daemon is not ready on every node at stage $stage: expected=$expected_daemon_nodes/$expected_daemon_nodes actual=$actual_daemon_nodes" >&2
        return 1
    }
    kubectl get podtemplate migration-pod-template -n migration-apps -o json | jq -e '
      .template.metadata.labels.app == "migration-pod-template" and
      .template.spec.restartPolicy == "Never" and
      .template.spec.containers[0].image == "busybox:1.36.1" and
      .template.spec.containers[0].command == ["sh", "-c", "echo pod-template-workload-running"]
    ' >/dev/null || {
        echo "PodTemplate workload data changed at stage $stage" >&2
        return 1
    }
    if ! kubectl wait -n migration-apps \
        --for=jsonpath='{.status.readyReplicas}'=1 \
        replicationcontroller/migration-replication-controller --timeout=5m; then
        echo "ReplicationController readiness timed out at stage $stage; collecting controller and Pod state" >&2
        kubectl get replicationcontroller migration-replication-controller -n migration-apps -o yaml >&2 || true
        kubectl get pods -n migration-apps -l app=migration-replication-controller -o wide >&2 || true
        kubectl get pods -n migration-apps -l app=migration-replication-controller -o yaml >&2 || true
        kubectl get events -n migration-apps --sort-by=.lastTimestamp | tail -n 40 >&2 || true
        return 1
    fi
    kubectl get replicationcontroller migration-replication-controller -n migration-apps -o json | jq -e '
      .spec.replicas == 1 and
      .spec.selector.app == "migration-replication-controller" and
      (.status.readyReplicas // 0) == 1
    ' >/dev/null || {
        echo "ReplicationController did not preserve or reconcile its single replica at stage $stage" >&2
        kubectl get replicationcontroller migration-replication-controller -n migration-apps -o yaml >&2 || true
        kubectl get pods -n migration-apps -l app=migration-replication-controller -o wide >&2 || true
        return 1
    }
    local replication_controller_pod
    replication_controller_pod="$(kubectl get pods -n migration-apps -l app=migration-replication-controller -o json | jq -r '
      [.items[] | select(.metadata.deletionTimestamp == null and
        .status.phase == "Running" and
        any(.status.conditions[]?; .type == "Ready" and .status == "True") and
        any(.metadata.ownerReferences[]?;
          .kind == "ReplicationController" and .name == "migration-replication-controller"))]
      | .[0].metadata.name // empty
    ')"
    [[ -n "$replication_controller_pod" ]] || {
        echo "ReplicationController-owned workload is not Ready at stage $stage" >&2
        kubectl get pods -n migration-apps -l app=migration-replication-controller -o yaml >&2 || true
        return 1
    }
    kubectl get pods -n migration-apps -l app=migration-replication-controller -o json | jq -e '
      [.items[] | select(.metadata.deletionTimestamp == null and
        .status.phase == "Running" and
        any(.status.conditions[]?; .type == "Ready" and .status == "True") and
        any(.metadata.ownerReferences[]?;
          .kind == "ReplicationController" and .name == "migration-replication-controller"))]
      | length == 1
    ' >/dev/null || {
        echo "ReplicationController-owned workload is not Ready at stage $stage" >&2
        kubectl get pods -n migration-apps -l app=migration-replication-controller -o yaml >&2 || true
        return 1
    }
    kubectl logs -n migration-apps "$replication_controller_pod" | grep -F \
        'replication-controller-workload-running' >/dev/null || {
        echo "ReplicationController workload did not execute at stage $stage" >&2
        return 1
    }
    kubectl get customresourcedefinitions.apiextensions.k8s.io ciliumendpoints.cilium.io
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl scale -n migration-apps deployment/migration-nginx --replicas=2
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl get deployment migration-nginx -n migration-apps -o json | jq -e '
      .spec.replicas == 2 and (.status.availableReplicas // 0) == 2
    ' >/dev/null || {
        echo "Deployment /scale did not reconcile two available replicas at stage $stage" >&2
        echo "Deployment state:" >&2
        kubectl get deployment migration-nginx -n migration-apps -o yaml >&2 || true
        echo "ReplicaSet state:" >&2
        kubectl get replicasets -n migration-apps -l app=migration-nginx -o wide >&2 || true
        kubectl get replicasets -n migration-apps -l app=migration-nginx -o yaml >&2 || true
        echo "Pod state:" >&2
        kubectl get pods -n migration-apps -l app=migration-nginx -o wide >&2 || true
        kubectl get events -n migration-apps --sort-by=.metadata.creationTimestamp >&2 || true
        return 1
    }
    kubectl scale -n migration-apps deployment/migration-nginx --replicas=1
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl wait --for=condition=Accepted gatewayclasses.gateway.networking.k8s.io/migration-traefik --timeout=2m
    kubectl wait -n migration-apps --for=condition=Programmed gateways.gateway.networking.k8s.io/migration-traefik --timeout=2m
    wait_for_httproute_condition migration-apps migration-nginx Accepted
    wait_for_httproute_condition migration-apps migration-nginx ResolvedRefs
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-standalone --timeout=5m
    kubectl rollout status -n migration-apps statefulset/migration-stateful --timeout=5m
    exercise_statefulset_scaling "$stage"
    kubectl get replicasets -n migration-apps -l app=migration-nginx -o json | jq -e '.items | length >= 2' >/dev/null || {
        echo "Deployment rollout history was not preserved at stage $stage" >&2
        return 1
    }
    kubectl get controllerrevisions.apps -n migration-apps -o json | jq -e '
      [.items[] | select(any(.metadata.ownerReferences[]?;
        .kind == "StatefulSet" and .name == "migration-stateful"))] | length >= 2
    ' >/dev/null || {
        echo "StatefulSet rollout history was not preserved at stage $stage" >&2
        return 1
    }
    kubectl get endpoints migration-external-db -n migration-apps -o json | jq -e '
      .subsets[0].addresses[0].ip == "192.0.2.20" and
      .subsets[0].ports[0].name == "postgres" and .subsets[0].ports[0].port == 5432
    ' >/dev/null || {
        echo "user-managed Endpoints state changed at stage $stage" >&2
        return 1
    }
    kubectl get endpointslice migration-external-db-v4 -n migration-apps -o json | jq -e '
      .metadata.labels["endpointslice.kubernetes.io/managed-by"] == "nodemigrate-test-fixture" and
      .endpoints[0].addresses == ["192.0.2.20"] and .ports[0].port == 5432
    ' >/dev/null || {
        echo "user-managed EndpointSlice state changed at stage $stage" >&2
        return 1
    }
    kubectl get lease migration-application-lock -n migration-apps -o json | jq -e \
        '.spec.holderIdentity == "nodemigrate-fixture" and .spec.leaseDurationSeconds == 60' >/dev/null || {
        echo "application Lease state changed at stage $stage" >&2
        return 1
    }
    local preserved_annotation
    local configmap_json
    configmap_json="$(kubectl get configmap migration-user-metadata -n migration-apps -o json)"
    preserved_annotation="$(jq -r '.metadata.annotations["nodemigrate.io/source-uid"]' <<<"$configmap_json")"
    [[ "$preserved_annotation" == user-value ]] || {
        echo "nodemigrate changed migration-user-metadata annotation at stage $stage" >&2
        return 1
    }
    [[ "$(jq -r '.data["migration-marker"]' <<<"$configmap_json")" == user-config-data ]] || {
        echo "nodemigrate changed migration-user-metadata data at stage $stage" >&2
        return 1
    }
    kubectl get configmap migration-user-binary -n migration-apps -o json | jq -e \
        '.binaryData["payload.bin"] == "AAECAw=="' >/dev/null || {
        echo "nodemigrate changed migration-user-binary data at stage $stage" >&2
        return 1
    }
    kubectl get configmap migration-user-immutable -n migration-apps -o json | jq -e \
        '.immutable == true and .data["immutable-marker"] == "mounted-immutable-config"' >/dev/null || {
        echo "nodemigrate changed immutable ConfigMap state at stage $stage" >&2
        return 1
    }
    kubectl get customresourcedefinitions.apiextensions.k8s.io migrationrecords.migration.nodemigrate.io -o json | jq -e '
        ([.spec.versions[] | select(.served == true) | .name] | sort) == ["v1", "v1alpha1"] and
        ([.status.conditions[]? | select(.type == "Established" and .status == "True")] | length) == 1
    ' >/dev/null || {
        echo "user CRD versions are not established at stage $stage" >&2
        return 1
    }
    kubectl get --raw '/apis/migration.nodemigrate.io/v1alpha1/migrationrecords/migration-record-0' \
        | jq -e '.apiVersion == "migration.nodemigrate.io/v1alpha1" and .spec.marker == "durable-custom-resource-data"' >/dev/null || {
        echo "custom resource conversion/read through v1alpha1 failed at stage $stage" >&2
        return 1
    }
    kubectl get secret migration-user-secret -n migration-apps -o json | jq -e \
        '.data["migration-secret"] == "bWlncmF0aW9uLXNlY3JldC12YWx1ZQ=="' >/dev/null || {
        echo "nodemigrate changed migration-user-secret data at stage $stage" >&2
        return 1
    }
    [[ "$(kubectl exec -n migration-apps migration-stateful-0 -- cat /state/marker)" == stateful-persistent-data ]] || {
        echo "StatefulSet claim-template data changed at stage $stage" >&2
        return 1
    }
    kubectl wait -n migration-apps --for=condition=Complete job/migration-job --timeout=5m
    kubectl logs -n migration-apps job/migration-job | grep migration-job-ran >/dev/null || {
        echo "Job workload did not execute at stage $stage" >&2
        return 1
    }
    local cron_job="migration-cron-check-$stage"
    kubectl create job --from=cronjob/migration-cron "$cron_job" -n migration-apps
    kubectl wait -n migration-apps --for=condition=Complete "job/$cron_job" --timeout=5m
    kubectl logs -n migration-apps "job/$cron_job" | grep migration-cron-ran >/dev/null || {
        echo "CronJob did not execute its workload at stage $stage" >&2
        return 1
    }
    kubectl delete job -n migration-apps "$cron_job" --wait=true
    local rbac_allow_job="migration-rbac-allow-$stage"
    local rbac_node_job="migration-rbac-node-$stage"
    local rbac_deny_job="migration-rbac-deny-$stage"
    kubectl apply -f - <<YAML
apiVersion: batch/v1
kind: Job
metadata:
  name: $rbac_allow_job
  namespace: migration-apps
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      serviceAccountName: migration-reader
      containers:
      - name: check
        image: $NODEMIGRATE_KUBECTL_IMAGE
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        command: ["kubectl"]
        args: ["get", "configmap", "migration-user-metadata", "-n", "migration-apps", "-o", "name"]
---
apiVersion: batch/v1
kind: Job
metadata:
  name: $rbac_node_job
  namespace: migration-apps
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      serviceAccountName: migration-reader
      containers:
      - name: check
        image: $NODEMIGRATE_KUBECTL_IMAGE
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        command: ["kubectl"]
        args: ["get", "node", "\$(NODE_NAME)", "-o", "name"]
        env:
        - name: NODE_NAME
          valueFrom:
            fieldRef:
              fieldPath: spec.nodeName
---
apiVersion: batch/v1
kind: Job
metadata:
  name: $rbac_deny_job
  namespace: migration-apps
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      serviceAccountName: migration-reader
      containers:
      - name: check
        image: $NODEMIGRATE_KUBECTL_IMAGE
        resources:
          requests:
            cpu: 1m
            memory: 1Mi
        command: ["kubectl"]
        args: ["get", "secret", "migration-user-secret", "-n", "migration-apps", "-o", "name"]
YAML
    kubectl wait -n migration-apps --for=condition=Complete "job/$rbac_allow_job" --timeout=5m
    kubectl wait -n migration-apps --for=condition=Complete "job/$rbac_node_job" --timeout=5m
    kubectl wait -n migration-apps --for=condition=Failed "job/$rbac_deny_job" --timeout=5m
    kubectl logs -n migration-apps "job/$rbac_deny_job" | grep Forbidden >/dev/null || {
        echo "service-account RBAC did not deny Secret access at stage $stage" >&2
        return 1
    }
    kubectl logs -n migration-apps "job/$rbac_node_job" | grep '^node/' >/dev/null || {
        echo "service-account ClusterRole node read did not succeed at stage $stage" >&2
        return 1
    }
    kubectl delete job -n migration-apps "$rbac_allow_job" "$rbac_node_job" "$rbac_deny_job" --wait=true
    verify_legacy_service_account_token "$stage"
    verify_authentication_and_authorization_reviews "$stage"
    verify_custom_resource_status_subresource "$stage"
    verify_ephemeral_container_subresource "$stage"
    kubectl wait -n migration-apps --for=condition=Ready certificate/migration-test --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=5m

    cat > /tmp/nodemigrate-verify-pod.yaml <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: migration-static-data-check
  namespace: migration-apps
spec:
  restartPolicy: Never
  tolerations:
  - key: node-role.kubernetes.io/control-plane
    operator: Exists
    effect: NoSchedule
  containers:
  - name: verify
    image: busybox:1.36.1
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    command: [sh, -c, 'test "$(cat /static/marker)" = static-persistent-data']
    volumeMounts:
    - name: static
      mountPath: /static
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
---
apiVersion: v1
kind: Pod
metadata:
  name: migration-csi-data-check
  namespace: migration-apps
spec:
  restartPolicy: Never
  containers:
  - name: verify
    image: busybox:1.36.1
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    command: [sh, -c, 'test "$(cat /csi/marker)" = csi-persistent-data']
    volumeMounts:
    - name: csi
      mountPath: /csi
  volumes:
  - name: csi
    persistentVolumeClaim:
      claimName: migration-csi-pvc
YAML
    kubectl delete pod -n migration-apps migration-static-data-check migration-csi-data-check \
        --ignore-not-found --wait=true
    kubectl apply -f /tmp/nodemigrate-verify-pod.yaml
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Succeeded \
        pod/migration-static-data-check --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Succeeded \
        pod/migration-csi-data-check --timeout=5m
    kubectl delete pod -n migration-apps migration-static-data-check migration-csi-data-check --wait=true

    kubectl rollout status -n traefik deployment/traefik --timeout=5m
    local traefik_chart_version
    traefik_chart_version="$(helm list -n traefik --output json \
        | jq -r '.[] | select(.name == "traefik" and .status == "deployed") | .chart | sub("^traefik-"; "")')"
    [[ -n "$traefik_chart_version" ]] || {
        echo "Traefik Helm release is missing or not deployed at stage $stage" >&2
        return 1
    }
    helm get manifest traefik -n traefik | grep '^kind: Deployment$' >/dev/null || {
        echo "Traefik Helm release has no Deployment manifest at stage $stage" >&2
        return 1
    }
    helm get values traefik -n traefik -o json | jq -e '.service.type == "ClusterIP"' >/dev/null || {
        echo "Traefik Helm service.type is not ClusterIP at stage $stage" >&2
        return 1
    }
    helm history traefik -n traefik -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null || {
        echo "Traefik Helm release has no deployed revision at stage $stage" >&2
        return 1
    }
    helm get manifest cert-manager -n cert-manager | grep '^kind: Deployment$' >/dev/null || {
        echo "cert-manager Helm release has no Deployment manifest at stage $stage" >&2
        return 1
    }
    helm get values cert-manager -n cert-manager -o json | jq -e '.crds.enabled == true' >/dev/null || {
        echo "cert-manager Helm release does not enable its CRDs at stage $stage" >&2
        return 1
    }
    helm history cert-manager -n cert-manager -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null || {
        echo "cert-manager Helm release has no deployed revision at stage $stage" >&2
        return 1
    }
    local cilium_chart_version
    cilium_chart_version="$(helm list -n kube-system --output json \
        | jq -r '.[] | select(.name == "cilium" and .status == "deployed") | .chart | sub("^cilium-"; "")')"
    [[ -n "$cilium_chart_version" ]] || {
        echo "Cilium Helm release is missing or not deployed at stage $stage" >&2
        return 1
    }
    helm get manifest cilium -n kube-system | grep '^kind: DaemonSet$' >/dev/null || {
        echo "Cilium Helm release has no DaemonSet manifest at stage $stage" >&2
        return 1
    }
    local expected_kpr="${NODEMIGRATE_CILIUM_KPR:-false}"
    local cilium_values
    cilium_values="$(helm get values cilium -n kube-system --all -o json)" || {
        echo "could not read Cilium Helm values at stage $stage" >&2
        return 1
    }
    local expected_clean_state actual_clean_state
    expected_clean_state="$(jq -r '.cleanState // false | tostring' <<<"$cilium_values")"
    actual_clean_state="$(kubectl -n kube-system get configmap cilium-config -o json \
        | jq -r '.data["clean-cilium-state"] // "false"')"
    if [[ "$actual_clean_state" != "$expected_clean_state" ]]; then
        echo "Cilium clean-cilium-state flag was not restored at stage $stage: expected=$expected_clean_state actual=$actual_clean_state" >&2
        return 1
    fi
    if ! jq -e --argjson kpr "$expected_kpr" \
        '.ipam.mode == "kubernetes" and .kubeProxyReplacement == $kpr and .cni.confPath == "/etc/cni/net.d"' \
        <<<"$cilium_values" >/dev/null; then
        echo "Cilium Helm values do not match IPAM, KPR, and CNI settings at stage $stage" >&2
        jq '{ipam: .ipam, kubeProxyReplacement: .kubeProxyReplacement, cni: .cni}' \
            <<<"$cilium_values" >&2 || true
        return 1
    fi
    helm history cilium -n kube-system -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null || {
        echo "Cilium Helm release has no deployed revision at stage $stage" >&2
        return 1
    }
    helm upgrade cilium cilium/cilium -n kube-system --version "$cilium_chart_version" \
        --reuse-values --dry-run=server --hide-secret >/dev/null || {
        echo "Cilium Helm server dry-run failed at stage $stage" >&2
        return 1
    }
    helm upgrade traefik traefik/traefik -n traefik --version "$traefik_chart_version" \
        --reuse-values --dry-run=server --hide-secret >/dev/null || {
        echo "Traefik Helm server dry-run failed at stage $stage" >&2
        return 1
    }
    kubectl delete pod -n traefik migration-route-check --ignore-not-found --wait=true
    kubectl port-forward -n traefik svc/traefik 18080:80 >/tmp/traefik-port-forward.log 2>&1 &
    local port_forward_pid=$!
    kubectl port-forward -n traefik deploy/traefik 18081:8080 >/tmp/gateway-port-forward.log 2>&1 &
    local gateway_port_forward_pid=$!
    trap 'kill "$port_forward_pid" "$gateway_port_forward_pid" 2>/dev/null || true' RETURN
    local response=""
    local gateway_response=""
    local response_body=""
    local gateway_response_body=""
    local response_status=""
    local gateway_response_status=""
    for _ in $(seq 1 30); do
        response="$(curl -sS -H 'Host: migration.test' -w $'\n%{http_code}' http://127.0.0.1:18080/ 2>/dev/null || true)"
        gateway_response="$(curl -sS -H 'Host: migration-gateway.test' -w $'\n%{http_code}' http://127.0.0.1:18081/ 2>/dev/null || true)"
        response_body="${response%$'\n'*}"
        gateway_response_body="${gateway_response%$'\n'*}"
        response_status="${response##*$'\n'}"
        gateway_response_status="${gateway_response##*$'\n'}"
        [[ "$response_body" == *"Welcome to nginx!"* && "$gateway_response_body" == *"Welcome to nginx!"* ]] && break
        sleep 2
    done
    [[ "$response_body" == *"Welcome to nginx!"* && "$gateway_response_body" == *"Welcome to nginx!"* ]] || {
        cat /tmp/traefik-port-forward.log >&2 || true
        cat /tmp/gateway-port-forward.log >&2 || true
        printf 'Ingress probe HTTP status: %s; response body: %.500s\n' "$response_status" "$response_body" >&2
        printf 'Gateway probe HTTP status: %s; response body: %.500s\n' "$gateway_response_status" "$gateway_response_body" >&2
        kubectl get svc,endpoints,endpointslices -n traefik -o wide >&2 || true
        kubectl get svc,endpoints,endpointslices -n migration-apps -o wide >&2 || true
        kubectl get ingress migration-nginx -n migration-apps -o json \
            | jq '{apiVersion, kind, metadata: {name: .metadata.name, namespace: .metadata.namespace}, spec}' >&2 || true
        kubectl get ingressclass traefik -o json \
            | jq '{apiVersion, kind, metadata: {name: .metadata.name, annotations: .metadata.annotations}, spec}' >&2 || true
        kubectl describe ingress migration-nginx -n migration-apps >&2 || true
        kubectl describe httproute migration-nginx -n migration-apps >&2 || true
        kubectl logs deploy/traefik -n traefik --tail=100 >&2 || true
        echo "Traefik Ingress or Gateway API did not route to nginx at stage $stage" >&2
        return 1
    }
    kill "$port_forward_pid" "$gateway_port_forward_pid" 2>/dev/null || true
    trap - RETURN
    kubectl get deploy,svc,ingress,certificate,pv,pvc -A -o wide
    record_fixture_storage_specs "$stage"
    capture_helm_release_state "$stage_dir/helm-releases.jsonl"
    capture_semantic_checkpoint "$stage"
    echo "PASS stage=$stage"
}

capture_semantic_checkpoint() {
    local stage="$1"
    local stage_dir="$CHECKPOINT_DIR/$stage"
    mkdir -p "$stage_dir"
    chmod 0700 "$CHECKPOINT_DIR" "$stage_dir"

    for crd in ingressroutes.traefik.io ingressroutetcps.traefik.io; do
        kubectl get customresourcedefinitions.apiextensions.k8s.io "$crd" -o json | python3 -c '
import json
import sys

stage, expected_name = sys.argv[1:]
obj = json.load(sys.stdin)
schema = obj["spec"]["versions"][0]["schema"]["openAPIV3Schema"]
routes = schema["properties"]["spec"]["properties"]["routes"]
priority = routes["items"]["properties"]["priority"]
maximum = priority["maximum"]
value = format(maximum, ".17g") if isinstance(maximum, float) else str(maximum)
print(f"CRD precision checkpoint stage={stage} crd={expected_name} maximum={value} type={type(maximum).__name__}")
' "$stage" "$crd"
    done

    capture_migratable_api_objects \
        "$stage_dir/migratable-objects.jsonl" \
        "$stage_dir/api-resources.txt"

    # Keep only stable, user-visible state. API-assigned UIDs, resource
    # versions, managed fields, and status timestamps naturally change during
    # a round trip; resource specs, bindings, workload identity, and readiness
    # must still match. Secret data is hashed in a pipe and never written to a
    # checkpoint or the action log.
    kubectl get nodes -o json | jq -S '
      [.items[] | {
        name: .metadata.name,
        controlPlane: ((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master"))),
        worker: (((.metadata.labels // {}) | (has("node-role.kubernetes.io/worker"))) or
          ((.metadata.labels // {}) | (has("node-role.kubernetes.io/control-plane") or has("node-role.kubernetes.io/master")) | not)),
        migrationLabel: ((.metadata.labels // {})["operator.example/pool"] // ""),
        migrationAnnotation: ((.metadata.annotations // {})["nodemigrate.io/source-uid"] // ""),
        migrationTaint: ([.spec.taints[]? | select(.key == "operator.example/dedicated")] | sort_by(.effect, .key, .value)),
        ready: ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1)
      }] | sort_by(.name)
    ' > "$stage_dir/nodes.json"

    kubectl get configmap,secret,serviceaccount,role,rolebinding,deployment,statefulset,daemonset,cronjob,poddisruptionbudget,service,ingress,pvc -n migration-apps -o json \
      | jq -S -f "$ROOT/.github/scripts/nodemigrate-application-snapshot.jq" > "$stage_dir/application.json"
    kubectl get pv -o json \
      | jq -S '[.items[] | select(.spec.claimRef.namespace == "migration-apps") | {
          kind, name: .metadata.name, labels: (.metadata.labels // {}),
          annotations: (.metadata.annotations // {}),
          spec: (.spec | if .claimRef then .claimRef |= del(.uid) else . end)
        }] | sort_by(.name)' > "$stage_dir/persistent-volumes.json"
    kubectl get certificate -n migration-apps migration-test -o json \
      | canonicalize_api_object > "$stage_dir/certificate.json"
    kubectl get clusterissuer migration-selfsigned -o json \
      | canonicalize_api_object > "$stage_dir/issuer.json"

    for namespace in cert-manager traefik; do
        kubectl get deployment -n "$namespace" -o json \
          | canonicalize_api_list > "$stage_dir/$namespace-deployments.json"
    done
    kubectl get daemonset cilium -n kube-system -o json | jq -S '
      {
        name: .metadata.name,
        images: ([.spec.template.spec.containers[].image] | sort),
        desired: .status.desiredNumberScheduled,
        ready: .status.numberReady,
        updated: .status.updatedNumberScheduled
      }
    ' > "$stage_dir/cilium.json"
    kubectl get customresourcedefinitions.apiextensions.k8s.io ciliumendpoints.cilium.io certificates.cert-manager.io \
      clusterissuers.cert-manager.io -o json \
      | jq -S '[.items[] | {
          name: .metadata.name,
          group: .spec.group,
          scope: .spec.scope,
          names: .spec.names,
          versions: [.spec.versions[] | {name, served, storage, schema}]
        }] | sort_by(.name)' > "$stage_dir/required-crds.json"
    kubectl get storageclass csi-hostpath-sc -o json \
      | canonicalize_api_object > "$stage_dir/storageclass.json"
    kubectl get secret migration-test-tls -n migration-apps -o json \
      | jq -S -c '.data // {}' | sha256sum | awk '{print $1}' \
      > "$stage_dir/certificate-secret.sha256"
    kubectl get configmap migration-user-metadata -n migration-apps -o json \
        | jq -S -c '.data // {}' | sha256sum | awk '{print $1}' \
        > "$stage_dir/user-configmap-data.sha256"
    kubectl get configmap migration-user-binary -n migration-apps -o json \
        | jq -S -c '.binaryData // {}' | sha256sum | awk '{print $1}' \
        > "$stage_dir/user-binary-configmap-data.sha256"
    kubectl get configmap migration-user-immutable -n migration-apps -o json \
        | jq -S -c '{immutable, data: (.data // {})}' | sha256sum | awk '{print $1}' \
        > "$stage_dir/user-immutable-configmap.sha256"
    kubectl get secret migration-user-secret -n migration-apps -o json \
        | jq -S -c '.data // {}' | sha256sum | awk '{print $1}' \
        > "$stage_dir/user-secret-data.sha256"

    jq -S -n \
      --slurpfile nodes "$stage_dir/nodes.json" \
      --slurpfile app "$stage_dir/application.json" \
      --slurpfile pvs "$stage_dir/persistent-volumes.json" \
      --slurpfile certificate "$stage_dir/certificate.json" \
      --slurpfile issuer "$stage_dir/issuer.json" \
      --slurpfile cert_manager "$stage_dir/cert-manager-deployments.json" \
      --slurpfile traefik "$stage_dir/traefik-deployments.json" \
      --slurpfile cilium "$stage_dir/cilium.json" \
      --slurpfile crds "$stage_dir/required-crds.json" \
      --slurpfile storageclass "$stage_dir/storageclass.json" \
      --slurpfile migratable_objects "$stage_dir/migratable-objects-summary.jsonl" \
      '{nodes:$nodes[0], application:$app[0], persistentVolumes:$pvs[0],
        certificate:$certificate[0], issuer:$issuer[0],
        certManager:$cert_manager[0], traefik:$traefik[0], cilium:$cilium[0],
        requiredCrds:$crds[0], storageClass:$storageclass[0],
        migratableObjects:$migratable_objects}' \
      > "$stage_dir/semantic-state.json"
    chmod 0600 "$stage_dir"/*.json "$stage_dir"/*.jsonl "$stage_dir"/*.sha256
}

verify_returned_k3s_audit() {
    local audit_log="${NODEMIGRATE_K3S_AUDIT_LOG:-/var/lib/rancher/k3s/server/logs/nodemigrate-audit.log}"
    local service_unit audit_policy
    audit_policy=/etc/rancher/k3s/nodemigrate-audit-policy.yaml
    service_unit="$(systemctl cat k3s)" || {
        echo "could not inspect the returned K3s service unit for audit settings" >&2
        return 1
    }
    grep -Fq -- "--kube-apiserver-arg=audit-policy-file=$audit_policy" <<< "$service_unit" \
        && grep -Fq -- "--kube-apiserver-arg=audit-log-path=$audit_log" <<< "$service_unit" || {
        echo "returned K3s service unit does not retain Node/Lease audit policy and log arguments" >&2
        return 1
    }
    [[ -s "$audit_log" ]] || {
        echo "returned K3s audit log is missing or empty: $audit_log" >&2
        return 1
    }
    python3 - "$audit_log" "$MIGRATION_STARTED_AT" <<'PY'
from datetime import datetime
import json
import sys

path, since_text = sys.argv[1:]
since = datetime.fromisoformat(since_text.replace("Z", "+00:00"))
node_events = []
lease_events = []
malformed = []
with open(path, encoding="utf-8") as audit_file:
    for line_number, line in enumerate(audit_file, 1):
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            malformed.append((line_number, str(error)))
            continue
        timestamp = event.get("requestReceivedTimestamp")
        object_ref = event.get("objectRef") or {}
        if not timestamp:
            continue
        try:
            event_time = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        except ValueError:
            malformed.append((line_number, f"invalid requestReceivedTimestamp {timestamp!r}"))
            continue
        if event_time < since:
            continue
        summary = {
            "time": timestamp,
            "verb": event.get("verb"),
            "resource": object_ref.get("resource"),
            "subresource": object_ref.get("subresource"),
            "name": object_ref.get("name"),
            "username": (event.get("user") or {}).get("username"),
            "code": (event.get("responseStatus") or {}).get("code"),
        }
        if object_ref.get("resource") == "nodes":
            node_events.append(summary)
        if (object_ref.get("resource") == "leases"
                and object_ref.get("namespace") == "kube-node-lease"):
            lease_events.append(summary)

if malformed:
    print(f"K3s audit log contains {len(malformed)} malformed line(s): {malformed[:3]}", file=sys.stderr)
    raise SystemExit(1)
if not node_events:
    print("K3s audit log has no Node API mutations since return migration began", file=sys.stderr)
    raise SystemExit(1)
if not lease_events:
    print("K3s audit log has no kube-node-lease mutations since return migration began", file=sys.stderr)
    raise SystemExit(1)
print(f"PASS returned K3s audit captured {len(node_events)} Node and {len(lease_events)} Lease mutation(s)")
print("Node mutation actors:", json.dumps(node_events, sort_keys=True))
PY
}

capture_migratable_api_objects() {
    local output="$1"
    local summary_output="${output%.jsonl}-summary.jsonl"
    local resource_inventory="$2"
    local resources resource list_json object_json normalized
    local source_inventory="$CHECKPOINT_DIR/source/api-resources.txt"
    if [[ "$resource_inventory" != "$source_inventory" && -f "$source_inventory" ]]; then
        local discovery_error missing deadline discovery_output
        discovery_error="$(mktemp)"
        discovery_output="$(mktemp)"
        deadline=$((SECONDS + 60))
        while true; do
            if kubectl api-resources --verbs=list -o name > "$discovery_output" 2>"$discovery_error"; then
                LC_ALL=C sort -u "$discovery_output" > "$resource_inventory"
                missing="$(comm -23 "$source_inventory" "$resource_inventory")"
                if [[ -z "$missing" ]]; then
                    break
                fi
            else
                missing="$(cat "$source_inventory")"
            fi
            if (( SECONDS >= deadline )); then
                echo "Source-discovered listable API resources did not reappear within 60 seconds at stage=${output%/migratable-objects.jsonl}:" >&2
                printf '%s\n' "$missing" >&2
                if [[ -s "$discovery_error" ]]; then
                    echo "Last API discovery error:" >&2
                    cat "$discovery_error" >&2
                fi
                rm -f "$discovery_error" "$discovery_output"
                return 1
            fi
            sleep 2
        done
        rm -f "$discovery_error" "$discovery_output"
    else
        kubectl api-resources --verbs=list -o name | LC_ALL=C sort -u > "$resource_inventory"
    fi
    resources="$(cat "$resource_inventory")"
    [[ -n "$resources" ]] || {
        echo "Kubernetes API discovery returned no listable resources" >&2
        return 1
    }
    : > "$output"
    while IFS= read -r resource; do
        [[ -n "$resource" ]] || continue
        list_json="$(kubectl get "$resource" --all-namespaces --chunk-size=500 -o json)"
        while IFS= read -r object_json; do
            normalized="$(jq -cS -f "$ROOT/.github/scripts/nodemigrate-snapshot-normalize.jq" \
                <<< "$object_json")"
            [[ -n "$normalized" ]] || continue
            PYTHONPATH="$ROOT/.github/scripts${PYTHONPATH:+:$PYTHONPATH}" python3 -c '
import hashlib
import json
import sys
from nodemigrate_api_inventory import normalize_api_object

def walk(value, path, result):
    if isinstance(value, dict) and value:
        for key, child in value.items():
            escaped = str(key).replace("~", "~0").replace("/", "~1")
            walk(child, f"{path}/{escaped}", result)
    elif isinstance(value, list) and value:
        for index, child in enumerate(value):
            walk(child, f"{path}/{index}", result)
    else:
        canonical = json.dumps(value, sort_keys=True, separators=(",", ":"))
        result[path or "/"] = hashlib.sha256(canonical.encode()).hexdigest()

raw = sys.stdin.read().removesuffix("\n")
obj = normalize_api_object(json.loads(raw))
raw = json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
fields = {}
walk(obj, "", fields)
version = obj.get("apiVersion", "")
group = version.split("/", 1)[0] if "/" in version else ""
metadata = obj.get("metadata") or {}
kind = obj.get("kind")
name = metadata.get("name", "")
labels = metadata.get("labels") or {}
owners = metadata.get("ownerReferences") or []
lifecycle_class = None
if (
    group == "rbac.authorization.k8s.io"
    and kind in {"Role", "RoleBinding", "ClusterRole", "ClusterRoleBinding"}
    and name.startswith("nodebootstrap:")
):
    lifecycle_class = "nodebootstrap-runtime-rbac"
elif kind == "EndpointSlice" and labels.get("endpointslice.kubernetes.io/managed-by") == "nodecontroller":
    lifecycle_class = "nodecontroller-regenerated-endpoints"
elif kind in {"ReplicaSet", "ControllerRevision"} and any(
    owner.get("controller") is True
    and owner.get("kind") in {"Deployment", "StatefulSet", "DaemonSet"}
    for owner in owners
):
    lifecycle_class = "controller-generated-rollout-history"
row = {
    "identity": {
        "apiGroup": group,
        "kind": kind,
        "namespace": metadata.get("namespace", ""),
        "name": name,
    },
    "sha256": hashlib.sha256(raw.encode()).hexdigest(),
    "fields": fields,
    "lifecycleClass": lifecycle_class,
}
if kind == "CustomResourceDefinition":
    row["crdSpec"] = obj.get("spec")
if (
    obj.get("kind") == "StatefulSet"
    and metadata.get("namespace") == "default"
    and metadata.get("name") == "csi-hostpathplugin"
):
    # The CSI catalog root is migration-critical and must stay strict. Keep a
    # narrow, non-secret diagnostic so parity failures show whether the named
    # fixture volume moved or only changed order in the PodSpec list.
    spec = obj.get("spec") or {}
    pod_spec = ((spec.get("template") or {}).get("spec") or {})
    row["csiVolumeDiagnostics"] = [
        {
            "name": volume.get("name"),
            "hostPath": {
                "path": ((volume.get("hostPath") or {}).get("path")),
                "type": ((volume.get("hostPath") or {}).get("type")),
            } if "hostPath" in volume else None,
        }
        for volume in pod_spec.get("volumes", [])
    ]
json.dump(row, sys.stdout, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
print()
' <<< "$normalized" >> "$output"
        done < <(jq -c '.items[]' <<< "$list_json")
    done <<< "$resources"
    LC_ALL=C sort -o "$output" "$output"
    jq -cS '{identity, sha256}' "$output" > "$summary_output"
    chmod 0600 "$output" "$summary_output"
    echo "Captured $(wc -l < "$resource_inventory" | tr -d ' ') listable API resources at stage=${resource_inventory%/api-resources.txt}:"
    cat "$resource_inventory"
}

assert_discovered_api_resources_preserved() {
    local expected="$1"
    local actual="$2"
    local stage="$3"
    local missing
    missing="$(mktemp)"
    comm -23 "$expected" "$actual" > "$missing"
    if [[ -s "$missing" ]]; then
        echo "Source-discovered listable API resources are missing at stage=$stage:" >&2
        cat "$missing" >&2
        rm -f "$missing"
        return 1
    fi
    rm -f "$missing"
    echo "PASS: all $(wc -l < "$expected" | tr -d ' ') source-discovered listable API resources remain exposed at stage=$stage"
}

assert_migratable_api_objects_unchanged() {
    local before="$1"
    local after="$2"
    local source_stage="$3"
    local target_stage="$4"
    if ! python3 - "$before" "$after" "${5:-}" <<'PY'
import json
import sys
import difflib

def records(path):
    with open(path, encoding="utf-8") as source:
        result = {}
        for row in map(json.loads, source):
            identity = json.dumps(row["identity"], sort_keys=True)
            if identity in result:
                raise ValueError(f"duplicate API object identity: {identity}")
            result[identity] = row
        return result

before = records(sys.argv[1])
after = records(sys.argv[2])
replaced_node_name = sys.argv[3] or None

def is_replaced_node_password(identity):
    if replaced_node_name is None:
        return False
    value = json.loads(identity)
    return value == {
        "apiGroup": "",
        "kind": "Secret",
        "name": f"{replaced_node_name}.node-password.k3s",
        "namespace": "kube-system",
    }

missing = sorted(before.keys() - after.keys())
changed = sorted(
    identity for identity in before.keys() & after.keys()
    if before[identity]["sha256"] != after[identity]["sha256"]
    and not is_replaced_node_password(identity)
)
expected_runtime_changes = sorted(
    identity for identity in before.keys() & after.keys()
    if before[identity]["sha256"] != after[identity]["sha256"]
    and is_replaced_node_password(identity)
)
target_only = sorted(after.keys() - before.keys())
print(f"Target-only objects: {len(target_only)}")
for identity in expected_runtime_changes:
    changed_fields = sorted(
        path for path in before[identity].get("fields", {}).keys() | after[identity].get("fields", {}).keys()
        if before[identity].get("fields", {}).get(path) != after[identity].get("fields", {}).get(path)
    )
    if changed_fields != ["/data/hash"]:
        print(f"Unexpected changed paths in K3s node-password Secret {identity}: {changed_fields}", file=sys.stderr)
        sys.exit(1)
    print(f"Expected K3s node-password hash rotation after Node UID replacement: {identity}")
unclassified = []
if target_only:
    print("Target-only API object identities:")
    for identity in target_only:
        row = after[identity]
        lifecycle_class = row.get("lifecycleClass")
        print(f"{identity} lifecycle={lifecycle_class or 'UNCLASSIFIED'}")
        if lifecycle_class is None:
            unclassified.append(identity)
if missing or changed or unclassified:
    if missing:
        print("Source API objects missing on target:", file=sys.stderr)
        print("\n".join(missing), file=sys.stderr)
    if unclassified:
        print("Target-only API objects lack an explicit generated-state classification:", file=sys.stderr)
        print("\n".join(unclassified), file=sys.stderr)
    for identity in changed:
        source_text = [f"sha256: {before[identity]['sha256']}\n"]
        target_text = [f"sha256: {after[identity]['sha256']}\n"]
        sys.stderr.writelines(difflib.unified_diff(
            source_text, target_text,
            fromfile=f"source {identity}", tofile=f"target {identity}", lineterm="\n"
        ))
        source_fields = before[identity].get("fields", {})
        target_fields = after[identity].get("fields", {})
        field_paths = sorted(
            path for path in source_fields.keys() | target_fields.keys()
            if source_fields.get(path) != target_fields.get(path)
        )
        if field_paths:
            print(f"Changed normalized field paths for {identity}:", file=sys.stderr)
            print("\n".join(field_paths), file=sys.stderr)
        source_crd_spec = before[identity].get("crdSpec")
        target_crd_spec = after[identity].get("crdSpec")
        if source_crd_spec is not None and target_crd_spec is not None:
            source_text = json.dumps(source_crd_spec, sort_keys=True, indent=2).splitlines(keepends=True)
            target_text = json.dumps(target_crd_spec, sort_keys=True, indent=2).splitlines(keepends=True)
            spec_diff = list(difflib.unified_diff(
                source_text,
                target_text,
                fromfile=f"source CRD spec {identity}",
                tofile=f"target CRD spec {identity}",
                lineterm="\n",
            ))
            print(f"CRD spec diff for {identity} (first 300 lines):", file=sys.stderr)
            sys.stderr.writelines(spec_diff[:300])
        if "csiVolumeDiagnostics" in before[identity] or "csiVolumeDiagnostics" in after[identity]:
            print(f"CSI test volume values for {identity}:", file=sys.stderr)
            print("source:", file=sys.stderr)
            json.dump(before[identity].get("csiVolumeDiagnostics", []), sys.stderr, indent=2)
            print("\ntarget:", file=sys.stderr)
            json.dump(after[identity].get("csiVolumeDiagnostics", []), sys.stderr, indent=2)
            print(file=sys.stderr)
    sys.exit(1)
print(f"Source objects preserved: {len(before)}; target-only objects: {len(target_only)}")
PY
    then
        echo "Normalized source API object data changed between stages $source_stage and $target_stage:" >&2
        return 1
    fi
    echo "PASS: normalized source API object data is unchanged between stages $source_stage and $target_stage"
}

capture_source_csi_device_volume() {
    local source_file="$CHECKPOINT_DIR/source/csi-hostpath-dev-volume.json"
    local volume
    volume="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get statefulset csi-hostpathplugin \
        -n default -o json | jq -ce '
            [.spec.template.spec.volumes[] | select(.name == "dev-dir")] as $matches
            | if ($matches | length) == 1 and $matches[0].hostPath.path == "/dev" then
                $matches[0]
              else error("source CSI StatefulSet must have exactly one dev-dir /dev hostPath volume")
              end
        ')"
    printf '%s\n' "$volume" > "$source_file"
    chmod 0600 "$source_file"
}

assert_csi_device_volume_matches_source() {
    local kubeconfig="$1"
    local stage="$2"
    local expected actual
    expected="$(jq -cS . "$CHECKPOINT_DIR/source/csi-hostpath-dev-volume.json")"
    actual="$(KUBECONFIG="$kubeconfig" kubectl get statefulset csi-hostpathplugin \
        -n default -o json | jq -cS '
            [.spec.template.spec.volumes[] | select(.name == "dev-dir")][0] // null
        ')"
    if [[ "$actual" != "$expected" ]]; then
        echo "CSI driver dev-dir volume differs from the source at stage=$stage" >&2
        echo "source: $expected" >&2
        echo "target: $actual" >&2
        return 1
    fi
    echo "PASS: source CSI dev-dir volume is preserved at stage=$stage"
}

restore_csi_device_volume_after_fixture_reinstall() {
    local kubeconfig="$1"
    local stage="$2"
    local source_volume plugin_json index patch current_volume
    source_volume="$(cat "$CHECKPOINT_DIR/source/csi-hostpath-dev-volume.json")"
    plugin_json="$(KUBECONFIG="$kubeconfig" kubectl get statefulset csi-hostpathplugin \
        -n default -o json)"
    index="$(jq -r --arg name "$(jq -r '.name' <<<"$source_volume")" '
        [.spec.template.spec.volumes | to_entries[] | select(.value.name == $name) | .key][0] // empty
    ' <<<"$plugin_json")"
    if [[ "$index" =~ ^[0-9]+$ ]]; then
        current_volume="$(jq -cS --argjson index "$index" \
            '.spec.template.spec.volumes[$index]' <<<"$plugin_json")"
        [[ "$current_volume" == "$(jq -cS . <<<"$source_volume")" ]] || {
            patch="$(jq -cn --argjson index "$index" --argjson volume "$source_volume" \
                '[{op:"replace",path:("/spec/template/spec/volumes/" + ($index|tostring)),value:$volume}]')"
            KUBECONFIG="$kubeconfig" kubectl patch statefulset csi-hostpathplugin \
                -n default --type=json -p "$patch"
        }
    else
        patch="$(jq -cn --argjson volume "$source_volume" \
            '[{op:"add",path:"/spec/template/spec/volumes/-",value:$volume}]')"
        KUBECONFIG="$kubeconfig" kubectl patch statefulset csi-hostpathplugin \
            -n default --type=json -p "$patch"
    fi
    KUBECONFIG="$kubeconfig" kubectl rollout status statefulset/csi-hostpathplugin \
        -n default --timeout=5m
    assert_csi_device_volume_matches_source "$kubeconfig" "$stage"
}

canonicalize_api_list() {
    jq -S '[.items[] | {
      apiVersion, kind, name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      labels: (.metadata.labels // {}),
      annotations: (.metadata.annotations // {}),
      ownerReferences: [(.metadata.ownerReferences // [])[] | {apiVersion, kind, name, controller}],
      spec: (.spec // {})
    }] | sort_by(.kind, .namespace, .name)'
}

canonicalize_api_object() {
    jq -S '{apiVersion, kind, name: .metadata.name,
      namespace: (.metadata.namespace // ""),
      labels: (.metadata.labels // {}),
      annotations: (.metadata.annotations // {}),
      spec: (.spec // {})}'
}

assert_round_trip_unchanged() {
    local initial="$CHECKPOINT_DIR/source"
    local returned="$CHECKPOINT_DIR/returned"
    local replaced_node_name=""
    if [[ "$SOURCE_DIST" == k3s ]]; then
        replaced_node_name="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get nodes \
            -o jsonpath='{.items[0].metadata.name}')" || return 1
    fi
    assert_migratable_api_objects_unchanged \
        "$initial/migratable-objects.jsonl" "$returned/migratable-objects.jsonl" \
        source returned "$replaced_node_name"
    assert_discovered_api_resources_preserved \
        "$initial/api-resources.txt" "$returned/api-resources.txt" returned
    jq -S 'del(.migratableObjects)' "$initial/semantic-state.json" \
        > "$initial/round-trip-semantic-state.json"
    jq -S 'del(.migratableObjects)' "$returned/semantic-state.json" \
        > "$returned/round-trip-semantic-state.json"
    if ! cmp -s "$initial/round-trip-semantic-state.json" "$returned/round-trip-semantic-state.json"; then
        echo "Returned Kubernetes semantic state differs from the source checkpoint" >&2
        diff -u "$initial/round-trip-semantic-state.json" "$returned/round-trip-semantic-state.json" || true
        return 1
    fi
    if ! cmp -s "$initial/certificate-secret.sha256" "$returned/certificate-secret.sha256"; then
        echo "Certificate secret contents changed during the migration round trip" >&2
        return 1
    fi
    if ! cmp -s "$initial/user-configmap-data.sha256" "$returned/user-configmap-data.sha256"; then
        echo "ConfigMap contents changed during the migration round trip" >&2
        return 1
    fi
    for digest in user-binary-configmap-data user-immutable-configmap user-secret-data; do
        if ! cmp -s "$initial/$digest.sha256" "$returned/$digest.sha256"; then
            echo "$digest changed during the migration round trip" >&2
            return 1
        fi
    done
    if ! cmp -s "$initial/helm-releases.jsonl" "$returned/helm-releases.jsonl"; then
        echo "Helm release identity, chart version, revision, values, or manifest changed during the migration round trip" >&2
        diff -u "$initial/helm-releases.jsonl" "$returned/helm-releases.jsonl" || true
        return 1
    fi
    echo "PASS: returned semantic state, text and binary ConfigMaps, Secret digests, certificate secret, and PVC data match the source checkpoint"
}

assert_migratable_api_objects_retained() {
    local before="$CHECKPOINT_DIR/$1/migratable-objects.jsonl"
    local after="$CHECKPOINT_DIR/$2/migratable-objects.jsonl"
    local before_ids="$CHECKPOINT_DIR/$1/migratable-object-identities.txt"
    local after_ids="$CHECKPOINT_DIR/$2/migratable-object-identities.txt"
    local missing_ids="$CHECKPOINT_DIR/$2/missing-source-object-identities.txt"
    assert_discovered_api_resources_preserved \
        "$CHECKPOINT_DIR/$1/api-resources.txt" \
        "$CHECKPOINT_DIR/$2/api-resources.txt" "$2"
    jq -cS '.identity' "$before" | LC_ALL=C sort -u > "$before_ids"
    jq -cS '.identity' "$after" | LC_ALL=C sort -u > "$after_ids"
    comm -23 "$before_ids" "$after_ids" > "$missing_ids"
    if [[ -s "$missing_ids" ]]; then
        echo "Source Kubernetes API objects are missing at stage $2" >&2
        cat "$missing_ids" >&2
        return 1
    fi
    if [[ "$2" == replaced && -n "${3:-}" ]]; then
        if ! assert_migratable_api_objects_unchanged "$before" "$after" "$1" "$2" "$3"; then
            return 1
        fi
    else
        if ! assert_migratable_api_objects_unchanged "$before" "$after" "$1" "$2"; then
            return 1
        fi
    fi

    # These fixture resources also have direct semantic snapshots and
    # behavioral probes. The general inventory compares normalized source
    # object data; it does not include status, server-owned metadata, or the
    # runtime-only objects filtered by the snapshot normalizer.
    for snapshot in application.json certificate.json issuer.json required-crds.json storageclass.json \
        certificate-secret.sha256 user-configmap-data.sha256 user-binary-configmap-data.sha256 \
        user-immutable-configmap.sha256 user-secret-data.sha256 helm-releases.jsonl; do
        if ! cmp -s "$CHECKPOINT_DIR/$1/$snapshot" "$CHECKPOINT_DIR/$2/$snapshot"; then
            echo "Durable migration fixture state differs between stages $1 and $2: $snapshot" >&2
            diff -u "$CHECKPOINT_DIR/$1/$snapshot" "$CHECKPOINT_DIR/$2/$snapshot" || true
            return 1
        fi
    done
    echo "PASS: all source API object identities and durable fixture state were retained at stage $2"
}

main() {
    need_root
    install_tools
    install_source
    install_cilium
    KUBECONFIG="$SOURCE_KUBECONFIG" install_hostpath_driver /var/lib/kubelet
    # Put the Nodelet target mount in the source fixture before the baseline is
    # captured. The target-side setup below is then idempotent, and the parity
    # comparison continues to check the complete CSI StatefulSet spec.
    KUBECONFIG="$SOURCE_KUBECONFIG" ensure_csi_can_access_nodelet_root
    install_workloads
    KUBECONFIG="$SOURCE_KUBECONFIG" pin_hostpath_driver_to_fixture_volumes
    verify_stage source "$SOURCE_KUBECONFIG"
    capture_source_csi_device_volume

    if [[ "${NODEMIGRATE_K3S_CILIUM_RESTART_PROBE:-false}" == true ]]; then
        MIGRATION_STARTED_AT="$(date -u --iso-8601=seconds)"
        probe_api_clusterip_from_pod "$SOURCE_KUBECONFIG" source
        echo "Cilium datapath before K3s restart"
        capture_cilium_datapath "$SOURCE_KUBECONFIG"

        restart_cilium_agent_pod "$SOURCE_KUBECONFIG"
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl wait \
            --for=condition=Ready node --all --timeout=5m
        verify_stage cilium-agent-restarted "$SOURCE_KUBECONFIG"
        assert_migratable_api_objects_retained source cilium-agent-restarted
        probe_api_clusterip_from_pod "$SOURCE_KUBECONFIG" cilium-agent-restarted
        echo "Cilium datapath after Cilium agent Pod recreation"
        capture_cilium_datapath "$SOURCE_KUBECONFIG"

        local cilium_container_before cilium_container_after cilium_pod_json
        local node_uid_before_handoff node_uid_after_handoff
        node_uid_before_handoff="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get nodes \
            -o json | jq -er 'if (.items | length) == 1 then .items[0].metadata.uid else empty end')"
        cilium_container_before="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get pods \
            -n kube-system -l k8s-app=cilium -o json | jq -er '
              if (.items | length) == 1 then
                .items[0].status.containerStatuses[]?
                | select(.name == "cilium-agent") | .containerID
              else empty end
            ')" || {
            echo "could not identify the source Cilium agent container before sandbox handoff" >&2
            return 1
        }
        echo "Stopping only source Cilium pod sandboxes before restarting K3s; nodemigrate remains disabled"
        stop_source_cilium_sandboxes_for_probe
        systemctl restart k3s
        local attempt
        for attempt in $(seq 1 90); do
            if KUBECONFIG="$SOURCE_KUBECONFIG" kubectl --request-timeout=2s \
                get --raw=/readyz >/dev/null 2>&1; then
                break
            fi
            sleep 2
        done
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl --request-timeout=5s \
            get --raw=/readyz >/dev/null || {
            echo "K3s API did not recover after the restart-only probe" >&2
            return 1
        }
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl wait \
            --for=condition=Ready node --all --timeout=5m
        node_uid_after_handoff="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get nodes \
            -o json | jq -er 'if (.items | length) == 1 then .items[0].metadata.uid else empty end')"
        [[ "$node_uid_after_handoff" == "$node_uid_before_handoff" ]] || {
            echo "Node identity changed during sandbox handoff: before=$node_uid_before_handoff after=$node_uid_after_handoff" >&2
            return 1
        }
        echo "PASS Node UID remained $node_uid_after_handoff across the sandbox handoff"
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl rollout status \
            daemonset/cilium -n kube-system --timeout=5m
        cilium_pod_json="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get pods \
            -n kube-system -l k8s-app=cilium -o json)"
        cilium_container_after="$(jq -er '
          if (.items | length) == 1 and any(.items[0].status.containerStatuses[]?;
              .name == "cilium-agent" and .ready == true) then
            .items[0].status.containerStatuses[]
            | select(.name == "cilium-agent") | .containerID
          else empty end
        ' <<< "$cilium_pod_json")" || {
            echo "Cilium agent did not become Ready after source sandbox recreation" >&2
            return 1
        }
        [[ "$cilium_container_after" != "$cilium_container_before" ]] || {
            echo "Cilium agent container identity did not change after CRI sandbox recreation" >&2
            return 1
        }
        echo "PASS Cilium agent container changed from $cilium_container_before to $cilium_container_after after full sandbox recreation"
        verify_stage sandbox-handoff-restarted "$SOURCE_KUBECONFIG"
        assert_migratable_api_objects_retained source sandbox-handoff-restarted
        probe_api_clusterip_from_pod "$SOURCE_KUBECONFIG" sandbox-handoff-restarted
        echo "Cilium datapath after all source CRI sandboxes were recreated"
        capture_cilium_datapath "$SOURCE_KUBECONFIG"

        local node_name previous_node_uid replacement_node_json replacement_node_uid
        node_name="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get nodes \
            -o jsonpath='{.items[0].metadata.name}')"
        previous_node_uid="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get node "$node_name" \
            -o jsonpath='{.metadata.uid}')"
        echo "Deleting Node $node_name UID=$previous_node_uid before another K3s restart"
        replacement_node_uid="$(KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get node "$node_name" \
            -o jsonpath='{.metadata.uid}')"
        if [[ "$replacement_node_uid" != "$previous_node_uid" ]]; then
            echo "Node $node_name changed before the diagnostic replacement: expected UID=$previous_node_uid, found UID=$replacement_node_uid" >&2
            return 1
        fi
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl delete node "$node_name" \
            --wait=true --timeout=60s
        if KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get node "$node_name" \
            --request-timeout=5s >/dev/null 2>&1; then
            echo "Node $node_name still exists after the diagnostic delete completed" >&2
            return 1
        fi
        systemctl restart k3s
        replacement_node_uid=""
        for attempt in $(seq 1 90); do
            if ! KUBECONFIG="$SOURCE_KUBECONFIG" kubectl --request-timeout=2s \
                get --raw=/readyz >/dev/null 2>&1; then
                sleep 2
                continue
            fi
            replacement_node_json="$(KUBECONFIG="$SOURCE_KUBECONFIG" \
                kubectl --request-timeout=2s get node "$node_name" -o json 2>/dev/null || true)"
            replacement_node_uid="$(jq -r '.metadata.uid // empty' \
                <<<"$replacement_node_json")"
            if [[ -n "$replacement_node_uid" && "$replacement_node_uid" != "$previous_node_uid" ]] \
                && jq -e 'any(.status.conditions[]?; .type == "Ready" and .status == "True")' \
                    <<<"$replacement_node_json" >/dev/null; then
                break
            fi
            sleep 2
        done
        [[ -n "$replacement_node_uid" && "$replacement_node_uid" != "$previous_node_uid" ]] || {
            echo "K3s did not register a Ready replacement Node $node_name after restart" >&2
            return 1
        }
        echo "Replacement Node $node_name UID=$replacement_node_uid is Ready"
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl label node "$node_name" \
            operator.example/pool=blue --overwrite
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl annotate node "$node_name" \
            nodemigrate.io/source-uid=operator-node-value --overwrite
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl taint node "$node_name" \
            operator.example/dedicated=migration:PreferNoSchedule --overwrite
        echo "Cilium datapath after same-name Node replacement"
        capture_cilium_datapath "$SOURCE_KUBECONFIG"
        verify_stage replaced "$SOURCE_KUBECONFIG"
        assert_migratable_api_objects_retained source replaced "$node_name"
        probe_api_clusterip_from_pod "$SOURCE_KUBECONFIG" replaced
        echo "PASS: K3s+Cilium Service and workload behavior survived full source sandbox recreation and same-name Node replacement without nodemigrate"
        return 0
    fi

    export NOTK8S_COMBINED_PREBUILT="$NK"
    export NODEBOOTSTRAP_COMBINED_SELF="$NK"
    export NOTK8S_BUILD_LAYOUT=combined
    export NODEBOOTSTRAP_REPO_ROOT="$ROOT"
    local nodestore_kubeconfig=/etc/nodebootstrap/admin.kubeconfig
    MIGRATION_STARTED_AT="$(date -u --iso-8601=seconds)"
    mkdir -p "$WORK"
    TARGET_WATCH_STOP_FILE="$WORK/target-forward-watch.$$.stop"
    TARGET_WATCH_LOG="$WORK/target-forward-watch.$$.log"
    rm -f "$TARGET_WATCH_STOP_FILE" "$TARGET_WATCH_LOG"
    watch_migration_target_state "$nodestore_kubeconfig" nodeapiserver \
        "$TARGET_WATCH_STOP_FILE" "$TARGET_WATCH_LOG" &
    TARGET_WATCH_PID=$!
    echo "Migrating $SOURCE_DIST -> nodestore"
    local migration_status=0
    (
        export NODEMIGRATE_SOURCE_KUBECONFIG="$SOURCE_KUBECONFIG"
        export NODEMIGRATE_DESTINATION_KUBECONFIG="$nodestore_kubeconfig"
        exec "$MIGRATE" to=nodestore "from=$SOURCE_DIST"
    ) &
    local migration_pid=$!
    if wait "$migration_pid"; then
        migration_status=0
    else
        migration_status=$?
    fi
    stop_target_forward_watch
    if [[ "$migration_status" -eq 0 ]]; then
        :
    else
        echo "Forward migration failed with status $migration_status; checking source rollback"
        local source_service=k3s
        [[ "$SOURCE_DIST" == kubernetes ]] && source_service=kubelet
        systemctl is-active --quiet "$source_service" || {
            echo "FAIL: source service $source_service was not restored after migration failure" >&2
            return 1
        }
        local attempt
        for attempt in $(seq 1 60); do
            if KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get --raw=/readyz >/dev/null 2>&1; then
                break
            fi
            sleep 2
        done
        KUBECONFIG="$SOURCE_KUBECONFIG" kubectl get --raw=/readyz >/dev/null || {
            echo "FAIL: source API did not recover after migration failure" >&2
            return 1
        }
        local recovery_dir
        recovery_dir="$(sed -n 's/^Protected API object export saved at //p' "$LOG" | tail -n 1)"
        [[ -n "$recovery_dir" && -d "$recovery_dir" ]] || {
            echo "FAIL: protected API export is missing after migration failure" >&2
            return 1
        }
        echo "PASS: source service and API recovered; protected export retained at $recovery_dir"
        return "$migration_status"
    fi
    CURRENT_KUBECONFIG="$nodestore_kubeconfig"
    export KUBECONFIG="$nodestore_kubeconfig"
    assert_csi_device_volume_matches_source "$nodestore_kubeconfig" after-forward-migration
    KUBECONFIG="$nodestore_kubeconfig" install_hostpath_driver /var/lib/nodelet true
    restore_csi_device_volume_after_fixture_reinstall "$nodestore_kubeconfig" nodestore
    verify_stage nodestore "$nodestore_kubeconfig"
    assert_migratable_api_objects_retained source nodestore

    MIGRATION_STARTED_AT="$(date -u --iso-8601=seconds)"
    echo "Migrating nodestore -> $SOURCE_DIST"
    local source_api_service=k3s
    [[ "$SOURCE_DIST" == kubernetes ]] && source_api_service=kubelet
    TARGET_WATCH_STOP_FILE="$WORK/target-return-watch.$$.stop"
    TARGET_WATCH_LOG="$WORK/target-return-watch.$$.log"
    rm -f "$TARGET_WATCH_STOP_FILE" "$TARGET_WATCH_LOG"
    watch_migration_target_state "$SOURCE_KUBECONFIG" "$source_api_service" \
        "$TARGET_WATCH_STOP_FILE" "$TARGET_WATCH_LOG" &
    TARGET_WATCH_PID=$!
    local return_migration_status=0
    if NODEMIGRATE_REPLACE_NODE=true \
        NODEMIGRATE_SOURCE_KUBECONFIG="$nodestore_kubeconfig" \
        NODEMIGRATE_DESTINATION_KUBECONFIG="$SOURCE_KUBECONFIG" \
            "$MIGRATE" "to=$SOURCE_DIST" from=nodestore; then
        return_migration_status=0
    else
        return_migration_status=$?
    fi
    if [[ "$return_migration_status" -ne 0 ]]; then
        echo "Return migration failed with status $return_migration_status; checking nodestore rollback"
        local attempt
        for attempt in $(seq 1 60); do
            if systemctl is-active --quiet nodestore \
                && systemctl is-active --quiet nodeapiserver \
                && KUBECONFIG="$nodestore_kubeconfig" kubectl --request-timeout=2s \
                    get --raw=/readyz >/dev/null 2>&1; then
                break
            fi
            sleep 2
        done
        systemctl is-active --quiet nodestore \
            && systemctl is-active --quiet nodeapiserver || {
            echo "FAIL: nodestore services were not restored after return migration failure" >&2
            return 1
        }
        KUBECONFIG="$nodestore_kubeconfig" kubectl --request-timeout=5s \
            get --raw=/readyz >/dev/null || {
                echo "FAIL: nodestore API did not recover after return migration failure" >&2
                return 1
            }
        local recovery_dir
        recovery_dir="$(sed -n 's/^Protected API object export saved at //p' "$LOG" | tail -n 1)"
        [[ -n "$recovery_dir" && -d "$recovery_dir" ]] || {
            echo "FAIL: protected API export is missing after return migration failure" >&2
            return 1
        }
        verify_stage nodestore "$nodestore_kubeconfig"
        echo "PASS: nodestore API and fixture state recovered; protected export retained at $recovery_dir"
        return "$return_migration_status"
    fi
    CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
    export KUBECONFIG="$SOURCE_KUBECONFIG"
    if [[ "$SOURCE_DIST" == k3s ]]; then
        verify_returned_k3s_audit
    fi
    assert_csi_device_volume_matches_source "$SOURCE_KUBECONFIG" after-return-migration
    KUBECONFIG="$SOURCE_KUBECONFIG" install_hostpath_driver /var/lib/kubelet true
    restore_csi_device_volume_after_fixture_reinstall "$SOURCE_KUBECONFIG" returned
    verify_stage returned "$SOURCE_KUBECONFIG"
    assert_round_trip_unchanged
    stop_target_forward_watch
}

if [[ "$LIBRARY_MODE" != true ]]; then
    main "$@"
fi
