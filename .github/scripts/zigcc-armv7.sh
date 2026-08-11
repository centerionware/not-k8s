#!/bin/sh
# zigcc-armv7.sh — a C compiler for armv7 musl, backed by zig.
#
# Used only as the *fallback* when musl.cc (the normal source of an armhf
# musl cross gcc, and what deploy/lib/toolchain-c.sh also falls back to) is
# unreachable or too slow from a CI runner, which happens often enough to
# have hung a job for 17 minutes.
#
# The one non-obvious thing this does: it strips any `--target=<rust triple>`
# argument before handing the rest to zig.
#
# cc-rs passes the *Rust* triple through to the C compiler
# (`--target=armv7-unknown-linux-musleabihf`). zig has its own architecture
# naming and calls 32-bit ARM `arm`, not `armv7`, so it rejects that with
# "error: unknown architecture: 'armv7'" and the ring/sqlite C builds fail.
# Passing `-target arm-linux-musleabihf` ourselves is not enough on its own —
# cc-rs's flag comes later on the command line and wins. It has to be removed.
#
# Rebuilding "$@" rather than using an array keeps this POSIX sh: consume the
# original arguments one at a time (tracked by $count, since $# changes as we
# append) and push the ones we keep onto the end.

count=$#
while [ "$count" -gt 0 ]; do
    arg=$1
    shift
    count=$((count - 1))
    case "$arg" in
        --target=*) continue ;;
    esac
    set -- "$@" "$arg"
done

exec python3 -m ziglang cc -target arm-linux-musleabihf "$@"
