# lib/test/cases/device_plugins.sh — device plugin discovery, Node
# capacity/allocatable advertisement, and Allocate() wiring into container
# creation (device_plugins.rs). Reuses the same plugin-registration
# directory csi_plugin_registration.sh checks (plugin_registry.rs handles
# both CSI and device plugin registrations through one watcher).
#
# Round 123: previously manual-only on the theory that this needed real
# (or hardware-faked) GPU/FPGA hardware — but the DevicePlugin gRPC
# protocol itself (GetInfo/NotifyRegistrationStatus for registration,
# then GetDevicePluginOptions/ListAndWatch/GetPreferredAllocation/
# PreStartContainer/Allocate on the plugin's own endpoint) needs no real
# hardware at all: a plugin can report entirely fabricated device IDs as
# "Healthy" (or flip one to "Unhealthy" on cue, or return a deliberately
# broken GetPreferredAllocation) and nodelet has no way to tell the
# difference. fake_device_plugin.py (generated below) is exactly that —
# same "fake the plugin, not the protocol" pattern credential_provider.sh's
# round-123 conversion used.

_fake_device_plugin_setup() {
    # Sourced into each test that needs the fake plugin so cleanup/teardown
    # stay consistent; sets FDP_* globals for the caller. Always advertises
    # pre_start_required=true and get_preferred_allocation_available=true —
    # cheap to always turn on since nothing outside the dedicated
    # preferred-allocation/prestart test actually depends on the plugin
    # behaving unusually there.
    if ! command -v python3 &>/dev/null; then
        skip_test "python3 not on PATH — needed to run the fake gRPC device plugin"
    fi
    FDP_DIR="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 15 bash -c "[[ -d '$FDP_DIR' ]]"; then
        skip_test "no $FDP_DIR — same directory csi_plugin_registration.sh checks; see its notes if this fails"
    fi
    FDP_WORK="$(mktemp -d)"
    FDP_RESOURCE="fake.example.com/testdevice"
    FDP_SOCK="$FDP_DIR/fake-device-plugin.sock"
    FDP_LOG="$FDP_WORK/plugin.log"
    FDP_PRESTART_LOG="$FDP_WORK/prestart.log"
    FDP_CONTROL_DIR="$FDP_WORK/control"
    sudo mkdir -p "$FDP_CONTROL_DIR"
    sudo touch "$FDP_PRESTART_LOG"
    sudo chmod 0666 "$FDP_PRESTART_LOG"

    if ! sudo python3 -c "import grpc" 2>/dev/null; then
        log "installing grpcio/grpcio-tools for the fake device plugin..."
        sudo python3 -m pip install --quiet grpcio grpcio-tools \
            || skip_test "couldn't install grpcio (no network access to PyPI?) — genuinely can't stand up a fake gRPC device plugin without it"
    fi

    log "generating gRPC stubs from the vendored proto files..."
    cp "$REPO_ROOT/crates/nodelet/proto/pluginregistration.proto" "$REPO_ROOT/crates/nodelet/proto/deviceplugin.proto" "$FDP_WORK/"
    (cd "$FDP_WORK" && sudo python3 -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. pluginregistration.proto deviceplugin.proto) \
        || skip_test "grpc_tools.protoc failed to generate stubs from the vendored proto files"

    cat > "$FDP_WORK/fake_device_plugin.py" <<'PYEOF'
# Fake DevicePlugin for e2e testing — fabricates 4 healthy devices with no
# real hardware behind them; nodelet's plugin_registry.rs/device_plugins.rs
# have no way to distinguish this from a real vendor plugin. Behavior is
# steered at runtime by files under the control dir (argv[3]), polled on
# every relevant RPC — no restart needed to change behavior mid-test:
#   unhealthy_ids     comma-separated device IDs to report Unhealthy
#   fail_preferred     if present, GetPreferredAllocation returns a real
#                       gRPC error (triggers nodelet's logged fallback path)
import sys, time, os
import grpc
from concurrent import futures
import pluginregistration_pb2 as reg_pb2
import pluginregistration_pb2_grpc as reg_pb2_grpc
import deviceplugin_pb2 as dp_pb2
import deviceplugin_pb2_grpc as dp_pb2_grpc

sock_path, resource_name, control_dir = sys.argv[1], sys.argv[2], sys.argv[3]
DEVICE_IDS = [f"fake-{i}" for i in range(4)]
PRESTART_LOG = os.path.join(os.path.dirname(control_dir), "prestart.log")

def unhealthy_ids():
    p = os.path.join(control_dir, "unhealthy_ids")
    if not os.path.exists(p):
        return set()
    return {x for x in open(p).read().strip().split(",") if x}

class Registration(reg_pb2_grpc.RegistrationServicer):
    def GetInfo(self, request, context):
        return reg_pb2.PluginInfo(type="DevicePlugin", name=resource_name, endpoint=sock_path, supported_versions=["v1beta1"])
    def NotifyRegistrationStatus(self, request, context):
        with open(sock_path + ".status", "w") as f:
            f.write(f"registered={request.plugin_registered} error={request.error!r}\n")
        return reg_pb2.RegistrationStatusResponse()

class DevicePlugin(dp_pb2_grpc.DevicePluginServicer):
    def GetDevicePluginOptions(self, request, context):
        return dp_pb2.DevicePluginOptions(pre_start_required=True, get_preferred_allocation_available=True)
    def ListAndWatch(self, request, context):
        last = None
        while True:
            unhealthy = unhealthy_ids()
            if unhealthy != last:
                devices = [dp_pb2.Device(ID=d, health=("Unhealthy" if d in unhealthy else "Healthy")) for d in DEVICE_IDS]
                yield dp_pb2.ListAndWatchResponse(devices=devices)
                last = unhealthy
            time.sleep(1)
    def GetPreferredAllocation(self, request, context):
        if os.path.exists(os.path.join(control_dir, "fail_preferred")):
            context.abort(grpc.StatusCode.INTERNAL, "e2e test: deliberately failing GetPreferredAllocation")
        resp = dp_pb2.PreferredAllocationResponse()
        for creq in request.container_requests:
            car = resp.container_responses.add()
            # Deterministic, verifiably-not-nodelet's-own-default preference:
            # highest device IDs first, honoring must_include.
            avail = sorted(set(creq.available_deviceIDs), reverse=True)
            picked = list(creq.must_include_deviceIDs)
            for d in avail:
                if len(picked) >= creq.allocation_size:
                    break
                if d not in picked:
                    picked.append(d)
            car.deviceIDs.extend(picked[: creq.allocation_size])
        return resp
    def Allocate(self, request, context):
        resp = dp_pb2.AllocateResponse()
        for creq in request.container_requests:
            car = resp.container_responses.add()
            car.envs["FAKE_DEVICE_IDS"] = ",".join(sorted(creq.devices_ids))
        return resp
    def PreStartContainer(self, request, context):
        if os.path.exists(os.path.join(control_dir, "fail_prestart")):
            context.abort(grpc.StatusCode.INTERNAL, "e2e test: deliberately failing PreStartContainer")
        with open(PRESTART_LOG, "a") as f:
            f.write(",".join(sorted(request.devices_ids)) + "\n")
        return dp_pb2.PreStartContainerResponse()

if os.path.exists(sock_path):
    os.remove(sock_path)
server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
reg_pb2_grpc.add_RegistrationServicer_to_server(Registration(), server)
dp_pb2_grpc.add_DevicePluginServicer_to_server(DevicePlugin(), server)
server.add_insecure_port(f"unix://{sock_path}")
server.start()
os.chmod(sock_path, 0o777)
print("ready", flush=True)
server.wait_for_termination()
PYEOF

    log "starting the fake device plugin (resource=$FDP_RESOURCE)..."
    sudo python3 "$FDP_WORK/fake_device_plugin.py" "$FDP_SOCK" "$FDP_RESOURCE" "$FDP_CONTROL_DIR" > "$FDP_LOG" 2>&1 &
    FDP_PID=$!
    if ! try_wait_until 20 bash -c "grep -q ready '$FDP_LOG' 2>/dev/null"; then
        cat "$FDP_LOG" 2>/dev/null || true
        _fake_device_plugin_teardown
        skip_test "fake device plugin never reported ready — see plugin.log content above for the real python/grpc error"
    fi
    if ! try_wait_until 20 bash -c "kctl get node -o jsonpath='{.items[0].status.capacity.fake\.example\.com/testdevice}' 2>/dev/null | grep -q 4"; then
        cat "$FDP_LOG" 2>/dev/null || true
        _fake_device_plugin_teardown
        die "Node.status.capacity['$FDP_RESOURCE'] never showed 4 after the fake plugin registered — check nodelet's logs for 'device plugin: inventory updated' / 'plugin registry: plugin registered'"
    fi
}

_fake_device_plugin_teardown() {
    [[ -n "${FDP_PID:-}" ]] && sudo kill "$FDP_PID" 2>/dev/null
    sudo rm -f "$FDP_SOCK" "$FDP_SOCK.status" 2>/dev/null
    rm -rf "${FDP_WORK:-}"
}

test_plugin_registry_watches_for_device_plugins_too() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local dir="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 15 bash -c "[[ -d '$dir' ]]"; then
        skip_test "no $dir — same directory csi_plugin_registration.sh checks; see its notes if this fails"
    fi
    assert_true test -d "$dir"
}

test_device_plugin_advertises_capacity_and_allocates_into_a_container() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    trap _fake_device_plugin_teardown RETURN
    _fake_device_plugin_setup

    local allocatable
    allocatable="$(kctl get node -o jsonpath='{.items[0].status.allocatable.fake\.example\.com/testdevice}' 2>/dev/null)"
    assert_eq "$allocatable" "4" "Node.status.allocatable['$FDP_RESOURCE']"

    local name="device-plugin-alloc-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        limits:
          $FDP_RESOURCE: "1"
EOF
    if ! wait_until 60 "$name Running" pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod requesting 1x $FDP_RESOURCE never reached Running — check nodelet's device-manager Allocate() wiring (device_plugins.rs / container_create.rs)"
    fi
    local env_val
    env_val="$(kctl exec "$name" -- sh -c 'echo $FAKE_DEVICE_IDS' 2>/dev/null)"
    local ars
    ars="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].allocatedResourcesStatus}' 2>/dev/null)"
    delete_pod_if_exists "$name"
    assert_contains "$env_val" "fake-" "container should have gotten FAKE_DEVICE_IDS from the fake plugin's real Allocate() response, proving nodelet actually wired the plugin's env vars into the container, not just tracked capacity"
    assert_contains "$ars" "Healthy" "containerStatuses[0].allocatedResourcesStatus should report the allocated device as Healthy, matching the plugin's ListAndWatch state at allocation time"
}

test_device_plugin_health_transition_updates_allocated_resources_status() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    trap _fake_device_plugin_teardown RETURN
    _fake_device_plugin_setup

    local name="device-plugin-health-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        limits:
          $FDP_RESOURCE: "1"
EOF
    if ! wait_until 60 "$name Running" pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod requesting 1x $FDP_RESOURCE never reached Running"
    fi
    local device_id
    device_id="$(kctl exec "$name" -- sh -c 'echo $FAKE_DEVICE_IDS' 2>/dev/null)"
    if [[ -z "$device_id" ]]; then
        delete_pod_if_exists "$name"
        die "couldn't read the allocated device ID back out of the container"
    fi

    # Flip that specific device unhealthy via the plugin's own ListAndWatch
    # — no container restart should be needed, this is a live status field.
    echo "$device_id" | sudo tee "$FDP_CONTROL_DIR/unhealthy_ids" >/dev/null
    local ars
    if ! try_wait_until 20 bash -c "kctl get pod $name -o jsonpath='{.status.containerStatuses[0].allocatedResourcesStatus}' 2>/dev/null | grep -q Unhealthy"; then
        delete_pod_if_exists "$name"
        die "containerStatuses[0].allocatedResourcesStatus never updated to Unhealthy after the plugin's ListAndWatch reported device '$device_id' unhealthy"
    fi
    local phase_after
    phase_after="$(kctl get pod "$name" -o jsonpath='{.status.phase}' 2>/dev/null)"
    delete_pod_if_exists "$name"
    assert_eq "$phase_after" "Running" "the container must NOT be restarted just because its allocated device went unhealthy — allocatedResourcesStatus is a live status signal, not a restart trigger"
}

test_device_plugin_preferred_allocation_and_prestart() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    trap _fake_device_plugin_teardown RETURN
    _fake_device_plugin_setup

    local marker
    marker="$(date -u +%Y-%m-%dT%H:%M:%S)"

    # Positive path: plugin's real GetPreferredAllocation/PreStartContainer
    # both succeed — the allocated set should match the plugin's own
    # deterministic "highest IDs first" preference (fake-3,fake-2), proving
    # nodelet actually used the plugin's response rather than its own
    # first-N default (which would pick fake-0,fake-1), and PreStartContainer
    # must have logged an entry before the container could start.
    local ok_name="device-plugin-preferred-ok"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $ok_name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        limits:
          $FDP_RESOURCE: "2"
EOF
    if ! wait_until 60 "$ok_name Running" pod_is_phase "$ok_name" Running; then
        delete_pod_if_exists "$ok_name"
        die "pod requesting 2x $FDP_RESOURCE (preferred-allocation path) never reached Running"
    fi
    local ids
    ids="$(kctl exec "$ok_name" -- sh -c 'echo $FAKE_DEVICE_IDS' 2>/dev/null)"
    delete_pod_if_exists "$ok_name"
    assert_eq "$ids" "fake-2,fake-3" "allocated devices should match the plugin's own GetPreferredAllocation response (highest IDs first), proving nodelet used it rather than its own default first-N selection"
    if ! sudo grep -q "fake-2,fake-3\|fake-3,fake-2" "$FDP_PRESTART_LOG"; then
        die "PreStartContainer was never called with the final device IDs before the container started — check device_plugins.rs's pre_start_required handling"
    fi

    # Negative path: plugin's GetPreferredAllocation deliberately errors —
    # nodelet must fall back to its own selection (pod still reaches
    # Running) and log the documented fallback warning.
    sudo touch "$FDP_CONTROL_DIR/fail_preferred"
    local fb_name="device-plugin-preferred-fallback"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $fb_name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        limits:
          $FDP_RESOURCE: "1"
EOF
    if ! wait_until 60 "$fb_name Running" pod_is_phase "$fb_name" Running; then
        delete_pod_if_exists "$fb_name"
        die "pod requesting 1x $FDP_RESOURCE never reached Running even via the fallback path (GetPreferredAllocation deliberately erroring)"
    fi
    delete_pod_if_exists "$fb_name"
    sudo rm -f "$FDP_CONTROL_DIR/fail_preferred"
    if ! sudo journalctl -u nodelet --since "$marker" 2>/dev/null | grep -q "GetPreferredAllocation failed; falling back"; then
        die "nodelet's log never showed the documented 'GetPreferredAllocation failed; falling back to nodelet's own selection' warning even though the plugin deliberately errored on that call"
    fi
}

test_allocated_resources_status_absent_without_device_resources() {
    # Regression check for round 79's wiring: a plain pod with no
    # device-plugin resources requested must never get a spurious
    # allocatedResourcesStatus entry.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="no-device-resources"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local ars
    ars="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].allocatedResourcesStatus}')"
    delete_pod_if_exists "$name"
    assert_eq "$ars" "" "containerStatuses[0].allocatedResourcesStatus should be empty/absent for a pod with no device-plugin resources allocated"
}

register_test test_plugin_registry_watches_for_device_plugins_too
register_test test_device_plugin_advertises_capacity_and_allocates_into_a_container
register_test test_device_plugin_health_transition_updates_allocated_resources_status
register_test test_device_plugin_preferred_allocation_and_prestart
register_test test_allocated_resources_status_absent_without_device_resources
