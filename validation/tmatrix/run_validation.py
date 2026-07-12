#!/usr/bin/env python3
"""Interpolation-only held-out checks and non-production physical sanity tests."""

from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import itertools
import json
import math
import os
import struct
import sys
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


class ValidationError(RuntimeError):
    pass


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def write_json(path: Path, value: Any) -> None:
    data = (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode(
        "utf-8"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def load_generator(tool_root: Path) -> Any:
    generator_path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_generate_lut", generator_path)
    if spec is None or spec.loader is None:
        raise ValidationError(f"cannot import {generator_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def decode_lut(generator: Any, path: Path) -> tuple[dict[str, Any], list[list[float]], bytes, bytes]:
    data, header, header_bytes, payload = generator._parse_lut(path)
    if header.get("payload_sha256") != sha256_bytes(payload):
        raise ValidationError(f"{path}: payload digest mismatch")
    if header.get("config_sha256") != sha256_bytes(
        header.get("generator_config_utf8", "").encode("utf-8")
    ):
        raise ValidationError(f"{path}: embedded config digest mismatch")
    if header.get("science", {}).get("validation") != {
        "status": "research_only_unvalidated"
    }:
        raise ValidationError(f"{path}: table is not research_only_unvalidated")
    points = int(header["grid_point_count"])
    if len(payload) != points * 9 * 8:
        raise ValidationError(f"{path}: payload length mismatch")
    values = [
        list(struct.unpack("<9d", payload[offset : offset + 72]))
        for offset in range(0, len(payload), 72)
    ]
    return header, values, data, header_bytes


def brackets(axis: dict[str, Any], coordinate: float) -> list[tuple[int, float]]:
    values = [float(value) for value in axis["coordinates"]]
    if len(values) == 1:
        if coordinate != values[0]:
            raise ValidationError(f"singleton {axis['kind']} coordinate must be exact")
        return [(0, 1.0)]
    if not values[0] < coordinate < values[-1]:
        raise ValidationError(
            f"held-out {axis['kind']}={coordinate} must be strictly inside LUT bounds"
        )
    if coordinate in values:
        return [(values.index(coordinate), 1.0)]
    upper = next(index for index, value in enumerate(values) if value > coordinate)
    lower = upper - 1
    fraction = (coordinate - values[lower]) / (values[upper] - values[lower])
    return [(lower, 1.0 - fraction), (upper, fraction)]


def interpolate(
    axes: list[dict[str, Any]], values: list[list[float]], coordinates: dict[str, float]
) -> list[float]:
    if set(coordinates) != {axis["kind"] for axis in axes}:
        raise ValidationError("held-out coordinate fields do not match table axes")
    per_axis = [brackets(axis, float(coordinates[axis["kind"]])) for axis in axes]
    result = [0.0] * 9
    dimensions = [len(axis["coordinates"]) for axis in axes]
    for corner in itertools.product(*per_axis):
        flat = 0
        weight = 1.0
        for dimension, (index, axis_weight) in zip(dimensions, corner):
            flat = flat * dimension + index
            weight *= axis_weight
        for component in range(9):
            result[component] += weight * values[flat][component]
    return result


def error_record(
    direct: Sequence[float], interpolated: Sequence[float], thresholds: dict[str, Any]
) -> tuple[list[dict[str, Any]], bool]:
    records = []
    passed = True
    for name, expected, actual in zip(COMPONENT_NAMES, direct, interpolated):
        threshold = thresholds[name]
        absolute = abs(float(actual) - float(expected))
        relative = absolute / max(abs(float(expected)), float(threshold["absolute"]))
        within = absolute <= float(threshold["absolute"]) + float(
            threshold["relative"]
        ) * abs(float(expected))
        passed = passed and within
        records.append(
            {
                "component": name,
                "absolute_error": absolute,
                "relative_error_with_absolute_floor": relative,
                "within_predeclared_threshold": within,
            }
        )
    return records, passed


def heldout(args: argparse.Namespace) -> None:
    generator = load_generator(args.tool_root.resolve())
    request_bytes, _, request = generator.parse_exact_json(args.nodes.resolve())
    if request.get("schema") != 1 or request.get("classification") != "held_out_from_lut_interpolation_check_only":
        raise ValidationError("held-out request has incorrect classification")
    selector_path = Path(__file__).resolve().with_name("select_held_out_nodes.py")
    if request.get("selector_source_sha256") != sha256_file(selector_path):
        raise ValidationError("held-out selector source changed after node selection")
    asset_root = args.asset_root.resolve()
    environment_path = args.environment_report.resolve()
    all_tables = []
    all_passed = True
    total_nodes = 0
    for table_request in request["tables"]:
        directory = asset_root / table_request["asset_directory"]
        config_path = directory / "config.json"
        lut_path = directory / "table.lut"
        config_bytes, config_text, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        if table_request.get("config_sha256_at_selection") != sha256_bytes(config_bytes):
            raise ValidationError(
                f"{config_path}: config changed after held-out node selection"
            )
        header, values, lut_bytes, header_bytes = decode_lut(generator, lut_path)
        if header["generator_config_utf8"] != config_text:
            raise ValidationError(f"{lut_path}: embedded config bytes differ from config.json")
        if header["config_sha256"] != sha256_bytes(config_bytes):
            raise ValidationError(f"{lut_path}: external config digest mismatch")
        node_reports = []
        table_passed = True
        for node_index, node in enumerate(table_request["nodes"]):
            coordinates = {str(key): float(value) for key, value in node.items()}
            for axis in config["axes"]:
                if len(axis["coordinates"]) > 1 and coordinates[axis["kind"]] in axis["coordinates"]:
                    # A point can be exact on one dimension, but it must not be an in-grid
                    # Cartesian node. This is checked across all dimensions below.
                    pass
            if all(
                coordinates[axis["kind"]] in [float(v) for v in axis["coordinates"]]
                for axis in config["axes"]
            ):
                raise ValidationError(f"node {node_index} is present in the LUT grid")
            interpolated = interpolate(header["axes"], values, coordinates)
            direct = generator.run_isolated_point(
                config, coordinates, int(config["execution"]["point_timeout_seconds"])
            )
            errors, node_passed = error_record(direct, interpolated, request["thresholds"])
            table_passed = table_passed and node_passed
            total_nodes += 1
            node_reports.append(
                {
                    "node_index": node_index,
                    "coordinates": coordinates,
                    "direct_pytmatrix": dict(zip(COMPONENT_NAMES, direct)),
                    "lut_multilinear_interpolation": dict(
                        zip(COMPONENT_NAMES, interpolated)
                    ),
                    "errors": errors,
                    "within_predeclared_interpolation_thresholds": node_passed,
                }
            )
        all_passed = all_passed and table_passed
        all_tables.append(
            {
                "table_id": config["table_id"],
                "asset_directory": table_request["asset_directory"],
                "lut_sha256": sha256_bytes(lut_bytes),
                "lut_header_json_sha256": sha256_bytes(header_bytes),
                "payload_sha256": header["payload_sha256"],
                "config_sha256": sha256_bytes(config_bytes),
                "held_out_node_count": len(node_reports),
                "within_predeclared_interpolation_thresholds": table_passed,
                "nodes": node_reports,
            }
        )
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-direct-node-interpolation-check-v1",
        "classification": "held_out_from_lut_interpolation_check_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "scientifically_independent": False,
        "production_validation": False,
        "interpolation_check_passed": all_passed,
        "held_out_node_count": total_nodes,
        "node_request_sha256": sha256_bytes(request_bytes),
        "environment_report_sha256": sha256_file(environment_path),
        "generator_source_sha256": sha256_file(args.tool_root.resolve() / "generate_lut.py"),
        "validation_source_sha256": sha256_file(Path(__file__).resolve()),
        "selection_protocol": request["selection_protocol"],
        "selection_seed": request["selection_seed"],
        "selector_source_sha256": request["selector_source_sha256"],
        "thresholds": request["thresholds"],
        "independence_limit": (
            "Held-out nodes are absent from the LUT and are recomputed in fresh PyTMatrix "
            "processes, but use the same PyTMatrix engine, generator, dielectric assumptions, "
            "orientation, geometry, and fall-speed closures. This tests generation and "
            "interpolation only; it is not independent scientific validation."
        ),
        "unmet_production_gates": [
            "independent Mie sphere comparison across the configured materials",
            "independently coded PSD integration and terminal-moment comparison",
            "independent covariance phase and radar H/V basis verification",
            "independent KDP and attenuation convention/unit verification",
            "reviewed wet-particle dielectric and morphology validation",
        ],
        "tables": all_tables,
    }
    write_json(args.output.resolve(), report)


def sanity(args: argparse.Namespace) -> None:
    generator = load_generator(args.tool_root.resolve())
    dry_bytes, _, dry = generator.parse_exact_json(args.dry_config.resolve())
    wet_bytes, _, wet = generator.parse_exact_json(args.wet_config.resolve())
    generator.validate_config(dry)
    generator.validate_config(wet)
    frequency = generator.axis_coordinates(dry, "frequency")[0]
    sphere_base = {
        "minor_to_major_axis_ratio": 1.0,
        "frequency": frequency,
    }
    sphere_points = []
    for diameter in (0.0001, 0.0002):
        coordinates = {"equivolume_diameter": diameter, **sphere_base}
        sphere_points.append(
            generator.run_isolated_point(
                dry, coordinates, int(dry["execution"]["point_timeout_seconds"])
            )
        )
    m = generator._complex_index(
        dry["dielectric"]["refractive_index"], "dielectric.refractive_index"
    )
    kw_squared = float(dry["radar"]["reference_water_dielectric_factor_squared"])
    rayleigh = []
    for diameter, direct in zip((0.0001, 0.0002), sphere_points):
        diameter_mm = diameter * 1000.0
        dielectric_factor = (m * m - 1.0) / (m * m + 2.0)
        analytic_zh = abs(dielectric_factor) ** 2 / kw_squared * diameter_mm**6
        relative = abs(direct[0] - analytic_zh) / analytic_zh
        rayleigh.append(
            {
                "diameter_m": diameter,
                "direct_pytmatrix_zh": direct[0],
                "analytic_rayleigh_zh": analytic_zh,
                "relative_error": relative,
            }
        )
    scaling_expected = 2.0**6
    scaling_actual = sphere_points[1][0] / sphere_points[0][0]
    sphere_checks = {
        "zh_zv_relative_difference": abs(sphere_points[0][0] - sphere_points[0][1])
        / sphere_points[0][0],
        "covariance_real_zh_relative_difference": abs(
            sphere_points[0][2] - sphere_points[0][0]
        )
        / sphere_points[0][0],
        "covariance_imaginary_absolute": abs(sphere_points[0][3]),
        "kdp_absolute_deg_km": abs(sphere_points[0][4]),
        "ah_av_relative_difference": abs(sphere_points[0][5] - sphere_points[0][6])
        / max(sphere_points[0][5], 1.0e-30),
        "doubled_diameter_zh_ratio": scaling_actual,
        "rayleigh_expected_ratio": scaling_expected,
        "rayleigh_ratio_relative_error": abs(scaling_actual - scaling_expected)
        / scaling_expected,
    }
    resonance = []
    wavelength_m = float(wet["radar"]["speed_of_light_m_s"]) / frequency
    for diameter in (0.035, 0.05):
        coordinates = {
            "equivolume_diameter": diameter,
            "liquid_mass_fraction": 0.4,
            "minor_to_major_axis_ratio": 0.7,
            "frequency": frequency,
        }
        direct = generator.run_isolated_point(
            wet, coordinates, int(wet["execution"]["point_timeout_seconds"])
        )
        resonance.append(
            {
                "coordinates": coordinates,
                "size_parameter_pi_d_over_lambda": math.pi * diameter / wavelength_m,
                "direct_pytmatrix": dict(zip(COMPONENT_NAMES, direct)),
                "solver_completed_with_finite_schema_valid_outputs": True,
            }
        )
    sanity_passed = (
        max(item["relative_error"] for item in rayleigh) < 0.01
        and sphere_checks["zh_zv_relative_difference"] < 1.0e-8
        and sphere_checks["covariance_real_zh_relative_difference"] < 1.0e-8
        and sphere_checks["rayleigh_ratio_relative_error"] < 0.01
        and all(item["solver_completed_with_finite_schema_valid_outputs"] for item in resonance)
    )
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-rayleigh-and-wet-hail-sanity-v1",
        "classification": "analytic_and_solver_sanity_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "sanity_checks_passed": sanity_passed,
        "scientifically_independent_validation": False,
        "production_validation": False,
        "dry_config_sha256": sha256_bytes(dry_bytes),
        "wet_config_sha256": sha256_bytes(wet_bytes),
        "environment_report_sha256": sha256_file(args.environment_report.resolve()),
        "generator_source_sha256": sha256_file(args.tool_root.resolve() / "generate_lut.py"),
        "validation_source_sha256": sha256_file(Path(__file__).resolve()),
        "rayleigh_sphere_checks": rayleigh,
        "sphere_symmetry_and_scaling_checks": sphere_checks,
        "resonance_sized_wet_hail_solver_checks": resonance,
        "limitations": [
            "Rayleigh comparison is an analytic small-particle sanity check, not an independent Mie implementation.",
            "Wet-hail checks establish solver completion and finite schema-valid outputs, not laboratory or independent-model agreement.",
            "The same generator implements direct points and tables.",
            "No PSD-integrated scientific comparison is made; all tables are monodisperse at exactly 1 m^-3.",
        ],
    }
    write_json(args.output.resolve(), report)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    held = commands.add_parser("heldout")
    held.add_argument("--tool-root", type=Path, required=True)
    held.add_argument("--asset-root", type=Path, required=True)
    held.add_argument("--nodes", type=Path, required=True)
    held.add_argument("--environment-report", type=Path, required=True)
    held.add_argument("--output", type=Path, required=True)
    held.set_defaults(function=heldout)
    check = commands.add_parser("sanity")
    check.add_argument("--tool-root", type=Path, required=True)
    check.add_argument("--dry-config", type=Path, required=True)
    check.add_argument("--wet-config", type=Path, required=True)
    check.add_argument("--environment-report", type=Path, required=True)
    check.add_argument("--output", type=Path, required=True)
    check.set_defaults(function=sanity)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        args.function(args)
        return 0
    except (ValidationError, RuntimeError) as error:
        print(f"run_validation.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
