#!/usr/bin/env python3
"""Convert cargo output into GitHub Actions workflow commands.

GitHub does not ship a Rust problem matcher, so compiler errors live only in
the job log — which this repository's sandbox cannot download. Emitting
file/line annotations instead makes them retrievable through the Checks API.

Usage: python3 ci/annotate.py <cargo-output-log>
"""

import re
import sys

MAX_ANNOTATIONS = 10


def esc_message(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def esc_property(text: str) -> str:
    return (
        text.replace("%", "%25")
        .replace("\r", "%0D")
        .replace("\n", "%0A")
        .replace(":", "%3A")
        .replace(",", "%2C")
    )


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: annotate.py <log>", file=sys.stderr)
        return 2

    try:
        with open(sys.argv[1], "r", errors="replace") as fh:
            lines = fh.read().splitlines()
    except OSError as exc:
        print(f"::error::annotate.py could not read {sys.argv[1]}: {exc}")
        return 0

    annotations = []
    pending_message = None

    error_re = re.compile(r"^(error(?:\[[A-Za-z_][A-Za-z0-9_]*\])?): (.*)$")
    loc_re = re.compile(r"^\s+-->\s+([^:]+):(\d+):(\d+)")
    panic_re = re.compile(r"panicked at (.+)$")

    def flush_pending(path=None, line=None, col=None):
        nonlocal pending_message
        if pending_message is None:
            return
        msg = pending_message
        pending_message = None
        if path is not None:
            annotations.append(
                f"::error file={esc_property(path)},line={line},col={col}::{esc_message(msg)}"
            )
        else:
            annotations.append(f"::error::{esc_message(msg)}")

    for line in lines:
        m = error_re.match(line)
        if m:
            flush_pending()
            pending_message = f"{m.group(1)}: {m.group(2)}"
            continue
        m = loc_re.match(line)
        if m and pending_message is not None:
            path, row, col = m.group(1), m.group(2), m.group(3)
            flush_pending(path, row, col)
            continue
        m = panic_re.match(line)
        if m:
            annotations.append(f"::error::test panicked at {esc_message(m.group(1))}")
            continue

    flush_pending()

    if not annotations:
        # Fallback: surface the tail of the log so we are never blind.
        tail = " | ".join(lines[-8:])
        annotations.append(f"::error::cargo failed (no parsed diagnostics); tail: {esc_message(tail)}")

    for line in annotations[:MAX_ANNOTATIONS]:
        print(line)
    if len(annotations) > MAX_ANNOTATIONS:
        print(f"::warning::{len(annotations) - MAX_ANNOTATIONS} more diagnostic(s) omitted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
