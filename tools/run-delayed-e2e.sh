#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 || ! "$1" =~ ^[0-9]+$ || -z "$2" ]]; then
    echo "usage: $0 <delay-minutes> <branch>" >&2
    exit 2
fi

delay_minutes=$1
branch=$2
sleep "$((delay_minutes * 60))"

for _ in 1 2 3; do
    gh workflow run e2e.yml \
        --ref "$branch" \
        -f proxy=nodeproxy \
        -f network=flannel \
        -f only=
done
