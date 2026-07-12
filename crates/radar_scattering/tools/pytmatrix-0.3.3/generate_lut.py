#!/usr/bin/env python3
"""Deterministic, process-isolated PyTMatrix 0.3.3 schema-v1 LUT generator.

The electromagnetic values are produced by official PyTMatrix 0.3.3.  A
small Rust emitter constructs the final file through radar_scattering's public
OfflineLut API, so this script does not duplicate the binary format writer.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import itertools
import json
import math
import os
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Iterable, Sequence


GENERATOR_VERSION = "1.0.0"
MAGIC = b"BRSLUT01"
SCHEMA_VERSION = 1
POINT_COMPONENT_COUNT = 9
POINT_MARKER = "BRSLUT_POINT_RESULT="

AXIS_UNITS = {
    "equivolume_diameter": "meter",
    "liquid_mass_fraction": "unitless_fraction",
    "minor_to_major_axis_ratio": "unitless_fraction",
    "frequency": "hertz",
}

OUTPUTS = [
    {"kind": "zh", "unit": "linear_reflectivity_millimeter6_per_meter3"},
    {"kind": "zv", "unit": "linear_reflectivity_millimeter6_per_meter3"},
    {
        "kind": "hh_vv_covariance_real",
        "unit": "linear_covariance_millimeter6_per_meter3",
    },
    {
        "kind": "hh_vv_covariance_imaginary",
        "unit": "linear_covariance_millimeter6_per_meter3",
    },
    {"kind": "kdp", "unit": "degree_per_kilometer"},
    {"kind": "ah", "unit": "decibel_per_kilometer"},
    {"kind": "av", "unit": "decibel_per_kilometer"},
    {
        "kind": "fall_speed_first_moment",
        "unit": "reflectivity_weighted_meter_per_second",
    },
    {
        "kind": "fall_speed_second_moment",
        "unit": "reflectivity_weighted_meter2_per_second2",
    },
]

TOOL_FILES = (
    "Dockerfile",
    "FAILURE_RECORD.md",
    "README.md",
    "generate_lut.py",
    "generator_config.example.json",
    "requirements-bootstrap-pinned.txt",
    "requirements-pytmatrix-pinned.txt",
    "run_all.ps1",
    "toolchain.json",
    "emitter/Cargo.toml",
    "emitter/Cargo.lock",
    "emitter/src/main.rs",
)


class GeneratorError(RuntimeError):
    """An input, solver, emitter, or audit check failed."""


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def f64_bits_hex(value: float) -> str:
    bits = struct.unpack("<Q", struct.pack("<d", float(value)))[0]
    return f"{bits:016x}"


def write_json(path: Path, value: Any) -> None:
    data = (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode(
        "utf-8"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def _reject_duplicate_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise GeneratorError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite_constant(value: str) -> None:
    raise GeneratorError(f"non-finite JSON number {value!r} is forbidden")


def parse_exact_json(path: Path) -> tuple[bytes, str, dict[str, Any]]:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise GeneratorError(f"{path}: config is not strict UTF-8: {error}") from error
    try:
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite_constant,
        )
    except (json.JSONDecodeError, GeneratorError) as error:
        raise GeneratorError(f"{path}: invalid strict JSON: {error}") from error
    if not isinstance(value, dict):
        raise GeneratorError(f"{path}: top-level config must be an object")
    return raw, text, value


def _require_object(value: Any, path: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise GeneratorError(f"{path} must be an object")
    return value


def _keys(
    value: dict[str, Any], *, required: Iterable[str], allowed: Iterable[str], path: str
) -> None:
    required_set = set(required)
    allowed_set = set(allowed)
    missing = sorted(required_set - value.keys())
    unknown = sorted(value.keys() - allowed_set)
    if missing:
        raise GeneratorError(f"{path} is missing keys: {', '.join(missing)}")
    if unknown:
        raise GeneratorError(f"{path} has unknown keys: {', '.join(unknown)}")


def _number(value: Any, path: str, *, positive: bool = False) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise GeneratorError(f"{path} must be a JSON number")
    result = float(value)
    if not math.isfinite(result):
        raise GeneratorError(f"{path} must be finite")
    if positive and result <= 0.0:
        raise GeneratorError(f"{path} must be positive")
    return result


def _positive_integer(value: Any, path: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise GeneratorError(f"{path} must be a positive integer")
    return value


def _text(value: Any, path: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise GeneratorError(f"{path} must be nonempty text")
    return value


def _complex_index(value: Any, path: str) -> complex:
    obj = _require_object(value, path)
    _keys(obj, required=("real", "imaginary"), allowed=("real", "imaginary"), path=path)
    real = _number(obj["real"], f"{path}.real", positive=True)
    imaginary = _number(obj["imaginary"], f"{path}.imaginary")
    if imaginary < 0.0:
        raise GeneratorError(f"{path}.imaginary must be nonnegative for passive media")
    return complex(real, imaginary)


def validate_config(config: dict[str, Any]) -> None:
    top_keys = (
        "schema",
        "status",
        "kernel",
        "table_id",
        "particle_population",
        "axes",
        "dielectric",
        "orientation",
        "radar",
        "terminal_velocity",
        "temporal",
        "execution",
        "payload",
        "references",
    )
    _keys(config, required=top_keys, allowed=top_keys, path="config")
    if config["schema"] != 1:
        raise GeneratorError("config.schema must equal 1")
    if config["status"] != "research_only_unvalidated":
        raise GeneratorError("config.status must be research_only_unvalidated")
    if config["kernel"] != "pytmatrix-0.3.3":
        raise GeneratorError("config.kernel must be pytmatrix-0.3.3")
    _text(config["table_id"], "config.table_id")

    population = _require_object(config["particle_population"], "particle_population")
    population_keys = (
        "microphysics_family",
        "category",
        "shape_family",
        "size_distribution",
        "normalization_number_concentration_m3",
    )
    _keys(population, required=population_keys, allowed=population_keys, path="particle_population")
    if population["microphysics_family"] != "conventional":
        raise GeneratorError("only conventional particle tables are supported")
    if population["category"] not in ("rain", "hail"):
        raise GeneratorError("particle_population.category must be rain or hail")
    if population["shape_family"] not in ("oblate_spheroid", "prolate_spheroid"):
        raise GeneratorError("shape_family must be oblate_spheroid or prolate_spheroid")
    if population["size_distribution"] != "monodisperse_node":
        raise GeneratorError("size_distribution must be monodisperse_node")
    concentration = _number(
        population["normalization_number_concentration_m3"],
        "particle_population.normalization_number_concentration_m3",
        positive=True,
    )
    if concentration != 1.0:
        raise GeneratorError("normalization number concentration must be exactly 1 m^-3")

    dielectric = _require_object(config["dielectric"], "dielectric")
    model = dielectric.get("model")
    if model == "explicit_homogeneous":
        required = (
            "model",
            "material",
            "refractive_index",
            "mass_density_kg_m3",
            "temperature_k",
            "frequency_dependence",
        )
        _keys(dielectric, required=required, allowed=required, path="dielectric")
        if dielectric["material"] not in ("liquid_water", "ice"):
            raise GeneratorError("homogeneous material must be liquid_water or ice")
        _complex_index(dielectric["refractive_index"], "dielectric.refractive_index")
        _number(dielectric["mass_density_kg_m3"], "dielectric.mass_density_kg_m3", positive=True)
        _number(dielectric["temperature_k"], "dielectric.temperature_k", positive=True)
        if dielectric["frequency_dependence"] != "constant_over_configured_s_band_nodes":
            raise GeneratorError("explicit dielectric frequency dependence must be declared constant")
    elif model == "maxwell_garnett_ice_host_water_inclusion":
        required = (
            "model",
            "ice_refractive_index",
            "liquid_water_refractive_index",
            "ice_density_kg_m3",
            "liquid_water_density_kg_m3",
            "temperature_k",
            "mass_to_volume_fraction_conversion",
            "frequency_dependence",
        )
        _keys(dielectric, required=required, allowed=required, path="dielectric")
        _complex_index(dielectric["ice_refractive_index"], "dielectric.ice_refractive_index")
        _complex_index(
            dielectric["liquid_water_refractive_index"],
            "dielectric.liquid_water_refractive_index",
        )
        _number(dielectric["ice_density_kg_m3"], "dielectric.ice_density_kg_m3", positive=True)
        _number(
            dielectric["liquid_water_density_kg_m3"],
            "dielectric.liquid_water_density_kg_m3",
            positive=True,
        )
        _number(dielectric["temperature_k"], "dielectric.temperature_k", positive=True)
        if dielectric["mass_to_volume_fraction_conversion"] != "component_specific_volume":
            raise GeneratorError("wet mixture must use component_specific_volume conversion")
        if dielectric["frequency_dependence"] != "constant_over_configured_s_band_nodes":
            raise GeneratorError("explicit dielectric frequency dependence must be declared constant")
    else:
        raise GeneratorError(f"unsupported dielectric.model {model!r}")

    axes = config["axes"]
    if not isinstance(axes, list):
        raise GeneratorError("axes must be an array")
    expected_kinds = ["equivolume_diameter"]
    if model == "maxwell_garnett_ice_host_water_inclusion":
        expected_kinds.append("liquid_mass_fraction")
    expected_kinds.extend(("minor_to_major_axis_ratio", "frequency"))
    if len(axes) != len(expected_kinds):
        raise GeneratorError(f"axes must be in exact order {expected_kinds}")
    for index, (axis_value, expected_kind) in enumerate(zip(axes, expected_kinds)):
        axis = _require_object(axis_value, f"axes[{index}]")
        _keys(axis, required=("kind", "unit", "coordinates"), allowed=("kind", "unit", "coordinates"), path=f"axes[{index}]")
        if axis["kind"] != expected_kind:
            raise GeneratorError(f"axes[{index}].kind must be {expected_kind}")
        if axis["unit"] != AXIS_UNITS[expected_kind]:
            raise GeneratorError(f"axes[{index}] has incorrect unit")
        coordinates = axis["coordinates"]
        if not isinstance(coordinates, list) or not coordinates:
            raise GeneratorError(f"axes[{index}].coordinates must be nonempty")
        numeric = [_number(v, f"axes[{index}].coordinates[{j}]") for j, v in enumerate(coordinates)]
        if any(a >= b for a, b in zip(numeric, numeric[1:])):
            raise GeneratorError(f"axes[{index}].coordinates must be strictly increasing")
        if expected_kind == "equivolume_diameter" and any(not (0.0 < v <= 0.1) for v in numeric):
            raise GeneratorError("diameters must lie in (0, 0.1] m")
        if expected_kind == "liquid_mass_fraction" and any(not (0.0 <= v <= 1.0) for v in numeric):
            raise GeneratorError("liquid mass fractions must lie in [0, 1]")
        if expected_kind == "minor_to_major_axis_ratio" and any(not (0.0 < v <= 1.0) for v in numeric):
            raise GeneratorError("minor-to-major ratios must lie in (0, 1]")
        if expected_kind == "frequency" and any(not (2.0e9 <= v <= 4.0e9) for v in numeric):
            raise GeneratorError("frequency nodes must remain in S band [2, 4] GHz")

    orientation = _require_object(config["orientation"], "orientation")
    if orientation.get("model") == "fixed_euler":
        orientation_keys = (
            "model",
            "yaw_deg",
            "pitch_deg",
            "roll_deg",
            "pytmatrix_alpha_deg",
            "pytmatrix_beta_deg",
            "symmetry_axis",
        )
        _keys(
            orientation,
            required=orientation_keys,
            allowed=orientation_keys,
            path="orientation",
        )
        for key in (
            "yaw_deg",
            "pitch_deg",
            "roll_deg",
            "pytmatrix_alpha_deg",
            "pytmatrix_beta_deg",
        ):
            if _number(orientation[key], f"orientation.{key}") != 0.0:
                raise GeneratorError(
                    f"orientation.{key} must be zero in this research generator"
                )
        if orientation["symmetry_axis"] != "vertical":
            raise GeneratorError("orientation.symmetry_axis must be vertical")
    elif orientation.get("model") == "gaussian_canting":
        orientation_keys = (
            "model",
            "mean_deg",
            "standard_deviation_deg",
            "alpha_quadrature_points",
            "beta_quadrature_points",
            "quadrature_method",
            "reference_symmetry_axis",
        )
        _keys(
            orientation,
            required=orientation_keys,
            allowed=orientation_keys,
            path="orientation",
        )
        mean = _number(orientation["mean_deg"], "orientation.mean_deg")
        if not 0.0 <= mean < 180.0:
            raise GeneratorError("orientation.mean_deg must lie in [0, 180)")
        _number(
            orientation["standard_deviation_deg"],
            "orientation.standard_deviation_deg",
            positive=True,
        )
        n_alpha = _positive_integer(
            orientation["alpha_quadrature_points"],
            "orientation.alpha_quadrature_points",
        )
        n_beta = _positive_integer(
            orientation["beta_quadrature_points"],
            "orientation.beta_quadrature_points",
        )
        if n_alpha * n_beta > 65_535:
            raise GeneratorError("total orientation quadrature points exceed u16")
        if orientation["quadrature_method"] != "pytmatrix_orient_averaged_fixed_gautschi":
            raise GeneratorError("unsupported Gaussian canting quadrature method")
        if orientation["reference_symmetry_axis"] != "vertical_at_zero_canting":
            raise GeneratorError("Gaussian reference symmetry axis must be vertical")
    else:
        raise GeneratorError("orientation.model must be fixed_euler or gaussian_canting")

    radar = _require_object(config["radar"], "radar")
    radar_keys = (
        "speed_of_light_m_s",
        "reference_water_dielectric_factor_squared",
        "length_unit_passed_to_pytmatrix",
        "backscatter_geometry_deg",
        "forward_scatter_geometry_deg",
        "covariance_phase_convention",
        "solver",
    )
    _keys(radar, required=radar_keys, allowed=radar_keys, path="radar")
    if _number(radar["speed_of_light_m_s"], "radar.speed_of_light_m_s", positive=True) != 299_792_458.0:
        raise GeneratorError("speed_of_light_m_s must be exactly 299792458")
    _number(
        radar["reference_water_dielectric_factor_squared"],
        "radar.reference_water_dielectric_factor_squared",
        positive=True,
    )
    if radar["length_unit_passed_to_pytmatrix"] != "millimeter":
        raise GeneratorError("PyTMatrix length unit must be millimeter")
    expected_back = [90.0, 90.0, 0.0, 180.0, 0.0, 0.0]
    expected_forward = [90.0, 90.0, 0.0, 0.0, 0.0, 0.0]
    if [_number(v, "radar.backscatter_geometry_deg") for v in radar["backscatter_geometry_deg"]] != expected_back:
        raise GeneratorError("backscatter geometry must equal PyTMatrix geom_horiz_back")
    if [_number(v, "radar.forward_scatter_geometry_deg") for v in radar["forward_scatter_geometry_deg"]] != expected_forward:
        raise GeneratorError("forward geometry must equal PyTMatrix geom_horiz_forw")
    if radar["covariance_phase_convention"] != "pytmatrix_delta_hv_hh_times_conjugate_vv":
        raise GeneratorError("unsupported covariance phase convention")
    solver = _require_object(radar["solver"], "radar.solver")
    _keys(solver, required=("shape", "ddelt", "ndgs"), allowed=("shape", "ddelt", "ndgs"), path="radar.solver")
    if solver["shape"] != "spheroid":
        raise GeneratorError("solver shape must be spheroid")
    _number(solver["ddelt"], "radar.solver.ddelt", positive=True)
    _positive_integer(solver["ndgs"], "radar.solver.ndgs")

    terminal = _require_object(config["terminal_velocity"], "terminal_velocity")
    law = terminal.get("law")
    if law == "atlas_rain_1973_exponential":
        keys = ("law", "a_m_s", "b_m_s", "c_per_mm", "valid_diameter_range_m")
        _keys(terminal, required=keys, allowed=keys, path="terminal_velocity")
        for key in ("a_m_s", "b_m_s", "c_per_mm"):
            _number(terminal[key], f"terminal_velocity.{key}", positive=True)
        valid_range = terminal["valid_diameter_range_m"]
        if not isinstance(valid_range, list) or len(valid_range) != 2:
            raise GeneratorError("terminal velocity valid range must have two values")
        low, high = [_number(v, "terminal_velocity.valid_diameter_range_m", positive=True) for v in valid_range]
        diameters = axis_coordinates(config, "equivolume_diameter")
        if low >= high or diameters[0] < low or diameters[-1] > high:
            raise GeneratorError("diameter axis exceeds the configured Atlas-law validity range")
    elif law == "schiller_naumann_gravity_drag":
        keys = (
            "law",
            "gravity_m_s2",
            "air_density_kg_m3",
            "air_dynamic_viscosity_pa_s",
            "drag_transition_reynolds",
            "high_reynolds_drag_coefficient",
            "maximum_iterations",
            "relative_tolerance",
        )
        _keys(terminal, required=keys, allowed=keys, path="terminal_velocity")
        for key in (
            "gravity_m_s2",
            "air_density_kg_m3",
            "air_dynamic_viscosity_pa_s",
            "drag_transition_reynolds",
            "high_reynolds_drag_coefficient",
            "relative_tolerance",
        ):
            _number(terminal[key], f"terminal_velocity.{key}", positive=True)
        _positive_integer(terminal["maximum_iterations"], "terminal_velocity.maximum_iterations")
    else:
        raise GeneratorError(f"unsupported terminal_velocity.law {law!r}")

    temporal = _require_object(config["temporal"], "temporal")
    _keys(temporal, required=("sampling",), allowed=("sampling",), path="temporal")
    if temporal["sampling"] != "instantaneous":
        raise GeneratorError("temporal sampling must be instantaneous")
    execution = _require_object(config["execution"], "execution")
    execution_keys = (
        "point_timeout_seconds",
        "process_isolation",
        "result_collection_order",
        "partial_grid_policy",
        "thread_count_per_process",
    )
    _keys(execution, required=execution_keys, allowed=execution_keys, path="execution")
    _positive_integer(execution["point_timeout_seconds"], "execution.point_timeout_seconds")
    if execution["process_isolation"] != "fresh_python_subprocess_per_grid_point":
        raise GeneratorError("each point must use a fresh Python subprocess")
    if execution["result_collection_order"] != "declared_axis_order_last_axis_fastest":
        raise GeneratorError("result collection order is not canonical")
    if execution["partial_grid_policy"] != "reject_entire_lut":
        raise GeneratorError("partial grids must be rejected")
    if execution["thread_count_per_process"] != 1:
        raise GeneratorError("thread_count_per_process must equal 1")
    payload = _require_object(config["payload"], "payload")
    _keys(payload, required=("encoding",), allowed=("encoding",), path="payload")
    if payload["encoding"] != "f64_le_point_major_last_axis_fastest":
        raise GeneratorError("payload encoding is not schema-v1 canonical")
    references = config["references"]
    if not isinstance(references, list) or not references or any(not isinstance(v, str) or not v for v in references):
        raise GeneratorError("references must be a nonempty string array")


def axis_coordinates(config: dict[str, Any], kind: str) -> list[float]:
    for axis in config["axes"]:
        if axis["kind"] == kind:
            return [float(value) for value in axis["coordinates"]]
    raise GeneratorError(f"required axis {kind!r} is missing")


def point_coordinates(config: dict[str, Any]) -> Iterable[dict[str, float]]:
    axes = config["axes"]
    for coordinates in itertools.product(*(axis["coordinates"] for axis in axes)):
        yield {axis["kind"]: float(value) for axis, value in zip(axes, coordinates)}


def _material(config: dict[str, Any], coordinates: dict[str, float]) -> tuple[complex, float]:
    dielectric = config["dielectric"]
    if dielectric["model"] == "explicit_homogeneous":
        return (
            _complex_index(dielectric["refractive_index"], "dielectric.refractive_index"),
            float(dielectric["mass_density_kg_m3"]),
        )

    liquid_mass_fraction = coordinates["liquid_mass_fraction"]
    ice_density = float(dielectric["ice_density_kg_m3"])
    water_density = float(dielectric["liquid_water_density_kg_m3"])
    ice_specific_volume = (1.0 - liquid_mass_fraction) / ice_density
    water_specific_volume = liquid_mass_fraction / water_density
    total_specific_volume = ice_specific_volume + water_specific_volume
    water_volume_fraction = water_specific_volume / total_specific_volume
    mixture_density = 1.0 / total_specific_volume

    ice_index = _complex_index(
        dielectric["ice_refractive_index"], "dielectric.ice_refractive_index"
    )
    water_index = _complex_index(
        dielectric["liquid_water_refractive_index"],
        "dielectric.liquid_water_refractive_index",
    )
    ice_permittivity = ice_index**2
    water_permittivity = water_index**2
    contrast = water_volume_fraction * (
        (water_permittivity - ice_permittivity)
        / (water_permittivity + 2.0 * ice_permittivity)
    )
    effective_permittivity = ice_permittivity * (1.0 + 2.0 * contrast) / (
        1.0 - contrast
    )
    effective_index = effective_permittivity**0.5
    if effective_index.real < 0.0:
        effective_index = -effective_index
    if effective_index.imag < 0.0:
        effective_index = effective_index.conjugate()
    return effective_index, mixture_density


def _terminal_speed(
    config: dict[str, Any], diameter_m: float, particle_density_kg_m3: float
) -> float:
    terminal = config["terminal_velocity"]
    if terminal["law"] == "atlas_rain_1973_exponential":
        diameter_mm = diameter_m * 1000.0
        speed = float(terminal["a_m_s"]) - float(terminal["b_m_s"]) * math.exp(
            -float(terminal["c_per_mm"]) * diameter_mm
        )
        if not math.isfinite(speed) or speed <= 0.0:
            raise GeneratorError(f"Atlas terminal speed is nonpositive at D={diameter_m} m")
        return speed

    gravity = float(terminal["gravity_m_s2"])
    air_density = float(terminal["air_density_kg_m3"])
    viscosity = float(terminal["air_dynamic_viscosity_pa_s"])
    transition = float(terminal["drag_transition_reynolds"])
    high_re_drag = float(terminal["high_reynolds_drag_coefficient"])
    tolerance = float(terminal["relative_tolerance"])
    iterations = int(terminal["maximum_iterations"])
    density_difference = particle_density_kg_m3 - air_density
    if density_difference <= 0.0:
        raise GeneratorError("particle density must exceed air density for gravitational fall")
    speed = 1.0
    for _ in range(iterations):
        reynolds = max(air_density * speed * diameter_m / viscosity, 1.0e-15)
        if reynolds < transition:
            drag = (24.0 / reynolds) * (1.0 + 0.15 * reynolds**0.687)
        else:
            drag = high_re_drag
        updated = math.sqrt(
            (4.0 * gravity * diameter_m * density_difference)
            / (3.0 * drag * air_density)
        )
        if abs(updated - speed) <= tolerance * max(updated, 1.0):
            return updated
        # Damping avoids a two-cycle around the configured drag transition.
        speed = 0.5 * (speed + updated)
    raise GeneratorError(
        f"terminal-speed iteration did not converge at D={diameter_m} m"
    )


def _pytmatrix_axis_ratio(shape_family: str, minor_to_major: float) -> float:
    """Map the crate's geometric ratio to PyTMatrix horizontal/rotational."""
    if not 0.0 < minor_to_major <= 1.0:
        raise GeneratorError("minor-to-major ratio must lie in (0, 1]")
    if shape_family == "oblate_spheroid":
        return 1.0 / minor_to_major
    if shape_family == "prolate_spheroid":
        return minor_to_major
    raise GeneratorError(f"unsupported shape family {shape_family!r}")


def _nonnegative(value: float, field: str, scale: float = 1.0) -> float:
    if not math.isfinite(value):
        raise GeneratorError(f"{field} is non-finite")
    tolerance = 256.0 * sys.float_info.epsilon * max(scale, 1.0)
    if value < -tolerance:
        raise GeneratorError(f"{field} is negative: {value!r}")
    return max(value, 0.0)


def compute_point(config: dict[str, Any], coordinates: dict[str, float]) -> list[float]:
    # Imports live only in isolated worker processes. A fatal Fortran STOP or
    # native crash therefore cannot leave the parent with a partial table.
    from pytmatrix import orientation as pytmatrix_orientation  # type: ignore[import-not-found]
    from pytmatrix import radar  # type: ignore[import-not-found]
    from pytmatrix.tmatrix import Scatterer  # type: ignore[import-not-found]

    validate_config(config)
    expected = {axis["kind"] for axis in config["axes"]}
    if set(coordinates) != expected:
        raise GeneratorError(
            f"worker coordinates {sorted(coordinates)} do not match axes {sorted(expected)}"
        )
    for key, value in coordinates.items():
        _number(value, f"coordinates.{key}")
    for axis in config["axes"]:
        coordinate = coordinates[axis["kind"]]
        lower = float(axis["coordinates"][0])
        upper = float(axis["coordinates"][-1])
        if coordinate < lower or coordinate > upper:
            raise GeneratorError(
                f"coordinate {axis['kind']}={coordinate} is outside [{lower}, {upper}]"
            )

    diameter_m = coordinates["equivolume_diameter"]
    frequency_hz = coordinates["frequency"]
    minor_to_major = coordinates["minor_to_major_axis_ratio"]
    refractive_index, particle_density = _material(config, coordinates)
    population = config["particle_population"]
    pytmatrix_axis_ratio = _pytmatrix_axis_ratio(
        population["shape_family"], minor_to_major
    )

    radar_config = config["radar"]
    solver = radar_config["solver"]
    wavelength_mm = (
        float(radar_config["speed_of_light_m_s"]) / frequency_hz * 1000.0
    )
    orientation_config = config["orientation"]
    fixed_orientation = orientation_config["model"] == "fixed_euler"
    scatterer = Scatterer(
        radius=diameter_m * 500.0,
        radius_type=Scatterer.RADIUS_EQUAL_VOLUME,
        wavelength=wavelength_mm,
        m=refractive_index,
        axis_ratio=pytmatrix_axis_ratio,
        shape=Scatterer.SHAPE_SPHEROID,
        ddelt=float(solver["ddelt"]),
        ndgs=int(solver["ndgs"]),
        alpha=float(orientation_config["pytmatrix_alpha_deg"])
        if fixed_orientation
        else 0.0,
        beta=float(orientation_config["pytmatrix_beta_deg"])
        if fixed_orientation
        else 0.0,
        Kw_sqr=float(radar_config["reference_water_dielectric_factor_squared"]),
        suppress_warning=True,
    )
    if not fixed_orientation:
        scatterer.orient = pytmatrix_orientation.orient_averaged_fixed
        scatterer.or_pdf = pytmatrix_orientation.gaussian_pdf(
            std=float(orientation_config["standard_deviation_deg"]),
            mean=float(orientation_config["mean_deg"]),
        )
        scatterer.n_alpha = int(orientation_config["alpha_quadrature_points"])
        scatterer.n_beta = int(orientation_config["beta_quadrature_points"])

    scatterer.set_geometry(tuple(float(v) for v in radar_config["backscatter_geometry_deg"]))
    zh_single = _nonnegative(float(radar.refl(scatterer, h_pol=True)), "ZH")
    zv_single = _nonnegative(float(radar.refl(scatterer, h_pol=False)), "ZV")
    rho_hv = float(radar.rho_hv(scatterer))
    delta_hv = float(radar.delta_hv(scatterer))
    if not math.isfinite(rho_hv) or not math.isfinite(delta_hv):
        raise GeneratorError("PyTMatrix covariance diagnostics are non-finite")
    if rho_hv < -1.0e-12 or rho_hv > 1.0 + 1.0e-12:
        raise GeneratorError(f"PyTMatrix rho_hv is outside [0,1]: {rho_hv}")
    rho_hv = min(max(rho_hv, 0.0), 1.0)
    covariance_magnitude = rho_hv * math.sqrt(zh_single * zv_single)
    covariance_single = complex(
        covariance_magnitude * math.cos(delta_hv),
        covariance_magnitude * math.sin(delta_hv),
    )

    scatterer.set_geometry(tuple(float(v) for v in radar_config["forward_scatter_geometry_deg"]))
    kdp_single = float(radar.Kdp(scatterer))
    ah_single = _nonnegative(float(radar.Ai(scatterer, h_pol=True)), "AH")
    av_single = _nonnegative(float(radar.Ai(scatterer, h_pol=False)), "AV")
    if not math.isfinite(kdp_single):
        raise GeneratorError("KDP is non-finite")

    number_density = float(population["normalization_number_concentration_m3"])
    zh = zh_single * number_density
    zv = zv_single * number_density
    covariance = covariance_single * number_density
    kdp = kdp_single * number_density
    ah = ah_single * number_density
    av = av_single * number_density
    fall_speed = _terminal_speed(config, diameter_m, particle_density)
    components = [
        zh,
        zv,
        covariance.real,
        covariance.imag,
        kdp,
        ah,
        av,
        zh * fall_speed,
        zh * fall_speed * fall_speed,
    ]
    validate_components(components)
    return components


def validate_components(components: Sequence[float]) -> None:
    if len(components) != POINT_COMPONENT_COUNT:
        raise GeneratorError(f"expected 9 components, got {len(components)}")
    if any(not math.isfinite(float(value)) for value in components):
        raise GeneratorError("point contains non-finite components")
    for index in (0, 1, 5, 6, 7, 8):
        if components[index] < 0.0:
            raise GeneratorError(f"component {index} must be nonnegative")
    covariance = math.hypot(components[2], components[3])
    bound = math.sqrt(components[0]) * math.sqrt(components[1])
    tolerance = 64.0 * sys.float_info.epsilon * max(bound, 1.0)
    if covariance > bound + tolerance:
        raise GeneratorError(f"covariance magnitude {covariance} exceeds bound {bound}")
    if components[0] == 0.0:
        if components[7] != 0.0 or components[8] != 0.0:
            raise GeneratorError("fall moments require nonzero ZH")
    else:
        mean = components[7] / components[0]
        raw_second = components[8] / components[0]
        variance = raw_second - mean * mean
        variance_tolerance = 64.0 * sys.float_info.epsilon * max(abs(raw_second), 1.0)
        if variance < -variance_tolerance:
            raise GeneratorError(f"fall-speed moments imply negative variance {variance}")


def run_isolated_point(
    config: dict[str, Any], coordinates: dict[str, float], timeout_seconds: int
) -> list[float]:
    request = json.dumps(
        {"config": config, "coordinates": coordinates},
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    environment = os.environ.copy()
    environment.update(
        {
            "PYTHONHASHSEED": "0",
            "PYTHONDONTWRITEBYTECODE": "1",
            "OPENBLAS_NUM_THREADS": "1",
            "OMP_NUM_THREADS": "1",
            "MKL_NUM_THREADS": "1",
            "NUMEXPR_NUM_THREADS": "1",
            "LC_ALL": "C.UTF-8",
            "TZ": "UTC",
        }
    )
    try:
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "_point_worker"],
            input=request,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise GeneratorError(
            f"point {coordinates} exceeded {timeout_seconds} s timeout"
        ) from error
    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    if completed.returncode != 0:
        raise GeneratorError(
            f"point {coordinates} worker exited {completed.returncode}; "
            f"stdout={stdout[-4000:]!r}; stderr={stderr[-4000:]!r}"
        )
    result_lines = [line for line in stdout.splitlines() if line.startswith(POINT_MARKER)]
    if len(result_lines) != 1:
        raise GeneratorError(
            f"point {coordinates} emitted {len(result_lines)} result markers; "
            f"stdout={stdout[-4000:]!r}"
        )
    try:
        components = json.loads(result_lines[0][len(POINT_MARKER) :])
    except json.JSONDecodeError as error:
        raise GeneratorError(f"point {coordinates} emitted invalid result JSON") from error
    if not isinstance(components, list):
        raise GeneratorError(f"point {coordinates} result is not an array")
    values = [float(value) for value in components]
    validate_components(values)
    return values


def _point_worker() -> int:
    try:
        request = json.loads(
            sys.stdin.buffer.read().decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite_constant,
        )
        if not isinstance(request, dict) or set(request) != {"config", "coordinates"}:
            raise GeneratorError("worker request has incorrect fields")
        components = compute_point(
            _require_object(request["config"], "worker.config"),
            {
                str(key): _number(value, f"worker.coordinates.{key}")
                for key, value in _require_object(
                    request["coordinates"], "worker.coordinates"
                ).items()
            },
        )
        print(
            POINT_MARKER
            + json.dumps(components, separators=(",", ":"), allow_nan=False),
            flush=True,
        )
        return 0
    except BaseException as error:  # Worker must turn every Python failure into a hard point failure.
        print(f"point worker failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


def package_versions() -> dict[str, str]:
    versions: dict[str, str] = {}
    for package in ("numpy", "pytmatrix", "scipy", "setuptools"):
        try:
            versions[package] = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError as error:
            raise GeneratorError(f"required package {package!r} is not installed") from error
    if versions["pytmatrix"] != "0.3.3":
        raise GeneratorError(f"expected pytmatrix 0.3.3, got {versions['pytmatrix']}")
    return dict(sorted(versions.items()))


def generator_metadata() -> dict[str, Any]:
    return {
        "name": "bowecho-pytmatrix-research-generator",
        "version": GENERATOR_VERSION,
        "executable": "crates/radar_scattering/tools/pytmatrix-0.3.3/generate_lut.py",
        "source_revision": f"sha256:{sha256_file(Path(__file__).resolve())}",
        "python_version": platform.python_version(),
        "package_versions": package_versions(),
    }


def science_metadata(config: dict[str, Any]) -> dict[str, Any]:
    dielectric = config["dielectric"]
    if dielectric["model"] == "maxwell_garnett_ice_host_water_inclusion":
        melting = {"model": "homogeneous_effective_medium", "rule": "maxwell_garnett"}
    elif dielectric["material"] == "ice":
        melting = {"model": "dry"}
    else:
        # The schema's melting field has no pure-liquid variant. SchemeResolved
        # is used only to say that the conventional category explicitly supplies
        # its all-liquid phase; the exact material remains in the config bytes.
        melting = {"model": "scheme_resolved"}
    orientation = config["orientation"]
    if orientation["model"] == "fixed_euler":
        orientation_metadata = {
            "model": "fixed_euler",
            "yaw_deg": float(orientation["yaw_deg"]),
            "pitch_deg": float(orientation["pitch_deg"]),
            "roll_deg": float(orientation["roll_deg"]),
        }
    else:
        orientation_metadata = {
            "model": "gaussian_canting",
            "mean_deg": float(orientation["mean_deg"]),
            "standard_deviation_deg": float(orientation["standard_deviation_deg"]),
            "quadrature_points": int(orientation["alpha_quadrature_points"])
            * int(orientation["beta_quadrature_points"]),
        }
    return {
        "kernel": {
            "model": "t_matrix",
            "implementation": {"implementation": "pytmatrix_0_3_3"},
        },
        "orientation": orientation_metadata,
        "melting": melting,
        "temporal": {"sampling": "instantaneous"},
        "validation": {"status": "research_only_unvalidated"},
    }


def _parse_lut(path: Path) -> tuple[bytes, dict[str, Any], bytes, bytes]:
    data = path.read_bytes()
    if len(data) < 14:
        raise GeneratorError(f"{path}: truncated LUT prefix")
    if data[:8] != MAGIC:
        raise GeneratorError(f"{path}: incorrect LUT magic")
    schema, header_length = struct.unpack("<HI", data[8:14])
    if schema != SCHEMA_VERSION:
        raise GeneratorError(f"{path}: schema is {schema}, expected 1")
    header_end = 14 + header_length
    if header_length == 0 or header_end > len(data):
        raise GeneratorError(f"{path}: invalid header length {header_length}")
    header_bytes = data[14:header_end]
    try:
        header = json.loads(
            header_bytes.decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, GeneratorError) as error:
        raise GeneratorError(f"{path}: invalid header JSON: {error}") from error
    if not isinstance(header, dict):
        raise GeneratorError(f"{path}: header is not an object")
    return data, header, header_bytes, data[header_end:]


def audit_emitted_lut(
    path: Path,
    *,
    config_bytes: bytes,
    config_text: str,
    config: dict[str, Any],
    generator: dict[str, Any],
    science: dict[str, Any],
    values: Sequence[Sequence[float]],
) -> dict[str, Any]:
    data, header, header_bytes, payload = _parse_lut(path)
    expected_axes = [
        {
            "kind": axis["kind"],
            "unit": axis["unit"],
            "coordinates": [float(value) for value in axis["coordinates"]],
        }
        for axis in config["axes"]
    ]
    expected_header = {
        "magic": MAGIC.decode("ascii"),
        "schema_version": SCHEMA_VERSION,
        "axes": expected_axes,
        "outputs": OUTPUTS,
        "generator": generator,
        "generator_config_utf8": config_text,
        "config_sha256": sha256_bytes(config_bytes),
        "science": science,
        "payload_encoding": "f64_le_point_major_last_axis_fastest",
        "grid_point_count": len(values),
        "payload_byte_length": len(values) * POINT_COMPONENT_COUNT * 8,
        "payload_sha256": sha256_bytes(payload),
    }
    if header != expected_header:
        expected_keys = set(expected_header)
        actual_keys = set(header)
        differences = []
        for key in sorted(expected_keys | actual_keys):
            if expected_header.get(key) != header.get(key):
                differences.append(key)
        raise GeneratorError(
            "Rust-emitted header disagrees with exact config/generator contract in fields: "
            + ", ".join(differences)
        )
    expected_payload = b"".join(
        struct.pack("<9d", *(float(component) for component in point))
        for point in values
    )
    if payload != expected_payload:
        actual_values = struct.unpack(f"<{len(payload) // 8}d", payload)
        expected_values = struct.unpack(f"<{len(expected_payload) // 8}d", expected_payload)
        first = next(
            (
                (index, expected, actual)
                for index, (expected, actual) in enumerate(
                    zip(expected_values, actual_values)
                )
                if struct.pack("<d", expected) != struct.pack("<d", actual)
            ),
            None,
        )
        raise GeneratorError(
            "Rust-emitted payload differs from collected point values; "
            f"first differing scalar={first}"
        )
    return {
        "lut_sha256": sha256_bytes(data),
        "lut_header_json_sha256": sha256_bytes(header_bytes),
        "generator_config_sha256": sha256_bytes(config_bytes),
        "payload_sha256": sha256_bytes(payload),
        "lut_byte_length": len(data),
        "lut_header_json_byte_length": len(header_bytes),
        "payload_byte_length": len(payload),
    }


def hash_tool_files(tool_root: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    for relative in TOOL_FILES:
        path = tool_root / relative
        if not path.is_file():
            raise GeneratorError(f"required tool file is missing: {path}")
        result[relative] = sha256_file(path)
    return result


def generate(args: argparse.Namespace) -> None:
    config_path = args.config.resolve()
    output_path = args.output.resolve()
    manifest_path = args.manifest.resolve()
    environment_path = args.environment_report.resolve()
    emitter = args.emitter
    for path in (output_path, manifest_path):
        if path.exists() and not args.overwrite:
            raise GeneratorError(f"refusing to overwrite existing artifact {path}")
    if not environment_path.is_file():
        raise GeneratorError(f"environment report does not exist: {environment_path}")

    config_bytes, config_text, config = parse_exact_json(config_path)
    validate_config(config)
    generator = generator_metadata()
    if generator["python_version"] != "3.11.9":
        raise GeneratorError(
            f"locked generator requires CPython 3.11.9, got {generator['python_version']}"
        )
    science = science_metadata(config)
    coordinates = list(point_coordinates(config))
    timeout_seconds = int(config["execution"]["point_timeout_seconds"])
    values: list[list[float]] = []
    for flat_index, point in enumerate(coordinates):
        try:
            values.append(run_isolated_point(config, point, timeout_seconds))
        except GeneratorError as error:
            raise GeneratorError(f"flat grid point {flat_index} failed: {error}") from error

    request = {
        "axes": [
            {
                "kind": axis["kind"],
                "unit": axis["unit"],
                "coordinates": [float(value) for value in axis["coordinates"]],
            }
            for axis in config["axes"]
        ],
        "generator": generator,
        "generator_config_utf8": config_text,
        "science": science,
        "value_f64_bits_hex": [
            [f64_bits_hex(component) for component in point] for point in values
        ],
    }
    output_path.parent.mkdir(parents=True, exist_ok=True)
    request_path = output_path.with_name(f".{output_path.name}.emit-request.tmp.json")
    emitted_path = output_path.with_name(f".{output_path.name}.emitting.tmp")
    request_bytes = (
        json.dumps(request, sort_keys=True, separators=(",", ":"), allow_nan=False)
    ).encode("utf-8")
    try:
        request_path.write_bytes(request_bytes)
        completed = subprocess.run(
            [emitter, str(request_path), str(emitted_path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            raise GeneratorError(
                f"Rust emitter exited {completed.returncode}; "
                f"stdout={completed.stdout.decode('utf-8', errors='replace')[-4000:]!r}; "
                f"stderr={completed.stderr.decode('utf-8', errors='replace')[-4000:]!r}"
            )
        if not emitted_path.is_file():
            raise GeneratorError("Rust emitter reported success without an output file")
        digests = audit_emitted_lut(
            emitted_path,
            config_bytes=config_bytes,
            config_text=config_text,
            config=config,
            generator=generator,
            science=science,
            values=values,
        )
        os.replace(emitted_path, output_path)
    finally:
        request_path.unlink(missing_ok=True)
        emitted_path.unlink(missing_ok=True)

    tool_root = Path(__file__).resolve().parent
    environment_bytes = environment_path.read_bytes()
    manifest = {
        "schema": 1,
        "artifact_classification": "unvalidated_research",
        "crate_validation_status": "research_only_unvalidated",
        "production_activation": False,
        "table_id": config["table_id"],
        "lut_file": output_path.name,
        **digests,
        "environment_report_file": environment_path.name,
        "environment_report_sha256": sha256_bytes(environment_bytes),
        "generator": generator,
        "science": science,
        "axes": request["axes"],
        "grid_point_count": len(values),
        "isolated_solver_process_count": len(values),
        "solver_failures": [],
        "tool_file_sha256": hash_tool_files(tool_root),
        "scope": {
            "microphysics_family": "conventional",
            "p3_coverage": False,
            "ishmael_coverage": False,
            "psd": "monodisperse_node_normalized_to_exactly_1_per_m3",
        },
        "validation_claim": (
            "No independent scientific validation. Separate direct-PyTMatrix held-out "
            "nodes test generator/LUT interpolation only."
        ),
    }
    write_json(manifest_path, manifest)


def _capture(command: list[str]) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": command, "error": f"{type(error).__name__}: {error}"}
    return {
        "command": command,
        "returncode": completed.returncode,
        "stdout": completed.stdout.decode("utf-8", errors="replace"),
        "stderr": completed.stderr.decode("utf-8", errors="replace"),
    }


def _capture_ldd(path: Path) -> dict[str, Any]:
    result = _capture(["ldd", str(path)])
    # ldd includes ASLR load addresses even though linkage is unchanged. Keep
    # exact sonames/resolved paths while removing only those ephemeral values.
    for field in ("stdout", "stderr"):
        if field in result:
            result[field] = re.sub(r"\s+\(0x[0-9a-fA-F]+\)", "", result[field])
    return result


def capture_environment(args: argparse.Namespace) -> None:
    tool_root = Path(__file__).resolve().parent
    try:
        import numpy  # type: ignore[import-not-found]
        from pytmatrix.fortran_tm import pytmatrix as fortran_extension  # type: ignore[import-not-found]
    except ImportError as error:
        raise GeneratorError(f"cannot inspect numerical environment: {error}") from error

    extension_path = Path(fortran_extension.__file__).resolve()
    numpy_core_path = Path(numpy.core._multiarray_umath.__file__).resolve()
    emitter_path_text = shutil.which(args.emitter)
    if emitter_path_text is None:
        raise GeneratorError(f"cannot locate emitter {args.emitter!r}")
    emitter_path = Path(emitter_path_text).resolve()
    os_release_path = Path("/etc/os-release")
    report = {
        "schema": 1,
        "artifact_classification": "reproducibility_environment_not_scientific_validation",
        "target": platform.machine().lower(),
        "platform": platform.platform(),
        "python_version": platform.python_version(),
        "python_vv": _capture([sys.executable, "-VV"]),
        "python_executable": str(Path(sys.executable).resolve()),
        "python_executable_sha256": sha256_file(Path(sys.executable).resolve()),
        "package_versions": package_versions(),
        "pip_freeze_all": _capture([sys.executable, "-m", "pip", "freeze", "--all"]),
        "numpy_show_config": _capture([sys.executable, "-c", "import numpy; numpy.show_config()"]),
        "numpy_core_extension": str(numpy_core_path),
        "numpy_core_extension_sha256": sha256_file(numpy_core_path),
        "numpy_core_ldd": _capture_ldd(numpy_core_path),
        "pytmatrix_fortran_extension": str(extension_path),
        "pytmatrix_fortran_extension_sha256": sha256_file(extension_path),
        "pytmatrix_fortran_extension_ldd": _capture_ldd(extension_path),
        "gfortran_version": _capture(["gfortran", "--version"]),
        "native_packages": _capture(
            [
                "dpkg-query",
                "-W",
                "-f=${Package}=${Version}\\n",
                "build-essential",
                "gcc",
                "gfortran",
                "gfortran-12",
                "libgfortran5",
                "libstdc++6",
            ]
        ),
        "rustc_version": _capture(["rustc", "-Vv"]),
        "emitter_executable": str(emitter_path),
        "emitter_executable_sha256": sha256_file(emitter_path),
        "tool_file_sha256": hash_tool_files(tool_root),
        "os_release": os_release_path.read_text(encoding="utf-8")
        if os_release_path.is_file()
        else None,
        "container_image_id": os.environ.get("BRSLUT_CONTAINER_IMAGE_ID", "not_supplied"),
        "base_image_linux_amd64_digest": (
            "python:3.11.9-slim-bookworm@sha256:"
            "2856e6af199e8128161abd320575eb9b341f3b76f017b5d0c9cd364f60d8a050"
        ),
        "rust_toolchain_image_linux_amd64_digest": (
            "rust:1.85.1-slim-bookworm@sha256:"
            "3490aa77d179a59d67e94239cca96dd84030b564470859200f535b942bdffedf"
        ),
        "debian_snapshot": "20240904T000000Z",
        "thread_environment": {
            key: os.environ.get(key)
            for key in (
                "OPENBLAS_NUM_THREADS",
                "OMP_NUM_THREADS",
                "MKL_NUM_THREADS",
                "NUMEXPR_NUM_THREADS",
                "PYTHONHASHSEED",
                "PYTHONDONTWRITEBYTECODE",
                "LC_ALL",
                "TZ",
                "SOURCE_DATE_EPOCH",
            )
        },
    }
    write_json(args.output.resolve(), report)


def inspect(args: argparse.Namespace) -> None:
    data, header, header_bytes, payload = _parse_lut(args.lut.resolve())
    summary = {
        "lut_sha256": sha256_bytes(data),
        "lut_header_json_sha256": sha256_bytes(header_bytes),
        "generator_config_sha256": header.get("config_sha256"),
        "payload_sha256_declared": header.get("payload_sha256"),
        "payload_sha256_computed": sha256_bytes(payload),
        "grid_point_count": header.get("grid_point_count"),
        "axes": header.get("axes"),
        "science": header.get("science"),
    }
    print(json.dumps(summary, indent=2, sort_keys=True, allow_nan=False))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    generate_parser = subparsers.add_parser("generate", help="generate one complete LUT")
    generate_parser.add_argument("--config", type=Path, required=True)
    generate_parser.add_argument("--output", type=Path, required=True)
    generate_parser.add_argument("--manifest", type=Path, required=True)
    generate_parser.add_argument("--environment-report", type=Path, required=True)
    generate_parser.add_argument("--emitter", default="brslut-emitter")
    generate_parser.add_argument("--overwrite", action="store_true")
    generate_parser.set_defaults(function=generate)

    environment_parser = subparsers.add_parser(
        "environment", help="capture deterministic native/package environment evidence"
    )
    environment_parser.add_argument("--output", type=Path, required=True)
    environment_parser.add_argument("--emitter", default="brslut-emitter")
    environment_parser.set_defaults(function=capture_environment)

    inspect_parser = subparsers.add_parser("inspect", help="print LUT digest/header summary")
    inspect_parser.add_argument("--lut", type=Path, required=True)
    inspect_parser.set_defaults(function=inspect)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments == ["_point_worker"]:
        return _point_worker()
    parser = build_parser()
    args = parser.parse_args(arguments)
    try:
        args.function(args)
        return 0
    except GeneratorError as error:
        print(f"generate_lut.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
