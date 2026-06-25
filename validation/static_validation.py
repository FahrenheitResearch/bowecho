#!/usr/bin/env python3
"""Lightweight source checks used when rustc is unavailable."""
from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "crates" / "product_engine" / "src"


def delimiter_errors(text: str) -> list[str]:
    stack: list[tuple[str, int, int]] = []
    errors: list[str] = []
    matching = {")": "(", "]": "[", "}": "{"}
    opens = set(matching.values())
    i = 0
    line = col = 1
    state: object = "code"
    while i < len(text):
        c = text[i]
        n = text[i + 1] if i + 1 < len(text) else ""
        if state == "code":
            if c == "/" and n == "/":
                state = "line"
                i += 2
                col += 2
                continue
            if c == "/" and n == "*":
                state = ("block", 1)
                i += 2
                col += 2
                continue
            if c == '"':
                state = "string"
            elif c == "r":
                j = i + 1
                hashes = 0
                while j < len(text) and text[j] == "#":
                    hashes += 1
                    j += 1
                if j < len(text) and text[j] == '"':
                    state = ("raw", hashes)
                    col += j - i
                    i = j
            elif c in opens:
                stack.append((c, line, col))
            elif c in matching:
                if not stack or stack[-1][0] != matching[c]:
                    errors.append(f"mismatched {c} at {line}:{col}")
                else:
                    stack.pop()
        elif state == "line":
            if c == "\n":
                state = "code"
        elif isinstance(state, tuple) and state[0] == "block":
            depth = state[1]
            if c == "/" and n == "*":
                state = ("block", depth + 1)
                i += 2
                col += 2
                continue
            if c == "*" and n == "/":
                depth -= 1
                state = "code" if depth == 0 else ("block", depth)
                i += 2
                col += 2
                continue
        elif state == "string":
            if c == "\\":
                i += 2
                col += 2
                continue
            if c == '"':
                state = "code"
        elif isinstance(state, tuple) and state[0] == "raw":
            hashes = state[1]
            if c == '"' and text[i + 1 : i + 1 + hashes] == "#" * hashes:
                i += hashes
                col += hashes
                state = "code"
        if c == "\n":
            line += 1
            col = 1
        else:
            col += 1
        i += 1
    if stack:
        errors.append(f"unclosed delimiter {stack[-1]}")
    return errors


def main() -> None:
    files = sorted(SRC.glob("*.rs"))
    source = {path.name: path.read_text() for path in files}
    sweep = source["sweep.rs"]

    enum_body = re.search(
        r"pub enum DerivedSweepProduct\s*\{(?P<body>.*?)\n\}", sweep, re.S
    ).group("body")
    variants = [
        line.strip().rstrip(",")
        for line in enum_body.splitlines()
        if line.strip() and not line.lstrip().startswith("//")
    ]
    all_body = re.search(
        r"pub const ALL:.*?=\s*&\[(?P<body>.*?)\n\s*\];", sweep, re.S
    ).group("body")
    all_variants = re.findall(r"Self::([A-Za-z0-9_]+)", all_body)
    id_match = re.search(
        r"pub const fn id\(self\).*?match self \{(?P<body>.*?)\n\s*\}\n\s*\}",
        sweep,
        re.S,
    ).group("body")
    ids = re.findall(r'Self::[A-Za-z0-9_]+\s*=>\s*"([^"]+)"', id_match)

    checks = {
        "rust_source_files": [path.name for path in files],
        "line_counts": {name: text.count("\n") + 1 for name, text in source.items()},
        "delimiter_errors": {
            name: delimiter_errors(text) for name, text in source.items()
        },
        "trailing_whitespace_lines": {
            name: sum(1 for line in text.splitlines() if line.rstrip() != line)
            for name, text in source.items()
        },
        "tab_characters": {name: text.count("\t") for name, text in source.items()},
        "derived_variant_count": len(variants),
        "all_list_count": len(all_variants),
        "all_list_matches_enum": set(variants) == set(all_variants),
        "product_id_count": len(ids),
        "product_ids_unique": len(ids) == len(set(ids)),
        "native_kdp_guard_present": "overwrite_existing" in sweep
        and "MomentType::SpecificDifferentialPhase" in sweep,
        "kdp_half_derivative_present": "0.5 * fit.slope" in sweep,
        "physical_range_alignment_present": "gate_center_m" in sweep
        and "row_by_radial" in sweep,
    }
    checks["passed"] = (
        all(not errors for errors in checks["delimiter_errors"].values())
        and all(count == 0 for count in checks["trailing_whitespace_lines"].values())
        and all(count == 0 for count in checks["tab_characters"].values())
        and checks["all_list_matches_enum"]
        and checks["product_ids_unique"]
        and checks["native_kdp_guard_present"]
        and checks["kdp_half_derivative_present"]
        and checks["physical_range_alignment_present"]
    )
    out = Path(__file__).with_name("static_validation.json")
    out.write_text(json.dumps(checks, indent=2) + "\n")
    print(json.dumps(checks, indent=2))
    raise SystemExit(0 if checks["passed"] else 1)


if __name__ == "__main__":
    main()
