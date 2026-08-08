#!/usr/bin/env bash
# e2e-misc-prereqs.sh — cheap, always-needed e2e prerequisites that have
# nothing to do with the real CSI/DRA reference drivers: a small real
# hugepage pool, and the grpcurl binary. Split out of e2e-full-setup.sh
# (round 124) specifically so every e2e shard can run this regardless of
# whether it's one of the shards installing the (much heavier) CSI/DRA
# reference drivers — hugepages_capacity_when_reserved and the
# PodResources gRPC test aren't CSI/DRA-gated at all and would otherwise
# silently skip on any shard that skipped e2e-full-setup.sh entirely.
#
# Assumes: a running not-k8s cluster isn't actually required for either of
# these (no kubectl calls here at all) — this only needs to run sometime
# before the e2e suite itself, same as e2e-full-setup.sh.
set -euo pipefail

WORK_DIR="${E2E_SETUP_WORK_DIR:-$(mktemp -d)}"

log() { echo "==> $*"; }

# ── hugepages: reserve a small real pool ────────────────────────────────
# 64 * 2Mi = 128Mi, small and safe on any GitHub-hosted runner's real RAM
# (several GB) — just enough for a test pod to request a couple of pages.
# Idempotent: re-running with the same count is a no-op if already
# reserved; harmless (just re-asserts the same value) if run again.
if [[ "$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0)" -eq 0 ]]; then
    log "reserving a small hugepage pool (64 * 2Mi) for hugepages-dependent e2e tests..."
    echo 64 | sudo tee /proc/sys/vm/nr_hugepages >/dev/null || log "couldn't reserve hugepages (not fatal — hugepages-dependent tests will just skip)"
fi

# ── grpcurl: the gRPC client pod_resources.sh's real query test needs ──────
if ! command -v grpcurl &>/dev/null; then
    log "installing grpcurl..."
    GRPCURL_VERSION="1.9.1"
    ARCH_RAW="$(uname -m)"
    case "$ARCH_RAW" in
        x86_64) GRPCURL_ARCH=x86_64 ;;
        aarch64) GRPCURL_ARCH=arm64 ;;
        armv7l) GRPCURL_ARCH=armv7 ;;
        *) echo "unsupported arch for grpcurl install: $ARCH_RAW" >&2; exit 1 ;;
    esac
    curl -fsSL "https://github.com/fullstorydev/grpcurl/releases/download/v${GRPCURL_VERSION}/grpcurl_${GRPCURL_VERSION}_linux_${GRPCURL_ARCH}.tar.gz" -o "$WORK_DIR/grpcurl.tar.gz"
    tar -xzf "$WORK_DIR/grpcurl.tar.gz" -C "$WORK_DIR" grpcurl
    sudo install -m 0755 "$WORK_DIR/grpcurl" /usr/local/bin/grpcurl
fi

[[ -z "${E2E_SETUP_WORK_DIR:-}" ]] && rm -rf "$WORK_DIR"
log "done."
