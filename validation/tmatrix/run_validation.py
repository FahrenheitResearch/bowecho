#!/usr/bin/env python3
"""Interpolation-only held-out checks and non-production physical sanity tests."""

from __future__ import annotations

import argparse
import concurrent.futures
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

PROPERTY_ASSET_DIRECTORIES = (
    "property_p3_ishmael_dry_oblate_sband_unvalidated",
    "property_p3_ishmael_dry_prolate_sband_unvalidated",
    "property_p3_ishmael_wet_oblate_sband_unvalidated",
    "property_p3_ishmael_wet_prolate_sband_unvalidated",
    "property_rain_sband_unvalidated",
)

# Union of the optional 0.1-degree cut, the historical 14-cut ladder, and all
# distinct Build-24 base-pattern centers in VCPs 12/34/35/112/212/215.
PROPERTY_VIEW_CENTERS_DEG = (
    0.1,
    0.5,
    0.9,
    1.3,
    1.8,
    2.4,
    3.1,
    4.0,
    4.5,
    5.1,
    6.4,
    8.0,
    10.0,
    12.0,
    12.5,
    14.0,
    15.6,
    16.7,
    19.5,
)
PROPERTY_DEFAULT_BEAM_FWHM_DEG = 0.95
PROPERTY_DEFAULT_BEAM_SIGMA_DEG = PROPERTY_DEFAULT_BEAM_FWHM_DEG / (
    2.0 * math.sqrt(2.0 * math.log(2.0))
)
PROPERTY_VIEW_THRESHOLDS = {
    "zh": {"relative": 0.10, "absolute": 1.0e-9},
    "zv": {"relative": 0.10, "absolute": 1.0e-9},
    "hh_vv_covariance_real": {"relative": 0.10, "absolute": 1.0e-9},
    "hh_vv_covariance_imaginary": {"relative": 0.25, "absolute": 1.0e-10},
    "kdp": {"relative": 0.25, "absolute": 1.0e-10},
    "ah": {"relative": 0.25, "absolute": 1.0e-10},
    "av": {"relative": 0.25, "absolute": 1.0e-10},
    "fall_speed_first_moment": {"relative": 0.10, "absolute": 1.0e-9},
    "fall_speed_second_moment": {"relative": 0.10, "absolute": 1.0e-9},
}
SOLVER_CONVERGENCE_NDGS = (12, 14)
SOLVER_CONVERGENCE_RELATIVE_TOLERANCE = 1.0e-3
SOLVER_CONVERGENCE_ABSOLUTE_TOLERANCE = 1.0e-12
SOLVER_CONVERGENCE_WORKERS = 12
SOLVER_CONVERGENCE_MAX_RECORDED_FAILURES = 100


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
    selector_filename = request.get("selector_filename", "select_held_out_nodes.py")
    if selector_filename not in {
        "select_held_out_nodes.py",
        "select_grid_design_nodes.py",
    }:
        raise ValidationError("held-out request has an unrecognized selector filename")
    selector_path = Path(__file__).resolve().with_name(selector_filename)
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
        node_metadata = table_request.get("node_metadata")
        if node_metadata is not None and len(node_metadata) != len(table_request["nodes"]):
            raise ValidationError("held-out node metadata length mismatch")
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
            node_report = {
                    "node_index": node_index,
                    "coordinates": coordinates,
                    "direct_pytmatrix": dict(zip(COMPONENT_NAMES, direct)),
                    "lut_multilinear_interpolation": dict(
                        zip(COMPONENT_NAMES, interpolated)
                    ),
                    "errors": errors,
                    "within_predeclared_interpolation_thresholds": node_passed,
                }
            if node_metadata is not None:
                node_report["selection_metadata"] = node_metadata[node_index]
            node_reports.append(node_report)
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
        "selector_filename": selector_filename,
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
        "radar_elevation": 0.0,
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
            "radar_elevation": 0.0,
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


def property_sanity(args: argparse.Namespace) -> None:
    generator = load_generator(args.tool_root.resolve())
    asset_root = args.asset_root.resolve()
    directories = (
        "property_p3_ishmael_dry_oblate_sband_unvalidated",
        "property_p3_ishmael_dry_prolate_sband_unvalidated",
        "property_p3_ishmael_wet_oblate_sband_unvalidated",
        "property_p3_ishmael_wet_prolate_sband_unvalidated",
        "property_rain_sband_unvalidated",
    )
    configs: dict[str, dict[str, Any]] = {}
    config_hashes = {}
    for directory in directories:
        config_bytes, _, config = generator.parse_exact_json(
            asset_root / directory / "config.json"
        )
        generator.validate_config(config)
        configs[directory] = config
        config_hashes[directory] = sha256_bytes(config_bytes)

    extreme_coordinates = {
        directories[0]: {
            "equivolume_diameter": generator.axis_coordinates(
                configs[directories[0]], "equivolume_diameter"
            )[-1],
            "temperature": 273.15,
            "bulk_density": 917.0,
            "minor_to_major_axis_ratio": 0.1,
            "frequency": 2.8e9,
            "radar_elevation": 20.0,
        },
        directories[1]: {
            "equivolume_diameter": generator.axis_coordinates(
                configs[directories[1]], "equivolume_diameter"
            )[-1],
            "temperature": 190.0,
            "bulk_density": 1.5,
            "minor_to_major_axis_ratio": 0.1,
            "frequency": 2.8e9,
            "radar_elevation": -0.5,
        },
        directories[2]: {
            "equivolume_diameter": generator.axis_coordinates(
                configs[directories[2]], "equivolume_diameter"
            )[-1],
            "temperature": 275.15,
            "condensed_volume_fraction": 1.0,
            "liquid_mass_fraction": 0.98,
            "minor_to_major_axis_ratio": 0.1,
            "frequency": 2.8e9,
            "radar_elevation": 20.0,
        },
        directories[3]: {
            "equivolume_diameter": generator.axis_coordinates(
                configs[directories[3]], "equivolume_diameter"
            )[-1],
            "temperature": 275.15,
            "condensed_volume_fraction": 1.0,
            "liquid_mass_fraction": 0.98,
            "minor_to_major_axis_ratio": 0.1,
            "frequency": 2.8e9,
            "radar_elevation": 20.0,
        },
        directories[4]: {
            "equivolume_diameter": 0.007,
            "temperature": 313.15,
            "minor_to_major_axis_ratio": 0.5,
            "frequency": 2.8e9,
            "radar_elevation": 20.0,
        },
    }
    solver_probes = []
    for directory in directories:
        config = configs[directory]
        coordinates = extreme_coordinates[directory]
        try:
            direct = generator.run_isolated_point(
                config,
                coordinates,
                int(config["execution"]["point_timeout_seconds"]),
            )
            solver_probes.append(
                {
                    "asset_directory": directory,
                    "coordinates": coordinates,
                    "solver_completed_with_finite_schema_valid_outputs": True,
                    "direct_pytmatrix": dict(zip(COMPONENT_NAMES, direct)),
                }
            )
        except BaseException as error:
            solver_probes.append(
                {
                    "asset_directory": directory,
                    "coordinates": coordinates,
                    "solver_completed_with_finite_schema_valid_outputs": False,
                    "failure": f"{type(error).__name__}: {error}",
                }
            )

    sphere_coordinates = {
        "equivolume_diameter": 0.004,
        "temperature": 260.0,
        "bulk_density": 100.0,
        "minor_to_major_axis_ratio": 1.0,
        "frequency": 2.8e9,
        "radar_elevation": 4.5,
    }
    sphere_values = []
    for directory in directories[:2]:
        config = configs[directory]
        sphere_values.append(
            generator.run_isolated_point(
                config,
                sphere_coordinates,
                int(config["execution"]["point_timeout_seconds"]),
            )
        )
    sphere_shape_relative_differences = {
        name: abs(oblate - prolate) / max(abs(oblate), 1.0e-30)
        for name, oblate, prolate in zip(
            COMPONENT_NAMES, sphere_values[0], sphere_values[1]
        )
    }

    reference_frequency = 2700832954.954955
    ice = generator._ice_permittivity_matzler_2006(273.15, reference_frequency)
    water = generator._water_permittivity_liebe_1991(273.15, reference_frequency)
    dielectric_golden = {
        "frequency_hz": reference_frequency,
        "temperature_k": 273.15,
        "ice_relative_permittivity": {"real": ice.real, "imaginary": ice.imag},
        "water_relative_permittivity": {
            "real": water.real,
            "imaginary": water.imag,
        },
        "within_published_formula_golden_tolerance": (
            abs(ice - complex(3.1885365, 0.00048568435)) < 1.0e-9
            and abs(water - complex(80.8523695, 22.8676989)) < 1.0e-6
        ),
    }
    mass_accounting_cases = []
    for total, paired in ((1.0, []), (1.0, [0.2, 0.3]), (1.0, [0.4, 0.6])):
        residual = generator.residual_rain_mass_after_wet_pairing(total, paired)
        mass_accounting_cases.append(
            {
                "total_rain_mass": total,
                "paired_liquid_masses": paired,
                "residual_rain_mass": residual,
                "mass_closes_without_double_count": (
                    abs(math.fsum(paired) + residual - total) <= 1.0e-15
                ),
            }
        )
    over_pairing_rejected = False
    try:
        generator.residual_rain_mass_after_wet_pairing(1.0, [0.8, 0.3])
    except generator.GeneratorError:
        over_pairing_rejected = True

    passed = (
        all(
            item["solver_completed_with_finite_schema_valid_outputs"]
            for item in solver_probes
        )
        and max(sphere_shape_relative_differences.values()) < 1.0e-8
        and dielectric_golden["within_published_formula_golden_tolerance"]
        and all(item["mass_closes_without_double_count"] for item in mass_accounting_cases)
        and over_pairing_rejected
    )
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-property-material-view-sanity-v1",
        "classification": "analytic_contract_and_solver_sanity_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "sanity_checks_passed": passed,
        "scientifically_independent_validation": False,
        "production_validation": False,
        "config_sha256": config_hashes,
        "config_snapshots": configs,
        "environment_report_sha256": sha256_file(args.environment_report.resolve()),
        "generator_source_sha256": sha256_file(args.tool_root.resolve() / "generate_lut.py"),
        "validation_source_sha256": sha256_file(Path(__file__).resolve()),
        "extreme_solver_probes": solver_probes,
        "sphere_oblate_prolate_identity_relative_differences": (
            sphere_shape_relative_differences
        ),
        "dielectric_formula_golden": dielectric_golden,
        "rain_mass_accounting_cases": mass_accounting_cases,
        "rain_over_pairing_rejected": over_pairing_rejected,
        "limitations": [
            "These checks use the same PyTMatrix generator and are not independent scattering validation.",
            "The symmetric Bruggeman topology is an explicit research approximation, not measured particle morphology.",
            "The tables represent one characteristic monodisperse particle at exactly 1 m^-3, not a scheme-native PSD integral.",
            "Radar-elevation checks remain in the configured axisymmetric Gaussian20 PPI basis, not a general body-frame transform.",
        ],
    }
    write_json(args.output.resolve(), report)


def solver_convergence(args: argparse.Namespace) -> None:
    """Compare ndgs=12 and ndgs=14 at every refined-v8 property grid point."""
    generator = load_generator(args.tool_root.resolve())
    asset_root = args.asset_root.resolve()
    environment_path = args.environment_report.resolve()
    validation_root = Path(__file__).resolve().parent
    generator_path = args.tool_root.resolve() / "generate_lut.py"
    validation_path = Path(__file__).resolve()
    grid_design_validation_path = validation_root / "run_grid_design_axis_budget.py"
    grid_design_audit_path = validation_root / "refined_grid_v9_full_axis_budget_report.json"
    initial_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "validation_source_sha256": sha256_file(validation_path),
        "environment_report_sha256": sha256_file(environment_path),
        "grid_design_validation_source_sha256": sha256_file(
            grid_design_validation_path
        ),
        "grid_design_audit_sha256": sha256_file(grid_design_audit_path),
    }
    environment = json.loads(environment_path.read_text(encoding="utf-8"))
    if environment.get("tool_file_sha256", {}).get("generate_lut.py") != initial_hashes[
        "generator_source_sha256"
    ]:
        raise ValidationError("environment report does not describe this generator source")
    grid_design_audit_bytes = grid_design_audit_path.read_bytes()
    grid_design_audit = json.loads(grid_design_audit_bytes)
    if grid_design_audit.get("axis_budget_check_passed") is not True:
        raise ValidationError("refined-v9 full grid-design audit did not pass")
    if grid_design_audit.get("generator_source_sha256") != initial_hashes[
        "generator_source_sha256"
    ]:
        raise ValidationError("generator changed after refined-v9 grid-design audit")
    if grid_design_audit.get("validation_source_sha256") != initial_hashes[
        "grid_design_validation_source_sha256"
    ]:
        raise ValidationError("grid-design validation source changed after its audit")
    if grid_design_audit.get("environment_report_sha256") != initial_hashes[
        "environment_report_sha256"
    ]:
        raise ValidationError("grid-design audit used a different environment report")
    audit_config_sha256 = {
        table["asset_directory"]: table["config_sha256"]
        for table in grid_design_audit["tables"]
    }
    if not set(PROPERTY_ASSET_DIRECTORIES).issubset(audit_config_sha256):
        raise ValidationError("refined-v9 audit does not cover all property configs")
    design_lineage_files = (
        "dense_grid_v3_design_failure_report.json",
        "dense_grid_v3_axis_audit_report.json",
        "refined_grid_v4_axis_budget_report.json",
        "refined_grid_v5_axis_budget_report.json",
        "refined_grid_v6_axis_budget_report.json",
        "refined_grid_v7_axis_budget_report.json",
        "refined_grid_v8_narrow_axis_budget_report.json",
        "refined_grid_v8_stale_environment_axis_budget_report.json",
        "refined_grid_v9_full_axis_budget_report.json",
    )
    design_lineage = [
        {
            "file": filename,
            "sha256": sha256_file(validation_root / filename),
        }
        for filename in design_lineage_files
    ]
    solver_domain_evidence_files = (
        "solver_ndgs12_to14_refined_v8_failure_report.json",
        "dry_prolate_rejected_points_preliminary_12_14_16_20_probe_report.json",
        "dry_prolate_rejected_points_12_14_16_18_20_probe_report.json",
        "dry_prolate_removed_diameter_parent_interval_audit_report.json",
        "dry_prolate_ddelt_sensitivity_probe_report.json",
    )
    solver_domain_evidence = [
        {
            "file": filename,
            "sha256": sha256_file(validation_root / filename),
        }
        for filename in solver_domain_evidence_files
    ]
    table_reports = []
    all_passed = True
    total_grid_points = 0
    total_component_comparisons = 0
    config_path_sha256: dict[Path, str] = {}

    for directory in PROPERTY_ASSET_DIRECTORIES:
        config_path = asset_root / directory / "config.json"
        config_path_sha256[config_path] = sha256_file(config_path)
        config_bytes, _, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        if audit_config_sha256[directory] != sha256_bytes(config_bytes):
            raise ValidationError(f"{directory}: config changed after refined-v9 audit")
        if int(config["radar"]["solver"]["ndgs"]) != SOLVER_CONVERGENCE_NDGS[1]:
            raise ValidationError(
                f"{directory}: final configured ndgs must equal {SOLVER_CONVERGENCE_NDGS[1]}"
            )
        if float(config["radar"]["solver"]["ddelt"]) != 0.001:
            raise ValidationError(f"{directory}: final ddelt must equal 0.001")

        coordinates = list(generator.point_coordinates(config))
        grouping = config["execution"]["grouping"]
        material_axes = tuple(grouping["material_state_axis_kinds"])
        grouped: dict[
            tuple[float, ...], list[tuple[int, dict[str, float]]]
        ] = {}
        for flat_index, point in enumerate(coordinates):
            key = tuple(float(point[kind]) for kind in material_axes)
            grouped.setdefault(key, []).append((flat_index, point))
        group_items = list(grouped.items())
        group_results: list[dict[str, Any] | None] = [None] * len(group_items)

        def evaluate_group(
            indexed_group: tuple[
                int, tuple[tuple[float, ...], list[tuple[int, dict[str, float]]]]
            ],
        ) -> tuple[int, dict[str, Any]]:
            group_index, (material_key, entries) = indexed_group
            points = [point for _, point in entries]
            timeout = int(grouping["group_timeout_seconds"])
            try:
                lower = generator.run_isolated_solver_ndgs_comparison_group(
                    config, points, SOLVER_CONVERGENCE_NDGS[0], timeout
                )
                upper = generator.run_isolated_solver_ndgs_comparison_group(
                    config, points, SOLVER_CONVERGENCE_NDGS[1], timeout
                )
                return group_index, {
                    "material_key": material_key,
                    "entries": entries,
                    "lower": lower,
                    "upper": upper,
                }
            except BaseException as error:
                return group_index, {
                    "material_key": material_key,
                    "entries": entries,
                    "failure": f"{type(error).__name__}: {error}",
                }

        completed_groups = 0
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=SOLVER_CONVERGENCE_WORKERS
        ) as executor:
            futures = [
                executor.submit(evaluate_group, item)
                for item in enumerate(group_items)
            ]
            for future in concurrent.futures.as_completed(futures):
                group_index, result = future.result()
                group_results[group_index] = result
                completed_groups += 1
                print(
                    f"solver convergence {directory}: "
                    f"{completed_groups}/{len(group_items)} material groups",
                    flush=True,
                )

        worst_by_component: list[dict[str, Any] | None] = [None] * len(
            COMPONENT_NAMES
        )
        failure_count = 0
        recorded_failures = []
        solver_group_failures = []
        table_passed = True
        compared_points = 0
        for group_index, result in enumerate(group_results):
            if result is None:
                raise ValidationError(
                    f"{directory}: convergence executor omitted group {group_index}"
                )
            material_coordinates = dict(zip(material_axes, result["material_key"]))
            if "failure" in result:
                table_passed = False
                solver_group_failures.append(
                    {
                        "group_index": group_index,
                        "material_coordinates": material_coordinates,
                        "failure": result["failure"],
                    }
                )
                continue
            for (flat_index, point), lower, upper in zip(
                result["entries"], result["lower"], result["upper"]
            ):
                compared_points += 1
                for component_index, (name, lower_value, upper_value) in enumerate(
                    zip(COMPONENT_NAMES, lower, upper)
                ):
                    absolute_difference = abs(float(upper_value) - float(lower_value))
                    scale = max(abs(float(lower_value)), abs(float(upper_value)))
                    if component_index in (2, 3):
                        scale = max(
                            math.hypot(float(lower[2]), float(lower[3])),
                            math.hypot(float(upper[2]), float(upper[3])),
                        )
                    allowed = SOLVER_CONVERGENCE_ABSOLUTE_TOLERANCE + (
                        SOLVER_CONVERGENCE_RELATIVE_TOLERANCE * scale
                    )
                    agreement_ratio = absolute_difference / allowed
                    within = agreement_ratio <= 1.0
                    record = {
                        "component": name,
                        "flat_grid_index": flat_index,
                        "coordinates": point,
                        "lower_ndgs": SOLVER_CONVERGENCE_NDGS[0],
                        "upper_ndgs": SOLVER_CONVERGENCE_NDGS[1],
                        "lower_value": float(lower_value),
                        "upper_value": float(upper_value),
                        "absolute_difference": absolute_difference,
                        "allowed_absolute_difference": allowed,
                        "difference_to_allowed_ratio": agreement_ratio,
                        "within_predeclared_tolerance": within,
                    }
                    previous = worst_by_component[component_index]
                    if previous is None or agreement_ratio > float(
                        previous["difference_to_allowed_ratio"]
                    ):
                        worst_by_component[component_index] = record
                    if not within:
                        table_passed = False
                        failure_count += 1
                        if (
                            len(recorded_failures)
                            < SOLVER_CONVERGENCE_MAX_RECORDED_FAILURES
                        ):
                            recorded_failures.append(record)

        grid_points = len(coordinates)
        component_comparisons = compared_points * len(COMPONENT_NAMES)
        total_grid_points += grid_points
        total_component_comparisons += component_comparisons
        all_passed = all_passed and table_passed
        table_reports.append(
            {
                "table_id": config["table_id"],
                "asset_directory": directory,
                "config_sha256": sha256_bytes(config_bytes),
                "configured_solver": config["radar"]["solver"],
                "grid_point_count": grid_points,
                "compared_grid_point_count": compared_points,
                "material_group_count": len(group_items),
                "worker_limit": SOLVER_CONVERGENCE_WORKERS,
                "effective_worker_count": min(
                    SOLVER_CONVERGENCE_WORKERS, len(group_items)
                ),
                "component_comparison_count": component_comparisons,
                "solver_group_failure_count": len(solver_group_failures),
                "solver_group_failures": solver_group_failures,
                "component_tolerance_failure_count": failure_count,
                "recorded_component_tolerance_failures": recorded_failures,
                "recorded_failure_limit": SOLVER_CONVERGENCE_MAX_RECORDED_FAILURES,
                "worst_comparison_by_component": worst_by_component,
                "within_predeclared_solver_convergence_tolerance": table_passed,
            }
        )

    final_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "validation_source_sha256": sha256_file(validation_path),
        "environment_report_sha256": sha256_file(environment_path),
        "grid_design_validation_source_sha256": sha256_file(
            grid_design_validation_path
        ),
        "grid_design_audit_sha256": sha256_file(grid_design_audit_path),
    }
    if final_hashes != initial_hashes:
        raise ValidationError("convergence inputs changed while solver work was running")
    for config_path, initial_sha256 in config_path_sha256.items():
        if sha256_file(config_path) != initial_sha256:
            raise ValidationError(
                f"config changed while convergence was running: {config_path}"
            )
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-property-refined-v9-all-grid-ndgs12-to14-v4",
        "classification": "native_solver_resolution_convergence_check_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "scientifically_independent": False,
        "production_validation": False,
        "solver_convergence_check_passed": all_passed,
        "comparison_solver_ndgs": list(SOLVER_CONVERGENCE_NDGS),
        "selected_final_solver_ndgs": SOLVER_CONVERGENCE_NDGS[1],
        "selected_final_solver_ddelt": 0.001,
        "predeclared_component_criterion": (
            "For every scalar except covariance real/imaginary: "
            "abs(value_high-value_low) <= 1e-12 + "
            "1e-3*max(abs(value_low),abs(value_high)). For each covariance "
            "component, the scale is max(hypot(cov_real_low,cov_imag_low),"
            "hypot(cov_real_high,cov_imag_high)), so numerical phase noise in "
            "a physically zero imaginary component is judged against the "
            "converged complex covariance rather than against zero."
        ),
        "relative_tolerance": SOLVER_CONVERGENCE_RELATIVE_TOLERANCE,
        "absolute_tolerance": SOLVER_CONVERGENCE_ABSOLUTE_TOLERANCE,
        "scope": (
            "Every Cartesian grid point in all five property configs, "
            "including every declared particle, material, frequency, and radar-view node."
        ),
        "worker_limit": SOLVER_CONVERGENCE_WORKERS,
        "effective_worker_count": SOLVER_CONVERGENCE_WORKERS,
        "refined_grid_design_audit_file": grid_design_audit_path.name,
        "refined_grid_design_audit_sha256": sha256_bytes(grid_design_audit_bytes),
        "refined_grid_design_audit_sample_count": grid_design_audit["sample_count"],
        "interpolation_grid_design_lineage": design_lineage,
        "solver_domain_design_evidence": solver_domain_evidence,
        "total_grid_point_count": total_grid_points,
        "total_component_comparison_count": total_component_comparisons,
        **initial_hashes,
        "candidate_selection_history": (
            "Exploratory crash-isolated probes attempted diameters above 50 mm and "
            "then scanned each phase/shape role at q=0.1. Role-specific candidate "
            "ceilings were initially fixed at 89 mm dry-oblate, 50 mm "
            "dry-prolate, 15 mm wet-oblate, and 6.3 mm wet-prolate. The immediately "
            "higher retained probes failed at 90, 51, 15.5, and 6.325 mm, respectively. "
            "After the 104,960-point candidate passed solver convergence, seeded and "
            "systematic interpolation design checks exposed coarse density, condensed-"
            "fraction, liquid-fraction, diameter, and narrow resonance cells. The v3 "
            "through v8 failures were preserved. The first exhaustive refined grid then "
            "found three dry-prolate near-zero KDP disagreements at the inserted "
            "36.12562655 mm node. Removing that node while retaining the 50 mm domain "
            "failed 2,195 interpolation component budgets across all declared states, "
            "and higher solver-order probes were pairwise nonmonotonic. The rigorous "
            "fail-closed resolution caps dry-prolate at the last all-grid-converged "
            "32.31174268 mm node, so interpolation cannot bridge the unresolved resonance."
        ),
        "post_pass_policy": (
            "A passing result freezes these solver and grid configs before LUT "
            "generation and held-out selection. Later held-out outputs may not tune "
            "the generated tables. Any failed rerun requires a separately retained report."
        ),
        "independence_limit": (
            "Both sides use the same PyTMatrix implementation and differ only in the "
            "PyTMatrix ndgs shape-integration resolution. This is numerical convergence "
            "evidence, not independent scattering validation."
        ),
        "tables": table_reports,
    }
    write_json(args.output.resolve(), report)


def property_view(args: argparse.Namespace) -> None:
    """Check view-axis interpolation at every supported center and +/- sigma."""
    generator = load_generator(args.tool_root.resolve())
    asset_root = args.asset_root.resolve()
    environment_path = args.environment_report.resolve()
    base_coordinates = {
        PROPERTY_ASSET_DIRECTORIES[0]: {
            "equivolume_diameter": 0.0027755575615628914,
            "temperature": 260.0,
            "bulk_density": 400.0,
            "minor_to_major_axis_ratio": 0.7,
            "frequency": 2.8e9,
        },
        PROPERTY_ASSET_DIRECTORIES[1]: {
            "equivolume_diameter": 0.0027755575615628914,
            "temperature": 260.0,
            "bulk_density": 400.0,
            "minor_to_major_axis_ratio": 0.7,
            "frequency": 2.8e9,
        },
        PROPERTY_ASSET_DIRECTORIES[2]: {
            "equivolume_diameter": 0.0027755575615628914,
            "temperature": 273.15,
            "condensed_volume_fraction": 0.5,
            "liquid_mass_fraction": 0.6,
            "minor_to_major_axis_ratio": 0.7,
            "frequency": 2.8e9,
        },
        PROPERTY_ASSET_DIRECTORIES[3]: {
            "equivolume_diameter": 0.0027755575615628914,
            "temperature": 273.15,
            "condensed_volume_fraction": 0.5,
            "liquid_mass_fraction": 0.6,
            "minor_to_major_axis_ratio": 0.7,
            "frequency": 2.8e9,
        },
        PROPERTY_ASSET_DIRECTORIES[4]: {
            "equivolume_diameter": 0.0026,
            "temperature": 293.15,
            "minor_to_major_axis_ratio": 0.85,
            "frequency": 2.8e9,
        },
    }
    view_samples = []
    for center in PROPERTY_VIEW_CENTERS_DEG:
        for offset_name, offset_sigma in (
            ("minus_one_sigma", -1.0),
            ("center", 0.0),
            ("plus_one_sigma", 1.0),
        ):
            view_samples.append(
                {
                    "center_elevation_deg": center,
                    "offset": offset_name,
                    "offset_sigma": offset_sigma,
                    "sample_elevation_deg": (
                        center + offset_sigma * PROPERTY_DEFAULT_BEAM_SIGMA_DEG
                    ),
                }
            )
    if len(PROPERTY_VIEW_CENTERS_DEG) != 19 or len(view_samples) != 57:
        raise ValidationError("property view coverage must contain 19 centers and 57 samples")

    all_passed = True
    table_reports = []
    for directory in PROPERTY_ASSET_DIRECTORIES:
        table_root = asset_root / directory
        config_path = table_root / "config.json"
        lut_path = table_root / "table.lut"
        config_bytes, config_text, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        header, values, lut_bytes, header_bytes = decode_lut(generator, lut_path)
        if header["generator_config_utf8"] != config_text:
            raise ValidationError(f"{lut_path}: embedded config bytes differ from config.json")
        if header["config_sha256"] != sha256_bytes(config_bytes):
            raise ValidationError(f"{lut_path}: external config digest mismatch")

        fixed = base_coordinates[directory]
        expected_nonview = {
            axis["kind"] for axis in config["axes"] if axis["kind"] != "radar_elevation"
        }
        if set(fixed) != expected_nonview:
            raise ValidationError(f"{directory}: view check fixed coordinates are incomplete")
        for axis in config["axes"]:
            if axis["kind"] == "radar_elevation":
                if axis["coordinates"][0] != -0.5 or axis["coordinates"][-1] != 20.0:
                    raise ValidationError(
                        f"{directory}: view axis must exactly span -0.5 through 20 degrees"
                    )
            elif fixed[axis["kind"]] not in [float(value) for value in axis["coordinates"]]:
                raise ValidationError(
                    f"{directory}: {axis['kind']} fixed coordinate is not an exact grid node"
                )

        points = [
            {**fixed, "radar_elevation": float(sample["sample_elevation_deg"])}
            for sample in view_samples
        ]
        for point in points:
            if not -0.5 <= point["radar_elevation"] <= 20.0:
                raise ValidationError(
                    f"{directory}: named center +/- sigma falls outside the declared view axis"
                )
        if generator._uses_material_state_grouping(config):
            direct_values = generator.run_isolated_material_state_group(
                config,
                points,
                int(config["execution"]["grouping"]["group_timeout_seconds"]),
            )
        else:
            direct_values = [
                generator.run_isolated_point(
                    config,
                    point,
                    int(config["execution"]["point_timeout_seconds"]),
                )
                for point in points
            ]

        sample_reports = []
        table_passed = True
        for sample, point, direct in zip(view_samples, points, direct_values):
            interpolated = interpolate(header["axes"], values, point)
            errors, sample_passed = error_record(
                direct, interpolated, PROPERTY_VIEW_THRESHOLDS
            )
            table_passed = table_passed and sample_passed
            sample_reports.append(
                {
                    **sample,
                    "direct_pytmatrix": dict(zip(COMPONENT_NAMES, direct)),
                    "lut_view_axis_interpolation": dict(
                        zip(COMPONENT_NAMES, interpolated)
                    ),
                    "errors": errors,
                    "within_predeclared_view_thresholds": sample_passed,
                }
            )
        all_passed = all_passed and table_passed
        table_reports.append(
            {
                "table_id": config["table_id"],
                "asset_directory": directory,
                "config_sha256": sha256_bytes(config_bytes),
                "lut_sha256": sha256_bytes(lut_bytes),
                "lut_header_json_sha256": sha256_bytes(header_bytes),
                "payload_sha256": header["payload_sha256"],
                "fixed_exact_grid_coordinates": fixed,
                "view_sample_count": len(sample_reports),
                "within_predeclared_view_thresholds": table_passed,
                "samples": sample_reports,
            }
        )

    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-property-build24-view-interpolation-v1",
        "classification": "direct_pytmatrix_view_axis_interpolation_check_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "scientifically_independent": False,
        "production_validation": False,
        "view_interpolation_check_passed": all_passed,
        "named_center_count": len(PROPERTY_VIEW_CENTERS_DEG),
        "samples_per_center": 3,
        "view_sample_count_per_table": len(view_samples),
        "total_view_sample_count": len(view_samples) * len(PROPERTY_ASSET_DIRECTORIES),
        "named_centers_deg": list(PROPERTY_VIEW_CENTERS_DEG),
        "center_provenance": (
            "Union of optional 0.1-degree cut, historical 14-cut ladder, and distinct "
            "WSR-88D Build-24 base-pattern centers for VCP 12/34/35/112/212/215."
        ),
        "default_beam_fwhm_deg": PROPERTY_DEFAULT_BEAM_FWHM_DEG,
        "default_beam_sigma_deg": PROPERTY_DEFAULT_BEAM_SIGMA_DEG,
        "sample_offsets_sigma": [-1.0, 0.0, 1.0],
        "thresholds": PROPERTY_VIEW_THRESHOLDS,
        "environment_report_sha256": sha256_file(environment_path),
        "generator_source_sha256": sha256_file(args.tool_root.resolve() / "generate_lut.py"),
        "validation_source_sha256": sha256_file(Path(__file__).resolve()),
        "independence_limit": (
            "Direct and interpolated paths share this generator, PyTMatrix, dielectric "
            "models, orientation quadrature, and geometry. Fixed particle/material "
            "coordinates are exact LUT nodes so this isolates only radar-elevation "
            "interpolation; it is not independent scientific validation."
        ),
        "tables": table_reports,
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
    property_check = commands.add_parser("property-sanity")
    property_check.add_argument("--tool-root", type=Path, required=True)
    property_check.add_argument("--asset-root", type=Path, required=True)
    property_check.add_argument("--environment-report", type=Path, required=True)
    property_check.add_argument("--output", type=Path, required=True)
    property_check.set_defaults(function=property_sanity)
    convergence_check = commands.add_parser("solver-convergence")
    convergence_check.add_argument("--tool-root", type=Path, required=True)
    convergence_check.add_argument("--asset-root", type=Path, required=True)
    convergence_check.add_argument("--environment-report", type=Path, required=True)
    convergence_check.add_argument("--output", type=Path, required=True)
    convergence_check.set_defaults(function=solver_convergence)
    property_view_check = commands.add_parser("property-view")
    property_view_check.add_argument("--tool-root", type=Path, required=True)
    property_view_check.add_argument("--asset-root", type=Path, required=True)
    property_view_check.add_argument("--environment-report", type=Path, required=True)
    property_view_check.add_argument("--output", type=Path, required=True)
    property_view_check.set_defaults(function=property_view)
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
