#!/usr/bin/env python3
"""Canonicalization for migration API inventory comparisons."""

import sys


def normalize_api_object(value):
    """Normalize only API fields with Kubernetes-generated representation changes."""
    result = dict(value)
    if result.get("kind") == "ControllerRevision":
        # Controllers assign this sequence number and can advance it when they
        # recreate rollout history during migration. Keep the payload and every
        # other source field under strict parity comparison.
        result.pop("revision", None)
    return result


def _self_test():
    first = {
        "kind": "CustomResourceDefinition",
        "spec": {"versions": [{"schema": {"maximum": 9223372036854775000}}]},
    }
    second = {
        "kind": "CustomResourceDefinition",
        "spec": {"versions": [{"schema": {"maximum": 9223372036854776000}}]},
    }
    assert normalize_api_object(first) != normalize_api_object(second)
    ordinary = {"kind": "ConfigMap", "value": 9223372036854775000}
    assert normalize_api_object(ordinary) == ordinary
    original = {"kind": "ControllerRevision", "revision": 2, "data": {"template": "same"}}
    advanced = {"kind": "ControllerRevision", "revision": 3, "data": {"template": "same"}}
    assert normalize_api_object(original) == normalize_api_object(advanced)
    changed_payload = {**advanced, "data": {"template": "changed"}}
    assert normalize_api_object(advanced) != normalize_api_object(changed_payload)
    print("nodemigrate API inventory canonicalization checks passed")


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        raise SystemExit("usage: nodemigrate_api_inventory.py --self-test")
    _self_test()
