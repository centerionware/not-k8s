# lib/test/cases/batch_controllers.sh — nodecontroller's Group F: batch
# controllers (job-controller, cronjob-controller, ttl-after-finished-controller).

_nodecontroller_is_running_bc() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet nodecontroller 2>/dev/null && return 0
    fi
    pgrep -x nodecontroller >/dev/null 2>&1
}

_require_nodecontroller_bc() {
    _nodecontroller_is_running_bc \
        || skip_test "nodecontroller isn't running here — deploy with --controller-manager=nodecontroller (which also disables k3s's own controller manager) to exercise these"
}

test_job_controller_runs_pods_to_completion() {
    _require_nodecontroller_bc
    local job="job-test"

    apply_manifest <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $job
spec:
  completions: 2
  parallelism: 2
  backoffLimit: 2
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sh", "-c", "exit 0"]
EOF
    trap 'kctl delete job "$job" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 90 "job $job reports 2 succeeded pods" \
        bash -c "[[ \"\$(kctl get job '$job' -o jsonpath='{.status.succeeded}')\" == '2' ]]"

    wait_until 30 "job $job reports Complete condition" \
        bash -c "kctl get job '$job' -o jsonpath='{.status.conditions[?(@.type==\"Complete\")].status}' | grep -q True"

    trap - EXIT
    kctl delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
}

test_job_controller_fails_after_backoff_limit() {
    _require_nodecontroller_bc
    local job="job-fail-test"

    apply_manifest <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $job
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sh", "-c", "exit 1"]
EOF
    trap 'kctl delete job "$job" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 90 "job $job reports Failed condition after exhausting backoffLimit" \
        bash -c "kctl get job '$job' -o jsonpath='{.status.conditions[?(@.type==\"Failed\")].status}' 2>/dev/null | grep -q True"

    trap - EXIT
    kctl delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
}

test_cronjob_controller_creates_a_job_on_schedule() {
    _require_nodecontroller_bc
    local cj="cronjob-test"

    apply_manifest <<EOF
apiVersion: batch/v1
kind: CronJob
metadata:
  name: $cj
spec:
  schedule: "* * * * *"
  concurrencyPolicy: Allow
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers:
            - name: busybox
              image: busybox:latest
              command: ["sh", "-c", "exit 0"]
EOF
    trap 'kctl delete cronjob "$cj" --ignore-not-found >/dev/null 2>&1 || true; kctl delete jobs -l "cronjob-name=$cj" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    # Schedule is every minute — allow up to just over two minutes for the
    # first boundary to land plus reconcile latency.
    wait_until 150 "cronjob $cj creates a Job on its schedule" \
        bash -c "[[ \"\$(kctl get jobs -l 'cronjob-name=$cj' --no-headers 2>/dev/null | wc -l | tr -d ' ')\" -ge '1' ]]"

    wait_until 30 "cronjob $cj reports lastScheduleTime" \
        bash -c "[[ -n \"\$(kctl get cronjob '$cj' -o jsonpath='{.status.lastScheduleTime}')\" ]]"

    trap - EXIT
    kctl delete cronjob "$cj" --ignore-not-found >/dev/null 2>&1 || true
    kctl delete jobs -l "cronjob-name=$cj" --ignore-not-found >/dev/null 2>&1 || true
}

test_ttl_after_finished_controller_deletes_expired_jobs() {
    _require_nodecontroller_bc
    local job="ttl-test"

    apply_manifest <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $job
spec:
  ttlSecondsAfterFinished: 5
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: busybox
          image: busybox:latest
          command: ["sh", "-c", "exit 0"]
EOF
    trap 'kctl delete job "$job" --ignore-not-found >/dev/null 2>&1 || true' EXIT

    wait_until 90 "job $job reports Complete condition" \
        bash -c "kctl get job '$job' -o jsonpath='{.status.conditions[?(@.type==\"Complete\")].status}' 2>/dev/null | grep -q True"

    wait_until 60 "ttl-after-finished-controller deletes job $job past its TTL" \
        bash -c "! kctl get job '$job' >/dev/null 2>&1"

    trap - EXIT
}

register_test test_job_controller_runs_pods_to_completion
register_test test_job_controller_fails_after_backoff_limit
register_test test_cronjob_controller_creates_a_job_on_schedule
register_test test_ttl_after_finished_controller_deletes_expired_jobs
