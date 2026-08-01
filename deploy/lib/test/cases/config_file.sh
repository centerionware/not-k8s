# lib/test/cases/config_file.sh — NODELET_CONFIG_FILE/NODELET_CONFIG_DIR
# (round 94): a YAML file (or drop-in directory of them) mapping the same
# NODELET_* keys the process environment already reads, loaded once at
# Config::from_env()'s very start (config.rs::apply_config_file_env()).
# Real environment variables always win over the file, matching kubelet's
# own flag-beats-config-file precedence.
#
# The pure parsing/merge logic (parse_config_yaml(), merge_config_layers())
# has full unit-test coverage in config.rs's own tests_config_file module.
# What's NOT automatable here: this suite runs against an already-running
# nodelet process and has no way to change its environment or restart it
# (same limitation TEST_CPU_MANAGER_STATIC=true and similar opt-in
# NODELET_* settings already carry) — so this is a manual spot-check, not
# a skip due to missing infrastructure otherwise available.

test_config_file_manual_note() {
    skip_test "NODELET_CONFIG_FILE/NODELET_CONFIG_DIR (round 94) load NODELET_* settings from a YAML file instead of the environment — the pure parsing/precedence logic is fully unit-tested (config.rs's tests_config_file module), but exercising this for real needs controlling nodelet's own startup environment, which this e2e suite (running against an already-started nodelet) can't do. Manual spot-check: write a YAML file (e.g. /etc/nodelet/config.yaml) with 'NODELET_MAX_PODS: 42', set NODELET_CONFIG_FILE=/etc/nodelet/config.yaml in nodelet's own environment (systemd unit / docker-compose / etc.), restart nodelet, and confirm 'kubectl get node <node> -o jsonpath={.status.capacity.pods}' reports 42. Then set NODELET_MAX_PODS=10 directly in the environment alongside the same config file and confirm the explicit environment variable wins (reports 10, not 42) — proving the precedence order, not just that the file loads at all. Repeat with a NODELET_CONFIG_DIR of two files (e.g. 00-base.yaml, 01-override.yaml both setting the same key) to confirm the later filename wins."
}

register_test test_config_file_manual_note
