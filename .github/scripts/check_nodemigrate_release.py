#!/usr/bin/env python3
"""Keep the standalone migration binary on release paths, outside notk8s."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


release = (ROOT / ".github/workflows/release.yml").read_text()
build = (ROOT / ".github/workflows/build.yml").read_text()
combined = (ROOT / "crates/notk8s/Cargo.toml").read_text()

# The migration crate lands on its own branch later. Keep these hooks guarded
# so this prerequisite CI branch remains buildable before that merge.
require('cargo test -p nodemigrate' in release, "release tests omit nodemigrate")
require('cargo build --release -p nodemigrate' in release,
        "release assets omit a standalone nodemigrate build")
require('cargo test -p nodemigrate' in build, "build workflow omits nodemigrate tests")
require('cargo build $1 -p nodemigrate' in build,
        "build workflow omits nodemigrate artifacts")
require("nodebootstrap notk8s nodemigrate" in release,
        "release publication does not rename nodemigrate assets")
require("nodebootstrap notk8s nodemigrate" in build,
        "build workflow does not stage nodemigrate artifacts")
require("nodemigrate =" not in combined,
        "nodemigrate must remain independent from the combined binary")

print("nodemigrate release hooks are present and remain standalone")
