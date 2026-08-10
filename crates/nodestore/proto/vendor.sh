#!/usr/bin/env bash
# vendor.sh — re-fetch etcd's own .proto files and strip them down to
# something protoc can compile without etcd's Go build environment.
#
# Why vendored at all: nodestore has to be wire-compatible with etcd v3,
# because its only real client is a real kube-apiserver, which will not
# negotiate. Field numbers, message shapes and service/method names must
# match exactly — so these come from etcd's own repo at a pinned tag rather
# than being hand-reconstructed from the documentation. (Same reasoning as
# deploy/lib/e2e-full-setup.sh fetching the real upstream CSI/DRA driver
# tooling instead of a hand-written copy of it.)
#
# Why stripped: upstream's protos carry two sets of annotations that are
# meaningless here and would otherwise drag in their own proto
# dependencies —
#
#   * gogoproto (`option (gogoproto.*)`, `[(gogoproto.nullable) = false]`)
#     — codegen hints for Go's gogo/protobuf fork. No effect on the wire
#     format, and prost has no equivalent.
#   * google.api.http (`option (google.api.http) = {...}`) — grpc-gateway's
#     REST mapping. etcd uses it to expose a JSON gateway; nodestore serves
#     gRPC only.
#
# Neither changes a single byte on the wire, which is what makes stripping
# them safe. Everything else — every field number, every message, every
# service — is upstream's, untouched.
#
# Run this only to move to a new etcd version; the output is committed, so
# an ordinary build never fetches anything.
set -euo pipefail

ETCD_VERSION="${1:-v3.5.16}"
cd "$(dirname "${BASH_SOURCE[0]}")"

echo "==> Fetching etcd $ETCD_VERSION protos..."
for path in api/mvccpb/kv.proto api/authpb/auth.proto api/etcdserverpb/rpc.proto; do
    curl -fsSL -o "$(basename "$path")" \
        "https://raw.githubusercontent.com/etcd-io/etcd/$ETCD_VERSION/$path"
done

echo "==> Stripping gogoproto + grpc-gateway annotations..."
python3 - "$ETCD_VERSION" <<'PY'
import re
import sys

version = sys.argv[1]

BANNER = (
    "// Vendored from etcd-io/etcd {v}, then stripped of gogoproto and\n"
    "// grpc-gateway (google.api.http) annotations by proto/vendor.sh.\n"
    "// Neither affects the wire format. Do not hand-edit: re-run vendor.sh.\n"
).format(v=version)

for name in ("kv.proto", "auth.proto", "rpc.proto"):
    src = open(name).read()

    # Imports that only existed for the annotations being removed.
    src = re.sub(r'^import "gogoproto/gogo\.proto";\n', "", src, flags=re.M)
    src = re.sub(r'^import "google/api/annotations\.proto";\n', "", src, flags=re.M)
    # etcd's own imports are repo-absolute; everything lives side by side here.
    src = re.sub(r'^import "etcd/api/\w+/(\w+\.proto)";', r'import "\1";', src, flags=re.M)

    # File- and message-level gogoproto options.
    src = re.sub(r"^\s*option \(gogoproto\.[^;]*;\n", "", src, flags=re.M)
    # Field-level ones: `bytes key = 1 [(gogoproto.nullable) = false];`
    src = re.sub(r"\s*\[\(gogoproto\.[^\]]*\]", "", src)

    # Multi-line grpc-gateway blocks:
    #     option (google.api.http) = {
    #       post: "/v3/kv/range"
    #       body: "*"
    #   };
    src = re.sub(r"\n\s*option \(google\.api\.http\) = \{.*?\};", "", src, flags=re.S)
    # That leaves `rpc Foo(Bar) returns (Baz) {\n  }` — legal, but collapse it.
    src = re.sub(r"(rpc \w+\([^)]*\) returns \((?:stream )?[^)]*\)) \{\s*\}", r"\1 {}", src)

    # Comment-only leftovers of the http annotations ("// for grpc-gateway").
    src = re.sub(r"^// for grpc-gateway\n", "", src, flags=re.M)
    src = re.sub(r"\n{3,}", "\n\n", src)

    open(name, "w").write(BANNER + src)
    print(f"    {name}: {len(src.splitlines())} lines")
PY

echo "==> Checking nothing gogo/gateway-shaped survived..."
# Comment lines are excluded on purpose: this script's own banner names both
# annotations, and a check that trips over its own output is worse than no
# check at all.
leftovers="$(grep -vE '^\s*//' ./*.proto | grep -n "gogoproto\|google\.api" || true)"
if [[ -n "$leftovers" ]]; then
    echo "$leftovers" >&2
    echo "vendor.sh: annotations survived the strip — fix the patterns above." >&2
    exit 1
fi
echo "==> Done. Rebuild to regenerate: cargo build -p nodestore"
