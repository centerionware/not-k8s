"""Permission coverage helpers for migrated Kubernetes default RBAC rules."""


def _wildcard_covers(pattern, value):
    return pattern == "*" or pattern == value


def _resource_covers(pattern, value):
    if pattern == "*":
        return True
    if pattern.endswith("/*"):
        return value.startswith(pattern[:-1])
    return pattern == value


def _url_covers(pattern, value):
    if pattern == "*" or pattern == value:
        return True
    return pattern.endswith("*") and value.startswith(pattern[:-1])


def _api_permission_covered(source, targets):
    groups = source.get("apiGroups") or []
    resources = source.get("resources") or []
    verbs = source.get("verbs") or []
    names = source.get("resourceNames") or []
    for group in groups:
        for resource in resources:
            for verb in verbs:
                for name in names or [None]:
                    if not any(
                        any(_wildcard_covers(candidate, group) for candidate in target.get("apiGroups", []))
                        and any(_resource_covers(candidate, resource) for candidate in target.get("resources", []))
                        and any(_wildcard_covers(candidate, verb) for candidate in target.get("verbs", []))
                        and (
                            name is None and not target.get("resourceNames")
                            or name is not None
                            and (not target.get("resourceNames") or name in target["resourceNames"])
                        )
                        for target in targets
                    ):
                        return False
    return True


def missing_source_rules(source_rules, target_rules):
    """Return source rules whose complete permission set is not granted."""
    missing = []
    for source in source_rules:
        if source.get("nonResourceURLs"):
            covered = True
            for url in source["nonResourceURLs"]:
                for verb in source.get("verbs", []):
                    if not any(
                        any(_url_covers(candidate, url) for candidate in target.get("nonResourceURLs", []))
                        and any(_wildcard_covers(candidate, verb) for candidate in target.get("verbs", []))
                        for target in target_rules
                    ):
                        covered = False
        else:
            covered = _api_permission_covered(source, target_rules)
        if not covered:
            missing.append(source)
    return missing


def source_rules_preserved(source_rules, target_rules):
    """Return whether target RBAC grants cover every source permission."""
    return not missing_source_rules(source_rules, target_rules)


def _self_test():
    source = [{"apiGroups": ["", "events.k8s.io"], "resources": ["events"], "verbs": ["create", "patch"]}]
    assert source_rules_preserved(source, [{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}])
    assert source_rules_preserved(source, [
        {"apiGroups": [""], "resources": ["events"], "verbs": ["create", "patch"]},
        {"apiGroups": ["events.k8s.io"], "resources": ["events"], "verbs": ["create", "patch"]},
    ])
    assert not source_rules_preserved(source, [{"apiGroups": [""], "resources": ["events"], "verbs": ["get"]}])
    named = [{"apiGroups": [""], "resources": ["secrets"], "resourceNames": ["a"], "verbs": ["get"]}]
    assert source_rules_preserved(named, [{"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"]}])
    assert not source_rules_preserved(
        [{"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"]}],
        [{"apiGroups": [""], "resources": ["secrets"], "resourceNames": ["a"], "verbs": ["get"]}],
    )
    assert source_rules_preserved(
        [{"nonResourceURLs": ["/api", "/apis/apps/v1/*"], "verbs": ["get"]}],
        [{"nonResourceURLs": ["/api", "/apis/*"], "verbs": ["get"]}],
    )
    assert not source_rules_preserved(
        [{"nonResourceURLs": ["/api"], "verbs": ["get"]}],
        [{"nonResourceURLs": ["/apis/*"], "verbs": ["get"]}],
    )


if __name__ == "__main__":
    _self_test()
    print("PASS: default RBAC permission coverage")
