#!/usr/bin/env python3
"""Keep the standalone migration binary on release paths, outside notk8s."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


release = (ROOT / ".github/workflows/release.yml").read_text()
build = (ROOT / ".github/workflows/build.yml").read_text()
checks = (ROOT / ".github/workflows/nodemigrate.yml").read_text()
publisher = (ROOT / ".github/workflows/nodemigrate-release.yml").read_text()
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
require("needs.detect.outputs.crate_exists == 'true'" in checks,
        "crate tests must be gated by the checked-out manifest")
require("hashFiles(" not in checks,
        "hashFiles is not available in a job-level condition")
require("releases/latest" in publisher,
        "standalone publication must read the latest regular release version")
require('tag=nodemigrate-v$version' in publisher,
        "standalone publication must use its own tag at the regular release version")
require('gh release create "$RELEASE_TAG" --target "$GITHUB_SHA"' in publisher,
        "standalone publication must target the migration build without advancing VERSION")
require("Advance VERSION" not in publisher and "/contents/VERSION" not in publisher,
        "standalone publication must not mutate the shared VERSION branch")

print("nodemigrate release hooks are present and remain standalone")
