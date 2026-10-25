#!/usr/bin/env python3
"""Extract concise failure details from a nodemigrate crate-test log."""

import re
import sys
from pathlib import Path


MARKER = re.compile(
    r"^(?:error(?:\[[^]]+\])?:|failures:|---- .* stdout ----|"
    r"thread .* panicked|test result: FAILED|test .* FAILED$)"
)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: summarize_nodemigrate_test_log.py PATH", file=sys.stderr)
        return 2
    lines = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace").splitlines()
    selected: set[int] = set()
    for index, line in enumerate(lines):
        if MARKER.search(line):
            selected.update(range(max(0, index - 2), min(len(lines), index + 5)))
    if not selected:
        selected.update(range(max(0, len(lines) - 30), len(lines)))
    summary = "\n".join(lines[index] for index in sorted(selected))
    print(summary[-3000:])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
