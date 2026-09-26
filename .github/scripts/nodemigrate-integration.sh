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

watch_target_forward_state() {
    local kubeconfig="${1:?missing target kubeconfig}"
    local stop_file="${2:?missing watcher stop file}"
    local output_file="${3:?missing watcher output file}"
    local previous_snapshot="" snapshot now last_capture=0
    : > "$output_file"
    while [[ ! -e "$stop_file" ]]; do
        if systemctl is-active --quiet nodeapiserver \
            && KUBECONFIG="$kubeconfig" kubectl --request-timeout=2s \
                get --raw=/readyz >/dev/null 2>&1; then
            snapshot="$(
                echo 'system pods:'
                KUBECONFIG="$kubeconfig" kubectl get pods -n kube-system \
                    -l 'k8s-app in (cilium,cilium-envoy,kube-dns)' -o json 2>&1 \
                    | jq -cS '[.items[]? | {
                        name: .metadata.name,
                        phase: .status.phase,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        containers: [.status.containerStatuses[]? | {name, ready, restartCount, state}],
                        initContainers: [.status.initContainerStatuses[]? | {name, ready, restartCount, state}],
                        podIP: .status.podIP,
                        nodeName: .spec.nodeName
                    }]' || true
                echo 'target node scheduling and readiness:'
                KUBECONFIG="$kubeconfig" kubectl get nodes -o json 2>&1 \
                    | jq -cS '[.items[]? | {
                        name: .metadata.name,
                        unschedulable: (.spec.unschedulable // false),
                        taints: .spec.taints,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        addresses: .status.addresses
                    }]' || true
                echo 'cert-manager pods:'
                KUBECONFIG="$kubeconfig" kubectl get pods -n cert-manager -o json 2>&1 \
                    | jq -cS '[.items[]? | {
                        name: .metadata.name,
                        phase: .status.phase,
                        conditions: [.status.conditions[]? | {type, status, reason, message}],
                        containers: [.status.containerStatuses[]? | {name, ready, restartCount, state}],
                        podIP: .status.podIP,
                        nodeName: .spec.nodeName
                    }]' || true
                echo 'cert-manager services and endpoints:'
                KUBECONFIG="$kubeconfig" kubectl get services,endpoints,endpointslices \
                    -n cert-manager -o json 2>&1 \
                    | jq -cS '[.items[]? | {
                        kind,
                        name: .metadata.name,
                        clusterIP: .spec.clusterIP,
                        selector: .spec.selector,
                        servicePorts: .spec.ports,
                        endpointPorts: .ports,
                        subsets,
                        endpoints: [.endpoints[]? | {addresses, conditions}]
                    }]' || true
                echo 'kubernetes API service:'
                KUBECONFIG="$kubeconfig" kubectl get service kubernetes \
                    -n default -o json 2>&1 \
                    | jq -cS '{clusterIP: .spec.clusterIP, clusterIPs: .spec.clusterIPs, ports: .spec.ports}' || true
                echo 'kubernetes API endpoints:'
                KUBECONFIG="$kubeconfig" kubectl get endpoints kubernetes \
                    -n default -o json 2>&1 \
                    | jq -cS '[.subsets[]? | {addresses, notReadyAddresses, ports}]' || true
                KUBECONFIG="$kubeconfig" kubectl get endpointslices \
                    -n default -l kubernetes.io/service-name=kubernetes -o json 2>&1 \
                    | jq -cS '[.items[]? | {
                        name: .metadata.name,
                        ports,
                        endpoints: [.endpoints[]? | {addresses, conditions, nodeName, targetRef}]
                    }]' || true
                echo 'nodeproxy service state:'
                systemctl is-active nodeproxy 2>&1 || true
                echo 'namespace service-account CA bundles match target API CA:'
                target_ca_b64="$(KUBECONFIG="$kubeconfig" kubectl config view \
                    --raw --flatten --minify -o json \
                    | jq -r '.clusters[0].cluster["certificate-authority-data"] // empty' \
                    || true)"
                if [[ -n "$target_ca_b64" ]]; then
                    KUBECONFIG="$kubeconfig" kubectl get configmaps -A -o json 2>&1 \
                        | jq -cS --arg ca "$target_ca_b64" '
                            [.items[]? | select(.metadata.name == "kube-root-ca.crt") | {
                              namespace: .metadata.namespace,
                              matchesTargetApiCa: ((.data["ca.crt"] // "" | @base64) == $ca)
                            }]
                          ' || true
                    echo 'target API CA fingerprint:'
                    printf '%s' "$target_ca_b64" | base64 -d 2>/dev/null | sha256sum || true
                    echo 'mounted service-account CA fingerprints for Cilium and cert-manager:'
                    KUBECONFIG="$kubeconfig" kubectl get pods -A -o json 2>/dev/null \
                        | jq -r '
                            .items[]?
                            | select((.metadata.namespace == "kube-system" and
                                      (.metadata.labels["k8s-app"] // "") == "cilium") or
                                     (.metadata.namespace == "cert-manager" and
                                      (.metadata.labels["app.kubernetes.io/component"] // "") == "webhook"))
                            | .metadata.namespace as $ns
                            | .metadata.name as $pod
                            | .spec.containers[]?.name
                            | [$ns, $pod, .] | @tsv
                          ' \
                        | while IFS=$'\t' read -r pod_ns pod_name container_name; do
                            [[ -n "$pod_name" && -n "$container_name" ]] || continue
                            printf '%s/%s container=%s ' "$pod_ns" "$pod_name" "$container_name"
                            if mounted_ca_b64="$(KUBECONFIG="$kubeconfig" \
                                kubectl --request-timeout=5s exec -n "$pod_ns" \
                                    "$pod_name" -c "$container_name" -- \
                                    cat /var/run/secrets/kubernetes.io/serviceaccount/ca.crt \
                                | base64 -w0)"; then
                                printf '%s' "$mounted_ca_b64" | base64 -d 2>/dev/null \
                                    | sha256sum || true
                            else
                                echo 'mounted CA unavailable (exec failed; see preceding error)'
                            fi
                        done || true
                else
                    echo 'target admin kubeconfig did not expose a flattened API CA'
                fi
            )"
            now="$(date +%s)"
            if [[ "$snapshot" != "$previous_snapshot" || $((now - last_capture)) -ge 30 ]]; then
                {
                    echo "Target cluster state at $(date -u +%FT%TZ):"
                    printf '%s\n' "$snapshot"
                    echo "Target CoreDNS logs:"
                    KUBECONFIG="$kubeconfig" kubectl logs -n kube-system \
                        -l k8s-app=kube-dns --all-containers --tail=100 2>&1 || true
                    echo "Target Cilium agent logs:"
                    KUBECONFIG="$kubeconfig" kubectl logs -n kube-system \
                        -l k8s-app=cilium -c cilium-agent --tail=100 2>&1 || true
                    echo "Target cert-manager webhook logs:"
                    KUBECONFIG="$kubeconfig" kubectl logs -n cert-manager \
                        -l app.kubernetes.io/component=webhook --all-containers --tail=100 2>&1 || true
                    echo "Target events:"
                    KUBECONFIG="$kubeconfig" kubectl get events -A \
                        --sort-by=.lastTimestamp 2>&1 | tail -n 80 || true
                } >> "$output_file"
                previous_snapshot="$snapshot"
                last_capture="$now"
            fi
        fi
        sleep 5
    done
}

stop_target_forward_watch() {
    [[ -n "$TARGET_WATCH_PID" ]] || return 0
    touch "$TARGET_WATCH_STOP_FILE"
    wait "$TARGET_WATCH_PID" 2>/dev/null || true
    TARGET_WATCH_PID=""
    echo "Target state captured during forward migration:"
    if [[ -s "$TARGET_WATCH_LOG" ]]; then
        cat "$TARGET_WATCH_LOG"
    else
        echo "The target API did not become ready while the migration was running."
    fi
}

capture_cni_host_diagnostics() {
    local config file pod
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
    pod="$(kubectl -n kube-system get pods -l k8s-app=cilium \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
    if [[ -n "$pod" ]]; then
        echo "Cilium agent status from pod/$pod:"
        kubectl -n kube-system exec "$pod" -c cilium-agent -- \
            cilium-dbg status --verbose 2>&1 || true
    fi
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
        if [[ -n "$CURRENT_KUBECONFIG" && -f "$CURRENT_KUBECONFIG" ]]; then
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get nodes -o wide || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl describe nodes || true
            KUBECONFIG="$CURRENT_KUBECONFIG" kubectl get pods,pvc,pv -A -o wide || true
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
            for pod in migration-seed migration-standalone; do
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
    [[ -x "$NK" && -x "$MIGRATE" ]] || {
        echo "build target/release/notk8s and target/release/nodemigrate first" >&2
        exit 2
    }
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
        curl -sfL https://get.k3s.io -o /tmp/install-k3s.sh
        INSTALL_K3S_VERSION="$k3s_version" \
        INSTALL_K3S_EXEC='server --flannel-backend=none --disable-network-policy --disable=traefik --cluster-cidr=10.42.0.0/16 --write-kubeconfig-mode=644' \
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
        kubeadm init \
            --kubernetes-version "$stable" \
            --pod-network-cidr=10.42.0.0/16 \
            --service-cidr=10.96.0.0/12 \
            --cri-socket=unix:///run/containerd/containerd.sock
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
        --set kubeProxyReplacement=false \
        --set operator.replicas=1 \
        --set k8sServiceHost="$api_host" \
        --set k8sServicePort=6443 \
        --wait --timeout 10m
    kubectl rollout status daemonset/cilium -n kube-system --timeout=10m
    kubectl rollout status deployment/cilium-operator -n kube-system --timeout=10m
    kubectl wait --for=condition=Ready node --all --timeout=5m
}

install_hostpath_driver() {
    local kubelet_data_dir="${1:?missing kubelet data directory}"
    local skip_snapshot_crds="${2:-false}"
    git -C "$ROOT" fetch --no-tags --depth=1 origin archive-shell-scripts-0.7.1
    git -C "$ROOT" show FETCH_HEAD:deploy/lib/e2e-full-setup.sh > /tmp/nodemigrate-hostpath-setup.sh
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
    if ! NODELET_DATA_DIR="$kubelet_data_dir" timeout 600 bash /tmp/nodemigrate-hostpath-setup.sh; then
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
    kubectl patch statefulset csi-hostpathplugin -n default --type=json -p "$state_patch"
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
        kubectl patch statefulset csi-hostpathplugin -n default --type=json -p "$stage_patch"
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
    fi
    kubectl get storageclass csi-hostpath-sc
}

install_workloads() {
    local stage=source
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
  - name: v1
    served: true
    storage: true
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
    if [[ -n "${NODEMIGRATE_STATIC_NODE:-}" ]]; then
        [[ "$NODEMIGRATE_STATIC_NODE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || {
            echo "invalid static PV node name: $NODEMIGRATE_STATIC_NODE" >&2
            return 1
        }
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
          values: ["$NODEMIGRATE_STATIC_NODE"]
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
    kubectl apply -f - <<'YAML'
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
  name: migration-seed
  namespace: migration-apps
spec:
  restartPolicy: Never
  initContainers:
  - name: seed-volumes
    image: busybox:1.36.1
    command: [sh, -c, 'echo static-persistent-data > /static/marker; echo csi-persistent-data > /csi/marker']
    resources:
      requests:
        cpu: 1m
        memory: 1Mi
    volumeMounts:
    - name: static
      mountPath: /static
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
    - name: static
      mountPath: /static
    - name: csi
      mountPath: /csi
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
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
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=10m
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-seed --timeout=10m
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
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
    kubectl delete pod -n migration-apps migration-seed --wait=true
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

verify_stage() {
    local stage="$1"
    CURRENT_KUBECONFIG="$2"
    export KUBECONFIG="$CURRENT_KUBECONFIG"
    echo "Verifying stage=$stage distro=$SOURCE_DIST kubeconfig=$CURRENT_KUBECONFIG"
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
    kubectl wait --for=condition=Ready node --all --timeout=5m
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
    kubectl rollout status daemonset/cilium -n kube-system --timeout=5m
    kubectl rollout status -n migration-apps daemonset/migration-daemon --timeout=5m
    local expected_daemon_nodes actual_daemon_nodes
    expected_daemon_nodes="$(kubectl get nodes -o json | jq '.items | length')"
    actual_daemon_nodes="$(kubectl get daemonset migration-daemon -n migration-apps -o json \
        | jq -r '[.status.desiredNumberScheduled, .status.numberReady] | @tsv')"
    [[ "$actual_daemon_nodes" == "$expected_daemon_nodes"$'\t'"$expected_daemon_nodes" ]] || {
        echo "DaemonSet migration-daemon is not ready on every node at stage $stage: expected=$expected_daemon_nodes/$expected_daemon_nodes actual=$actual_daemon_nodes" >&2
        return 1
    }
    kubectl get customresourcedefinitions.apiextensions.k8s.io ciliumendpoints.cilium.io
    kubectl rollout status -n migration-apps deployment/migration-nginx --timeout=5m
    kubectl wait --for=condition=Accepted gatewayclasses.gateway.networking.k8s.io/migration-traefik --timeout=2m
    kubectl wait -n migration-apps --for=condition=Programmed gateways.gateway.networking.k8s.io/migration-traefik --timeout=2m
    wait_for_httproute_condition migration-apps migration-nginx Accepted
    wait_for_httproute_condition migration-apps migration-nginx ResolvedRefs
    kubectl wait -n migration-apps --for=condition=Ready pod/migration-standalone --timeout=5m
    kubectl rollout status -n migration-apps statefulset/migration-stateful --timeout=5m
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
    kubectl wait -n migration-apps --for=condition=Ready certificate/migration-test --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-static-pvc --timeout=5m
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Bound pvc/migration-csi-pvc --timeout=5m

    cat > /tmp/nodemigrate-verify-pod.yaml <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: migration-data-check
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
    command: [sh, -c, 'test "$(cat /static/marker)" = static-persistent-data && test "$(cat /csi/marker)" = csi-persistent-data']
    volumeMounts:
    - name: static
      mountPath: /static
    - name: csi
      mountPath: /csi
  volumes:
  - name: static
    persistentVolumeClaim:
      claimName: migration-static-pvc
  - name: csi
    persistentVolumeClaim:
      claimName: migration-csi-pvc
YAML
    kubectl delete pod -n migration-apps migration-data-check --ignore-not-found --wait=true
    kubectl apply -f /tmp/nodemigrate-verify-pod.yaml
    kubectl wait -n migration-apps --for=jsonpath='{.status.phase}'=Succeeded pod/migration-data-check --timeout=5m
    kubectl delete pod -n migration-apps migration-data-check --wait=true

    kubectl rollout status -n traefik deployment/traefik --timeout=5m
    local traefik_chart_version
    traefik_chart_version="$(helm list -n traefik --output json \
        | jq -r '.[] | select(.name == "traefik" and .status == "deployed") | .chart | sub("^traefik-"; "")')"
    [[ -n "$traefik_chart_version" ]] || {
        echo "Traefik Helm release is missing or not deployed at stage $stage" >&2
        return 1
    }
    helm get manifest traefik -n traefik | grep '^kind: Deployment$' >/dev/null
    helm get values traefik -n traefik -o json | jq -e '.service.type == "ClusterIP"' >/dev/null
    helm history traefik -n traefik -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null
    helm get manifest cert-manager -n cert-manager | grep '^kind: Deployment$' >/dev/null
    helm get values cert-manager -n cert-manager -o json | jq -e '.crds.enabled == true' >/dev/null
    helm history cert-manager -n cert-manager -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null
    local cilium_chart_version
    cilium_chart_version="$(helm list -n kube-system --output json \
        | jq -r '.[] | select(.name == "cilium" and .status == "deployed") | .chart | sub("^cilium-"; "")')"
    [[ -n "$cilium_chart_version" ]] || {
        echo "Cilium Helm release is missing or not deployed at stage $stage" >&2
        return 1
    }
    helm get manifest cilium -n kube-system | grep '^kind: DaemonSet$' >/dev/null
    helm get values cilium -n kube-system -o json | jq -e \
        '.ipam.mode == "kubernetes" and .kubeProxyReplacement == false and .cni.confPath == "/etc/cni/net.d"' >/dev/null
    helm history cilium -n kube-system -o json | jq -e 'any(.[]; .status == "deployed")' >/dev/null
    helm upgrade cilium cilium/cilium -n kube-system --version "$cilium_chart_version" \
        --reuse-values --dry-run=server --hide-secret >/dev/null
    helm upgrade traefik traefik/traefik -n traefik --version "$traefik_chart_version" \
        --reuse-values --dry-run=server --hide-secret >/dev/null
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
    capture_semantic_checkpoint "$stage"
    echo "PASS stage=$stage"
}

capture_semantic_checkpoint() {
    local stage="$1"
    local stage_dir="$CHECKPOINT_DIR/$stage"
    mkdir -p "$stage_dir"
    chmod 0700 "$CHECKPOINT_DIR" "$stage_dir"

    capture_migratable_api_objects "$stage_dir/migratable-objects.jsonl"

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

    kubectl get configmap,secret,serviceaccount,role,rolebinding,deployment,statefulset,daemonset,cronjob,service,ingress,pvc -n migration-apps -o json \
      | canonicalize_api_list > "$stage_dir/application.json"
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
      --slurpfile migratable_objects "$stage_dir/migratable-objects.jsonl" \
      '{nodes:$nodes[0], application:$app[0], persistentVolumes:$pvs[0],
        certificate:$certificate[0], issuer:$issuer[0],
        certManager:$cert_manager[0], traefik:$traefik[0], cilium:$cilium[0],
        requiredCrds:$crds[0], storageClass:$storageclass[0],
        migratableObjects:$migratable_objects}' \
      > "$stage_dir/semantic-state.json"
    chmod 0600 "$stage_dir"/*.json "$stage_dir"/*.jsonl "$stage_dir"/*.sha256
}

capture_migratable_api_objects() {
    local output="$1"
    local resources resource list_json object_json normalized identity digest
    resources="$(kubectl api-resources --verbs=list -o name | LC_ALL=C sort -u)"
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
            identity="$(jq -cS '{apiVersion, kind, namespace: (.metadata.namespace // ""), name: .metadata.name}' <<< "$normalized")"
            digest="$(printf '%s' "$normalized" | sha256sum | awk '{print $1}')"
            jq -cS -n --argjson identity "$identity" --arg digest "$digest" \
                '{identity: $identity, sha256: $digest}' >> "$output"
        done < <(jq -c '.items[]' <<< "$list_json")
    done <<< "$resources"
    LC_ALL=C sort -o "$output" "$output"
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
    if ! cmp -s "$initial/semantic-state.json" "$returned/semantic-state.json"; then
        echo "Returned Kubernetes semantic state differs from the source checkpoint" >&2
        diff -u "$initial/semantic-state.json" "$returned/semantic-state.json" || true
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
    echo "PASS: returned semantic state, text and binary ConfigMaps, Secret digests, certificate secret, and PVC data match the source checkpoint"
}

assert_migratable_api_objects_retained() {
    local before="$CHECKPOINT_DIR/$1/migratable-objects.jsonl"
    local after="$CHECKPOINT_DIR/$2/migratable-objects.jsonl"
    local before_ids="$CHECKPOINT_DIR/$1/migratable-object-identities.txt"
    local after_ids="$CHECKPOINT_DIR/$2/migratable-object-identities.txt"
    local missing_ids="$CHECKPOINT_DIR/$2/missing-source-object-identities.txt"
    jq -cS '.identity' "$before" | LC_ALL=C sort -u > "$before_ids"
    jq -cS '.identity' "$after" | LC_ALL=C sort -u > "$after_ids"
    comm -23 "$before_ids" "$after_ids" > "$missing_ids"
    if [[ -s "$missing_ids" ]]; then
        echo "Source Kubernetes API objects are missing at stage $2" >&2
        cat "$missing_ids" >&2
        return 1
    fi

    # These fixture resources have stable, explicit durable-state snapshots;
    # compare them directly. The global inventory intentionally checks source
    # identity retention, since API defaulting, node replacement, controllers,
    # and Cilium create or update legitimate destination runtime state.
    for snapshot in application.json certificate.json issuer.json required-crds.json storageclass.json \
        certificate-secret.sha256 user-configmap-data.sha256 user-binary-configmap-data.sha256 \
        user-immutable-configmap.sha256 user-secret-data.sha256; do
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
    install_workloads
    verify_stage source "$SOURCE_KUBECONFIG"

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
    watch_target_forward_state "$nodestore_kubeconfig" \
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
    KUBECONFIG="$nodestore_kubeconfig" install_hostpath_driver /var/lib/nodelet true
    verify_stage nodestore "$nodestore_kubeconfig"
    assert_migratable_api_objects_retained source nodestore

    MIGRATION_STARTED_AT="$(date -u --iso-8601=seconds)"
    echo "Migrating nodestore -> $SOURCE_DIST"
    NODEMIGRATE_REPLACE_NODE=true \
    NODEMIGRATE_SOURCE_KUBECONFIG="$nodestore_kubeconfig" \
    NODEMIGRATE_DESTINATION_KUBECONFIG="$SOURCE_KUBECONFIG" \
        "$MIGRATE" "to=$SOURCE_DIST" from=nodestore
    CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
    export KUBECONFIG="$SOURCE_KUBECONFIG"
    KUBECONFIG="$SOURCE_KUBECONFIG" install_hostpath_driver /var/lib/kubelet true
    verify_stage returned "$SOURCE_KUBECONFIG"
    assert_round_trip_unchanged
}

if [[ "$LIBRARY_MODE" != true ]]; then
    main "$@"
fi
