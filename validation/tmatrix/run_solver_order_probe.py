#!/usr/bin/env python3
"""Probe higher solver orders at points rejected by an exhaustive sweep."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
from typing import Any, Sequence


COMPONENT_NAMES = (
    "zh",
    "zv",
    "hh_vv_covariance_real",
    "hh_vv_covariance_imaginary",
    "kdp",
    "ah",
    "av",
    "fall_speed_first_moment",
    "fall_speed_second_moment",
)
ORDERS = (12, 14, 16, 18, 20)
RELATIVE_TOLERANCE = 1.0e-3
ABSOLUTE_TOLERANCE = 1.0e-12


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_solver_probe_generator", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_json(path: Path, value: Any) -> None:
    data = (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode(
        "utf-8"
    )
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-root", type=Path, required=True)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--failure-report", type=Path, required=True)
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    tool_root = args.tool_root.resolve()
    generator_path = tool_root / "generate_lut.py"
    source_hashes = {
        "generator": sha256_file(generator_path),
        "probe": sha256_file(Path(__file__).resolve()),
        "environment": sha256_file(args.environment_report.resolve()),
        "failure": sha256_file(args.failure_report.resolve()),
    }
    generator = load_generator(tool_root)
    failure = json.loads(args.failure_report.resolve().read_text(encoding="utf-8"))
    rejected = next(
        table
        for table in failure["tables"]
        if table["asset_directory"]
        == "property_p3_ishmael_dry_prolate_sband_unvalidated"
    )
    points = []
    seen = set()
    for record in rejected["recorded_component_tolerance_failures"]:
        coordinates = {key: float(value) for key, value in record["coordinates"].items()}
        key = tuple(sorted(coordinates.items()))
        if key not in seen:
            seen.add(key)
            points.append(coordinates)
    config_path = (
        args.asset_root.resolve()
        / "property_p3_ishmael_dry_prolate_sband_unvalidated"
        / "config.json"
    )
    config_bytes, _, config = generator.parse_exact_json(config_path)
    generator.validate_config(config)
    timeout = int(config["execution"]["grouping"]["group_timeout_seconds"])
    point_reports = []
    for coordinates in points:
        values = {
            order: generator.run_isolated_solver_ndgs_comparison_group(
                config, [coordinates], order, timeout
            )[0]
            for order in ORDERS
        }
        transitions = []
        for lower_order, upper_order in zip(ORDERS, ORDERS[1:]):
            lower = values[lower_order]
            upper = values[upper_order]
            comparisons = []
            passed = True
            for index, (name, low, high) in enumerate(
                zip(COMPONENT_NAMES, lower, upper)
            ):
                scale = max(abs(float(low)), abs(float(high)))
                if index in (2, 3):
                    scale = max(
                        math.hypot(float(lower[2]), float(lower[3])),
                        math.hypot(float(upper[2]), float(upper[3])),
                    )
                allowed = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * scale
                ratio = abs(float(high) - float(low)) / allowed
                within = ratio <= 1.0
                passed = passed and within
                comparisons.append(
                    {
                        "component": name,
                        "difference_to_allowed_ratio": ratio,
                        "within_predeclared_tolerance": within,
                    }
                )
            transitions.append(
                {
                    "lower_ndgs": lower_order,
                    "upper_ndgs": upper_order,
                    "within_predeclared_tolerance": passed,
                    "comparisons": comparisons,
                }
            )
        point_reports.append(
            {
                "coordinates": coordinates,
                "values_by_ndgs": {str(order): values[order] for order in ORDERS},
                "transitions": transitions,
            }
        )
    if source_hashes != {
        "generator": sha256_file(generator_path),
        "probe": sha256_file(Path(__file__).resolve()),
        "environment": sha256_file(args.environment_report.resolve()),
        "failure": sha256_file(args.failure_report.resolve()),
    }:
        raise RuntimeError("probe inputs changed while solver work was running")
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-rejected-dry-prolate-higher-order-probe-v1",
        "classification": "solver_order_design_probe_only",
        "scientifically_independent": False,
        "production_validation": False,
        "orders": list(ORDERS),
        "relative_tolerance": RELATIVE_TOLERANCE,
        "absolute_tolerance": ABSOLUTE_TOLERANCE,
        "point_count": len(point_reports),
        "config_sha256": hashlib.sha256(config_bytes).hexdigest(),
        "source_hashes": source_hashes,
        "points": point_reports,
    }
    write_json(args.output.resolve(), report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
