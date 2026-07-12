#!/usr/bin/env python3
"""Audit candidate grid cells one axis at a time before LUT generation."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
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
RELATIVE_BUDGETS = {
    name: 0.20 if name.startswith("fall_speed_") else 0.15
    for name in COMPONENT_NAMES
}
ABSOLUTE_FLOOR = 1.0e-12
TARGET_AXES = {
    "conventional_wet_hail_sband_unvalidated": ("equivolume_diameter",),
    "property_p3_ishmael_dry_oblate_sband_unvalidated": (
        "equivolume_diameter",
        "bulk_density",
    ),
    "property_p3_ishmael_dry_prolate_sband_unvalidated": (
        "equivolume_diameter",
        "bulk_density",
    ),
    "property_p3_ishmael_wet_oblate_sband_unvalidated": (
        "equivolume_diameter",
        "condensed_volume_fraction",
        "liquid_mass_fraction",
    ),
    "property_p3_ishmael_wet_prolate_sband_unvalidated": (
        "equivolume_diameter",
        "condensed_volume_fraction",
        "liquid_mass_fraction",
    ),
    "property_rain_sband_unvalidated": ("equivolume_diameter",),
}
MATERIAL_AXES = {
    "bulk_density",
    "condensed_volume_fraction",
    "liquid_mass_fraction",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_axis_budget_generator", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_json(path: Path, value: Any) -> None:
    data = (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode(
        "utf-8"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-root", type=Path, required=True)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--parent-failure-report", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    tool_root = args.tool_root.resolve()
    asset_root = args.asset_root.resolve()
    generator_path = tool_root / "generate_lut.py"
    validation_path = Path(__file__).resolve()
    environment_path = args.environment_report.resolve()
    initial_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "validation_source_sha256": sha256_file(validation_path),
        "environment_report_sha256": sha256_file(environment_path),
    }
    environment = json.loads(environment_path.read_text(encoding="utf-8"))
    environment_generator_sha256 = environment.get("tool_file_sha256", {}).get(
        "generate_lut.py"
    )
    if environment_generator_sha256 != initial_hashes["generator_source_sha256"]:
        raise RuntimeError("environment report does not describe this generator source")
    generator = load_generator(tool_root)
    failed_ranges: dict[tuple[str, str], list[tuple[float, float]]] | None = None
    parent_failure_sha256 = None
    parent_path = None
    if args.parent_failure_report is not None:
        parent_path = args.parent_failure_report.resolve()
        parent = json.loads(parent_path.read_text(encoding="utf-8"))
        if parent.get("axis_budget_check_passed") is not False:
            raise RuntimeError("parent report is not a failed axis-budget report")
        failed_ranges = {}
        for table in parent["tables"]:
            for axis in table["axes"]:
                for sample in axis["samples"]:
                    if not sample["within_axis_budget"]:
                        failed_ranges.setdefault(
                            (table["asset_directory"], axis["axis"]), []
                        ).append((float(sample["lower"]), float(sample["upper"])))
        parent_failure_sha256 = sha256_file(parent_path)
    table_reports = []
    config_path_sha256: dict[Path, str] = {}
    all_passed = True
    total_samples = 0
    for asset_directory, target_axes in TARGET_AXES.items():
        if failed_ranges is not None and not any(
            (asset_directory, kind) in failed_ranges for kind in target_axes
        ):
            continue
        config_path = asset_root / asset_directory / "config.json"
        config_path_sha256[config_path] = sha256_file(config_path)
        config_bytes, _, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        axes = {axis["kind"]: axis for axis in config["axes"]}
        reference = {
            kind: float(axis["coordinates"][len(axis["coordinates"]) // 2])
            for kind, axis in axes.items()
        }
        axis_reports = []
        table_passed = True
        for kind in target_axes:
            if failed_ranges is not None and (asset_directory, kind) not in failed_ranges:
                continue
            coordinates = [float(value) for value in axes[kind]["coordinates"]]
            fractions = (0.25, 0.5, 0.75) if kind in MATERIAL_AXES else (0.5,)
            intervals = list(enumerate(zip(coordinates, coordinates[1:])))
            if failed_ranges is not None:
                parent_ranges = failed_ranges[(asset_directory, kind)]
                intervals = [
                    (index, bounds)
                    for index, bounds in intervals
                    if any(
                        bounds[0] >= parent_lower and bounds[1] <= parent_upper
                        for parent_lower, parent_upper in parent_ranges
                    )
                ]
                if not intervals:
                    raise RuntimeError("refinement produced no child cells for failed range")
            sample_values = set()
            for _, (lower, upper) in intervals:
                sample_values.update((lower, upper))
                for fraction in fractions:
                    sample_values.add(lower + fraction * (upper - lower))
            points = []
            for value in sorted(sample_values):
                point = dict(reference)
                point[kind] = value
                points.append(point)
            timeout = int(config["execution"]["point_timeout_seconds"])
            grouping = config["execution"].get("grouping")
            if kind in MATERIAL_AXES or grouping is None:
                direct = [
                    generator.run_isolated_point(config, point, timeout)
                    for point in points
                ]
            else:
                direct = generator.run_isolated_material_state_group(
                    config, points, int(grouping["group_timeout_seconds"])
                )
            values = {point[kind]: result for point, result in zip(points, direct)}
            samples = []
            axis_passed = True
            for interval, (lower, upper) in intervals:
                for fraction in fractions:
                    coordinate = lower + fraction * (upper - lower)
                    expected = values[coordinate]
                    interpolated = [
                        (1.0 - fraction) * lo + fraction * hi
                        for lo, hi in zip(values[lower], values[upper])
                    ]
                    errors = []
                    sample_passed = True
                    for name, direct_value, interpolated_value in zip(
                        COMPONENT_NAMES, expected, interpolated
                    ):
                        relative = abs(interpolated_value - direct_value) / max(
                            abs(direct_value), ABSOLUTE_FLOOR
                        )
                        within = relative <= RELATIVE_BUDGETS[name]
                        sample_passed = sample_passed and within
                        errors.append(
                            {
                                "component": name,
                                "relative_error_with_absolute_floor": relative,
                                "within_axis_budget": within,
                            }
                        )
                    axis_passed = axis_passed and sample_passed
                    total_samples += 1
                    samples.append(
                        {
                            "interval_index": interval,
                            "lower": lower,
                            "upper": upper,
                            "within_cell_fraction": fraction,
                            "coordinate": coordinate,
                            "errors": errors,
                            "within_axis_budget": sample_passed,
                        }
                    )
            table_passed = table_passed and axis_passed
            axis_reports.append(
                {
                    "axis": kind,
                    "sample_count": len(samples),
                    "within_axis_budget": axis_passed,
                    "samples": samples,
                }
            )
        all_passed = all_passed and table_passed
        table_reports.append(
            {
                "asset_directory": asset_directory,
                "table_id": config["table_id"],
                "config_sha256": hashlib.sha256(config_bytes).hexdigest(),
                "within_axis_budget": table_passed,
                "axes": axis_reports,
            }
        )
    final_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "validation_source_sha256": sha256_file(validation_path),
        "environment_report_sha256": sha256_file(environment_path),
    }
    if final_hashes != initial_hashes:
        raise RuntimeError("audit source or environment changed while work was running")
    for config_path, initial_sha256 in config_path_sha256.items():
        if sha256_file(config_path) != initial_sha256:
            raise RuntimeError(f"config changed while audit was running: {config_path}")
    if parent_path is not None and sha256_file(parent_path) != parent_failure_sha256:
        raise RuntimeError("parent failure report changed while audit was running")
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-candidate-grid-one-axis-budget-v1",
        "classification": "grid_design_not_final_held_out_validation",
        "scientifically_independent": False,
        "production_validation": False,
        "axis_budget_check_passed": all_passed,
        "relative_budgets": RELATIVE_BUDGETS,
        "absolute_floor": ABSOLUTE_FLOOR,
        "sample_count": total_samples,
        "worker_limit": 1,
        "effective_worker_count": 1,
        "selection_protocol": (
            "Only rule-refined axes are varied. Material axes use fixed 0.25, 0.5, "
            "and 0.75 cell fractions; diameter uses cell midpoints. All other axes "
            "are held at declared interior LUT nodes. This is a pre-freeze grid-design "
            "margin audit and is not the final seeded held-out set."
        ),
        **initial_hashes,
        "environment_generator_source_sha256": environment_generator_sha256,
        "tables": table_reports,
    }
    if parent_failure_sha256 is not None:
        report["parent_failure_report_sha256"] = parent_failure_sha256
        report["selection_protocol"] = (
            "Deterministic recursive refinement audit. Only child cells wholly inside "
            "failed ranges from the identified parent report are tested, with unchanged "
            "within-cell fractions and error budgets. A full audit is required after this "
            "narrow audit passes and before the axes may be frozen."
        )
    write_json(args.output.resolve(), report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
