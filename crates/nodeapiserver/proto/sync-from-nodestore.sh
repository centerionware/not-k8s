#!/usr/bin/env bash
# sync-from-nodestore.sh — copies nodestore's already-vendored, already-
# stripped etcd v3 protos (kv.proto, auth.proto, rpc.proto) into this
# directory verbatim.
#
# Why a second copy instead of a crate dependency on nodestore: dependency
# discipline (see docs/APISERVER.md and crates/nodeproxy/Cargo.toml's own
# comment) — nodeapiserver must not depend on nodestore, it speaks etcd v3
# over the wire like any other client. But re-fetching and re-stripping
# from etcd's own upstream repo here too (nodestore/proto/vendor.sh's job)
# would mean two independent places that can drift on which etcd version is
# targeted. Since the wire format has to be byte-identical either way (both
# crates are compiled against literally the same etcd v3 API), copying
# nodestore's already-vendored files is the one honest way to keep that
# guarantee without a crate dependency: same content, no shared code.
#
# Run this after nodestore/proto/vendor.sh moves to a new etcd version.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$SCRIPT_DIR/../../nodestore/proto"

for f in kv.proto auth.proto rpc.proto; do
    cp "$SRC/$f" "$SCRIPT_DIR/$f"
    echo "==> synced $f"
done
