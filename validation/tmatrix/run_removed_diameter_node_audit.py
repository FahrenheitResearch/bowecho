#!/usr/bin/env python3
"""Audit interpolation across a parent interval before removing one grid node."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import importlib.util
import itertools
import json
import os
from pathlib import Path
from typing import Any, Sequence


ASSET_DIRECTORY = "property_p3_ishmael_dry_prolate_sband_unvalidated"
LOWER_DIAMETER = 0.03231174267785264
REMOVED_DIAMETER = 0.0361256265495798
UPPER_DIAMETER = 0.0403896783473158
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
RELATIVE_BUDGETS = {
    name: 0.20 if name.startswith("fall_speed_") else 0.15
    for name in COMPONENT_NAMES
}
ABSOLUTE_FLOOR = 1.0e-12
WORKER_LIMIT = 12


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_removed_node_generator", path)
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
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    tool_root = args.tool_root.resolve()
    generator_path = tool_root / "generate_lut.py"
    script_path = Path(__file__).resolve()
    config_path = args.asset_root.resolve() / ASSET_DIRECTORY / "config.json"
    environment_path = args.environment_report.resolve()
    initial_hashes = {
        "generator": sha256_file(generator_path),
        "audit": sha256_file(script_path),
        "config": sha256_file(config_path),
        "environment": sha256_file(environment_path),
    }
    generator = load_generator(tool_root)
    config_bytes, _, config = generator.parse_exact_json(config_path)
    generator.validate_config(config)
    diameters = generator.axis_coordinates(config, "equivolume_diameter")
    for coordinate in (LOWER_DIAMETER, REMOVED_DIAMETER, UPPER_DIAMETER):
        if coordinate not in diameters:
            raise RuntimeError(f"required diameter node is absent: {coordinate}")
    axes = [axis for axis in config["axes"] if axis["kind"] != "equivolume_diameter"]
    other_points = [
        dict(zip((axis["kind"] for axis in axes), values))
        for values in itertools.product(
            *([float(value) for value in axis["coordinates"]] for axis in axes)
        )
    ]
    grouping = config["execution"]["grouping"]
    material_axes = tuple(grouping["material_state_axis_kinds"])
    grouped: dict[tuple[float, ...], list[dict[str, float]]] = {}
    for other in other_points:
        key = tuple(other[kind] for kind in material_axes)
        grouped.setdefault(key, []).append(other)
    group_items = list(grouped.items())
    group_results: list[tuple[list[dict[str, float]], list[list[float]]] | None] = [
        None
    ] * len(group_items)

    def evaluate_group(indexed):
        index, (_, states) = indexed
        points = []
        for state in states:
            for diameter in (LOWER_DIAMETER, REMOVED_DIAMETER, UPPER_DIAMETER):
                point = dict(state)
                point["equivolume_diameter"] = diameter
                points.append(point)
        values = generator.run_isolated_material_state_group(
            config, points, int(grouping["group_timeout_seconds"])
        )
        return index, states, values

    with concurrent.futures.ThreadPoolExecutor(max_workers=WORKER_LIMIT) as executor:
        futures = [
            executor.submit(evaluate_group, item) for item in enumerate(group_items)
        ]
        for future in concurrent.futures.as_completed(futures):
            index, states, values = future.result()
            group_results[index] = (states, values)

    fraction = (REMOVED_DIAMETER - LOWER_DIAMETER) / (
        UPPER_DIAMETER - LOWER_DIAMETER
    )
    failure_count = 0
    recorded_failures = []
    worst_by_component = [None] * len(COMPONENT_NAMES)
    sample_count = 0
    for result in group_results:
        if result is None:
            raise RuntimeError("executor omitted a material group")
        states, values = result
        for state, offset in zip(states, range(0, len(values), 3)):
            lower, direct, upper = values[offset : offset + 3]
            sample_count += 1
            for index, (name, low, expected, high) in enumerate(
                zip(COMPONENT_NAMES, lower, direct, upper)
            ):
                interpolated = (1.0 - fraction) * low + fraction * high
                relative = abs(interpolated - expected) / max(
                    abs(expected), ABSOLUTE_FLOOR
                )
                within = relative <= RELATIVE_BUDGETS[name]
                record = {
                    "component": name,
                    "coordinates_without_diameter": state,
                    "relative_error_with_absolute_floor": relative,
                    "within_axis_budget": within,
                }
                previous = worst_by_component[index]
                if previous is None or relative > previous[
                    "relative_error_with_absolute_floor"
                ]:
                    worst_by_component[index] = record
                if not within:
                    failure_count += 1
                    if len(recorded_failures) < 100:
                        recorded_failures.append(record)
    final_hashes = {
        "generator": sha256_file(generator_path),
        "audit": sha256_file(script_path),
        "config": sha256_file(config_path),
        "environment": sha256_file(environment_path),
    }
    if final_hashes != initial_hashes:
        raise RuntimeError("audit inputs changed while solver work was running")
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-dry-prolate-removed-diameter-parent-cell-v1",
        "classification": "grid_design_not_final_held_out_validation",
        "scientifically_independent": False,
        "production_validation": False,
        "removed_node_audit_passed": failure_count == 0,
        "lower_diameter_m": LOWER_DIAMETER,
        "removed_diameter_m": REMOVED_DIAMETER,
        "upper_diameter_m": UPPER_DIAMETER,
        "raw_linear_fraction": fraction,
        "sample_count": sample_count,
        "component_comparison_count": sample_count * len(COMPONENT_NAMES),
        "worker_limit": WORKER_LIMIT,
        "effective_worker_count": min(WORKER_LIMIT, len(group_items)),
        "material_group_count": len(group_items),
        "relative_budgets": RELATIVE_BUDGETS,
        "absolute_floor": ABSOLUTE_FLOOR,
        "component_failure_count": failure_count,
        "recorded_failure_limit": 100,
        "recorded_failures": recorded_failures,
        "worst_by_component": worst_by_component,
        "source_hashes": initial_hashes,
        "config_sha256": hashlib.sha256(config_bytes).hexdigest(),
        "selection_protocol": (
            "The inserted diameter node is treated as an off-grid direct point. Raw-linear "
            "interpolation from its two retained parent endpoints is checked at every "
            "declared temperature, density, shape, frequency, and radar-view state with "
            "the unchanged 15 percent scattering and 20 percent fall-moment budgets."
        ),
    }
    write_json(args.output.resolve(), report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
