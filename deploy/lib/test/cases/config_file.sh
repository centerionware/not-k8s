# lib/test/cases/config_file.sh — NODELET_CONFIG_FILE/NODELET_CONFIG_DIR
# (round 94): a YAML file (or drop-in directory of them) mapping the same
# NODELET_* keys the process environment already reads, loaded once at
# Config::from_env()'s very start (config.rs::apply_config_file_env()).
# Real environment variables always win over the file, matching kubelet's
# own flag-beats-config-file precedence.
#
# The pure parsing/merge logic (parse_config_yaml(), merge_config_layers())
# has full unit-test coverage in config.rs's own tests_config_file module.
# Round 123: the "this suite has no way to restart nodelet with a
# different startup environment" limitation these tests used to note is
# gone — nodelet_restart_with_env (nodelet_env.sh) does exactly that.

test_config_file_sets_a_value_env_did_not_override() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CONFIG_FILE set"; fi

    local config_file
    config_file="$(mktemp /tmp/nodelet-config-file-test.XXXXXX.yaml)"
    echo "NODELET_MAX_PODS: 42" > "$config_file"

    config_file_test_cleanup() {
        nodelet_restore_env
        rm -f "${config_file:-}"
    }
    trap config_file_test_cleanup EXIT

    nodelet_restart_with_env "NODELET_CONFIG_FILE=$config_file"

    local pods
    pods="$(kubectl get node "$(node_name)" -o jsonpath='{.status.capacity.pods}')"
    assert_eq "$pods" "42" "Node.status.capacity.pods should reflect NODELET_MAX_PODS loaded from NODELET_CONFIG_FILE"
}

test_config_file_precedence_a_real_env_var_still_wins() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CONFIG_FILE + an env override set"; fi

    local config_file
    config_file="$(mktemp /tmp/nodelet-config-file-precedence-test.XXXXXX.yaml)"
    echo "NODELET_MAX_PODS: 42" > "$config_file"

    config_file_precedence_test_cleanup() {
        nodelet_restore_env
        rm -f "${config_file:-}"
    }
    trap config_file_precedence_test_cleanup EXIT

    # Same file as above, but this time a real environment variable is
    # ALSO set for the same key — matching kubelet's own
    # flag-beats-config-file precedence, the actual explicit env var must
    # win over the file, not just "the file loads at all".
    nodelet_restart_with_env "NODELET_CONFIG_FILE=$config_file" "NODELET_MAX_PODS=10"

    local pods
    pods="$(kubectl get node "$(node_name)" -o jsonpath='{.status.capacity.pods}')"
    assert_eq "$pods" "10" "a real NODELET_MAX_PODS environment variable should win over the same key set in NODELET_CONFIG_FILE"
}

test_config_dir_merges_files_in_filename_order() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_CONFIG_DIR set"; fi

    local config_dir
    config_dir="$(mktemp -d /tmp/nodelet-config-dir-test.XXXXXX)"
    echo "NODELET_MAX_PODS: 42" > "$config_dir/00-base.yaml"
    echo "NODELET_MAX_PODS: 77" > "$config_dir/01-override.yaml"

    config_dir_test_cleanup() {
        nodelet_restore_env
        rm -rf "${config_dir:-}"
    }
    trap config_dir_test_cleanup EXIT

    nodelet_restart_with_env "NODELET_CONFIG_DIR=$config_dir"

    local pods
    pods="$(kubectl get node "$(node_name)" -o jsonpath='{.status.capacity.pods}')"
    assert_eq "$pods" "77" "NODELET_CONFIG_DIR should merge its files in filename sort order, later file wins (01-override.yaml over 00-base.yaml)"
}

register_test test_config_file_sets_a_value_env_did_not_override
register_test test_config_file_precedence_a_real_env_var_still_wins
register_test test_config_dir_merges_files_in_filename_order
