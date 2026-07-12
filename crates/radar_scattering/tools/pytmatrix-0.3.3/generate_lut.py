#!/usr/bin/env python3
"""Deterministic, process-isolated PyTMatrix 0.3.3 schema-v1 LUT generator.

The electromagnetic values are produced by official PyTMatrix 0.3.3.  A
small Rust emitter constructs the final file through radar_scattering's public
OfflineLut API, so this script does not duplicate the binary format writer.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
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


GENERATOR_VERSION = "1.1.0"
MAGIC = b"BRSLUT01"
SCHEMA_VERSION = 1
POINT_COMPONENT_COUNT = 9
POINT_MARKER = "BRSLUT_POINT_RESULT="
GROUP_MARKER = "BRSLUT_GROUP_RESULT="
PROPERTY_SOLVER_NDGS = 14
GENERATION_WORKERS = 12
CONVENTIONAL_SOLVER_NDGS = 2
CONVERGENCE_SOLVER_NDGS = (8, 10, 12, 14, 16, 18, 20)
EXACT_PROPERTY_BAND_FREQUENCIES_HZ = {
    "s": 2.8e9,
    "c": 5.6e9,
    "x": 9.4e9,
}

AXIS_UNITS = {
    "equivolume_diameter": "meter",
    "temperature": "kelvin",
    "bulk_density": "kilogram_per_cubic_meter",
    "condensed_volume_fraction": "unitless_fraction",
    "liquid_mass_fraction": "unitless_fraction",
    "minor_to_major_axis_ratio": "unitless_fraction",
    "frequency": "hertz",
    "radar_elevation": "degree",
}

PROPERTY_FAMILY = "property_aware_p3_ishmael"
PROPERTY_CATEGORY = "frozen_characteristic_particle"
DRY_PROPERTY_DIELECTRIC_MODEL = (
    "symmetric_bruggeman_spherical_air_ice_matzler_2006_v1"
)
WET_PROPERTY_DIELECTRIC_MODEL = "symmetric_bruggeman_spherical_air_ice_water_v1"
TEMPERATURE_WATER_DIELECTRIC_MODEL = "temperature_dependent_liquid_water_liebe_1991"
WET_PROPERTY_AXIS_ORDER = (
    "equivolume_diameter",
    "temperature",
    "condensed_volume_fraction",
    "liquid_mass_fraction",
    "minor_to_major_axis_ratio",
    "frequency",
    "radar_elevation",
)
DRY_PROPERTY_AXIS_ORDER = (
    "equivolume_diameter",
    "temperature",
    "bulk_density",
    "minor_to_major_axis_ratio",
    "frequency",
    "radar_elevation",
)
CONVENTIONAL_AXIS_SUFFIX = (
    "minor_to_major_axis_ratio",
    "frequency",
    "radar_elevation",
)

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
    "PACK_FORMAT.md",
    "README.md",
    "generate_band_pack.py",
    "generate_lut.py",
    "generator_config.example.json",
    "requirements-bootstrap-pinned.txt",
    "requirements-pytmatrix-pinned.txt",
    "run_all.ps1",
    "toolchain.json",
    "test_generate_band_pack.py",
    "emitter/Cargo.toml",
    "emitter/Cargo.lock",
    "emitter/src/main.rs",
    "emitter/src/bin/validate_tmatrix_lut.rs",
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


def _is_property_aware(config: dict[str, Any]) -> bool:
    population = config.get("particle_population")
    return (
        isinstance(population, dict)
        and population.get("microphysics_family") == PROPERTY_FAMILY
    )


def _is_residual_rain(config: dict[str, Any]) -> bool:
    population = config.get("particle_population")
    return (
        isinstance(population, dict)
        and population.get("microphysics_family") == "conventional"
        and population.get("category") == "rain"
        and isinstance(population.get("coexistence_descriptor"), dict)
    )


def _uses_material_state_grouping(config: dict[str, Any]) -> bool:
    return _is_property_aware(config) or _is_residual_rain(config)


def _property_phase_regime(config: dict[str, Any]) -> str:
    if not _is_property_aware(config):
        raise GeneratorError("config is not property-aware")
    phase = config["particle_population"].get("phase_regime")
    if phase not in ("dry", "wet"):
        raise GeneratorError("property-aware phase regime is invalid")
    return str(phase)


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
    conventional_population_keys = (
        "microphysics_family",
        "category",
        "shape_family",
        "size_distribution",
        "normalization_number_concentration_m3",
    )
    property_population_keys = conventional_population_keys + (
        "phase_regime",
        "state_descriptor",
    )
    family = population.get("microphysics_family")
    if family == "conventional":
        conventional_allowed = conventional_population_keys + (
            ("coexistence_descriptor",)
            if "coexistence_descriptor" in population
            else ()
        )
        _keys(
            population,
            required=conventional_allowed,
            allowed=conventional_allowed,
            path="particle_population",
        )
        if population["category"] not in ("rain", "hail"):
            raise GeneratorError("conventional particle_population.category must be rain or hail")
        if population["size_distribution"] != "monodisperse_node":
            raise GeneratorError("conventional size_distribution must be monodisperse_node")
        if "coexistence_descriptor" in population:
            if population["category"] != "rain":
                raise GeneratorError("coexistence_descriptor is restricted to rain")
            descriptor = _require_object(
                population["coexistence_descriptor"],
                "particle_population.coexistence_descriptor",
            )
            descriptor_contract = {
                "role": "standalone_rain_and_residual_after_mixed_phase_pairing",
                "allocation_rule": (
                    "max_total_rain_mass_minus_liquid_mass_paired_into_"
                    "wet_frozen_categories_zero"
                ),
                "double_count_policy": (
                    "paired_liquid_mass_removed_exactly_once_before_rain_lookup"
                ),
                "over_pairing_policy": "reject",
            }
            _keys(
                descriptor,
                required=descriptor_contract,
                allowed=descriptor_contract,
                path="particle_population.coexistence_descriptor",
            )
            for key, expected in descriptor_contract.items():
                if descriptor[key] != expected:
                    raise GeneratorError(
                        f"particle_population.coexistence_descriptor.{key} "
                        f"must equal {expected!r}"
                    )
    elif family == PROPERTY_FAMILY:
        _keys(
            population,
            required=property_population_keys,
            allowed=property_population_keys,
            path="particle_population",
        )
        if population["category"] != PROPERTY_CATEGORY:
            raise GeneratorError(
                f"property-aware particle_population.category must be {PROPERTY_CATEGORY}"
            )
        if population["size_distribution"] != "monodisperse_characteristic_particle_node":
            raise GeneratorError(
                "property-aware size_distribution must be "
                "monodisperse_characteristic_particle_node"
            )
        phase_regime = population["phase_regime"]
        if phase_regime not in ("dry", "wet"):
            raise GeneratorError("property-aware phase_regime must be dry or wet")
        descriptor = _require_object(
            population["state_descriptor"], "particle_population.state_descriptor"
        )
        descriptor_contract = {
            "compatible_closed_state_families": ["p3", "ishmael"],
            "characteristic_diameter_mapping": (
                "closure_derived_equivolume_characteristic_diameter"
            ),
            "bulk_density_mapping": (
                "closure_derived_effective_bulk_density_including_rime_mass_and_rime_density"
                if phase_regime == "dry"
                else "closure_bulk_density_and_liquid_mass_fraction_mapped_to_"
                "condensed_volume_fraction"
            ),
            "shape_mapping": "closure_derived_minor_to_major_axis_ratio",
            "liquid_mapping": (
                "required_exactly_zero_liquid_mass_fraction"
                if phase_regime == "dry"
                else "diagnosed_or_prescribed_strictly_positive_liquid_mass_fraction"
            ),
            "phase_dispatch": (
                "liquid_mass_fraction_equal_zero_selects_dry_table"
                if phase_regime == "dry"
                else "liquid_mass_fraction_greater_than_zero_selects_wet_table"
            ),
            "rime_axes": (
                "not_explicit_rime_influences_only_through_bulk_density_and_shape"
            ),
            "rime_effect_on_dielectric": "none_given_bulk_density",
            "psd_mapping": (
                "none_monodisperse_characteristic_particle_not_scheme_native_psd"
            ),
            "extrapolation": "forbidden",
            "density_applicability": (
                "bulk_density_1p5_to_917_kg_m3_downward_fall_requires_density_"
                "above_1p225_kg_m3_air"
                if phase_regime == "dry"
                else "condensed_volume_fraction_0p0015_to_1_downward_fall_"
                "requires_reconstructed_density_above_1p225_kg_m3_air"
            ),
        }
        if phase_regime == "wet":
            descriptor_contract["condensed_volume_fraction_definition"] = (
                "rho_bulk_times_open_parenthesis_one_minus_w_over_917_plus_w_"
                "over_999p84_close_parenthesis"
            )
        _keys(
            descriptor,
            required=descriptor_contract,
            allowed=descriptor_contract,
            path="particle_population.state_descriptor",
        )
        for key, expected in descriptor_contract.items():
            if descriptor[key] != expected:
                raise GeneratorError(
                    f"particle_population.state_descriptor.{key} must equal {expected!r}"
                )
    else:
        raise GeneratorError(
            "particle_population.microphysics_family must be conventional or "
            f"{PROPERTY_FAMILY}"
        )
    if population["shape_family"] not in ("oblate_spheroid", "prolate_spheroid"):
        raise GeneratorError("shape_family must be oblate_spheroid or prolate_spheroid")
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
    elif model == TEMPERATURE_WATER_DIELECTRIC_MODEL:
        required = (
            "model",
            "liquid_water_permittivity_model",
            "mass_density_kg_m3",
            "temperature_range_k",
            "frequency_range_hz",
            "applicability",
        )
        _keys(dielectric, required=required, allowed=required, path="dielectric")
        if not _is_residual_rain(config):
            raise GeneratorError(
                f"dielectric.model {TEMPERATURE_WATER_DIELECTRIC_MODEL} requires "
                "the residual-rain coexistence descriptor"
            )
        exact_water_contract = {
            "liquid_water_permittivity_model": (
                "liebe_hufford_manabe_1991_double_debye"
            ),
            "temperature_range_k": [250.0, 313.15],
            "applicability": (
                "pure_fresh_supercooled_or_liquid_water_250_to_313p15_k"
            ),
        }
        for key, expected in exact_water_contract.items():
            if dielectric[key] != expected:
                raise GeneratorError(f"dielectric.{key} must equal {expected!r}")
        if dielectric["frequency_range_hz"] not in (
            [2.0e9, 4.0e9],
            [2.0e9, 10.0e9],
        ):
            raise GeneratorError(
                "residual-rain dielectric.frequency_range_hz must be either "
                "the legacy S-only [2, 4] GHz range or the exact-band-pack "
                "[2, 10] GHz range"
            )
        if (
            _number(
                dielectric["mass_density_kg_m3"],
                "dielectric.mass_density_kg_m3",
                positive=True,
            )
            != 999.84
        ):
            raise GeneratorError("residual-rain density must be exactly 999.84 kg m^-3")
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
    elif model == WET_PROPERTY_DIELECTRIC_MODEL:
        required = (
            "model",
            "air_relative_permittivity",
            "ice_permittivity_model",
            "liquid_water_permittivity_model",
            "ice_material_density_kg_m3",
            "liquid_water_density_kg_m3",
            "condensed_volume_fraction_interpretation",
            "liquid_mass_fraction_interpretation",
            "component_volume_fraction_conversion",
            "bulk_density_reconstruction",
            "mixing_equation",
            "root_selection",
            "homotopy_steps",
            "newton_max_iterations",
            "newton_relative_tolerance",
            "temperature_range_k",
            "ice_temperature_treatment",
            "applicability",
        )
        _keys(dielectric, required=required, allowed=required, path="dielectric")
        if not _is_property_aware(config):
            raise GeneratorError(
                f"dielectric.model {WET_PROPERTY_DIELECTRIC_MODEL} requires {PROPERTY_FAMILY}"
            )
        if _property_phase_regime(config) != "wet":
            raise GeneratorError("three-component dielectric requires wet phase_regime")
        air_permittivity = _complex_index(
            dielectric["air_relative_permittivity"],
            "dielectric.air_relative_permittivity",
        )
        if air_permittivity != complex(1.0, 0.0):
            raise GeneratorError("air_relative_permittivity must be exactly 1+0i")
        exact_dielectric = {
            "ice_permittivity_model": "matzler_2006",
            "liquid_water_permittivity_model": (
                "liebe_hufford_manabe_1991_double_debye"
            ),
            "condensed_volume_fraction_interpretation": (
                "ice_plus_liquid_component_volume_over_outer_spheroid_volume"
            ),
            "liquid_mass_fraction_interpretation": (
                "liquid_mass_over_total_condensed_mass"
            ),
            "component_volume_fraction_conversion": (
                "condensed_volume_fraction_times_mass_specific_volume_shares"
            ),
            "bulk_density_reconstruction": (
                "condensed_volume_fraction_divided_by_total_component_specific_volume"
            ),
            "mixing_equation": (
                "sum_f_j_times_eps_j_minus_eps_eff_over_eps_j_plus_2eps_eff_equals_zero"
            ),
            "root_selection": (
                "vacuum_to_constituents_homotopy_passive_continuous_branch"
            ),
            "temperature_range_k": [269.15, 275.15],
            "ice_temperature_treatment": (
                "minimum_environment_temperature_and_273p15_k_phase_equilibrium"
            ),
            "applicability": (
                "quasistatic_spherical_inclusions_homogeneous_effective_medium"
            ),
        }
        for key, expected in exact_dielectric.items():
            if dielectric[key] != expected:
                raise GeneratorError(f"dielectric.{key} must equal {expected!r}")
        ice_density = _number(
            dielectric["ice_material_density_kg_m3"],
            "dielectric.ice_material_density_kg_m3",
            positive=True,
        )
        water_density = _number(
            dielectric["liquid_water_density_kg_m3"],
            "dielectric.liquid_water_density_kg_m3",
            positive=True,
        )
        if ice_density != 917.0 or water_density != 999.84:
            raise GeneratorError(
                "property-aware component densities must be exactly 917.0 and 999.84 kg m^-3"
            )
        if _positive_integer(dielectric["homotopy_steps"], "dielectric.homotopy_steps") < 16:
            raise GeneratorError("dielectric.homotopy_steps must be at least 16")
        if (
            _positive_integer(
                dielectric["newton_max_iterations"],
                "dielectric.newton_max_iterations",
            )
            < 16
        ):
            raise GeneratorError("dielectric.newton_max_iterations must be at least 16")
        tolerance = _number(
            dielectric["newton_relative_tolerance"],
            "dielectric.newton_relative_tolerance",
            positive=True,
        )
        if tolerance > 1.0e-10:
            raise GeneratorError("dielectric Newton tolerance must be <= 1e-10")
    elif model == DRY_PROPERTY_DIELECTRIC_MODEL:
        required = (
            "model",
            "air_relative_permittivity",
            "ice_permittivity_model",
            "ice_material_density_kg_m3",
            "bulk_density_interpretation",
            "component_volume_fraction_conversion",
            "mixing_equation",
            "root_selection",
            "homotopy_steps",
            "newton_max_iterations",
            "newton_relative_tolerance",
            "temperature_range_k",
            "temperature_evidence",
            "applicability",
        )
        _keys(dielectric, required=required, allowed=required, path="dielectric")
        if not _is_property_aware(config) or _property_phase_regime(config) != "dry":
            raise GeneratorError("dry air/ice dielectric requires dry property-aware phase")
        air_permittivity = _complex_index(
            dielectric["air_relative_permittivity"],
            "dielectric.air_relative_permittivity",
        )
        if air_permittivity != complex(1.0, 0.0):
            raise GeneratorError("air_relative_permittivity must be exactly 1+0i")
        dry_contract = {
            "ice_permittivity_model": "matzler_2006",
            "bulk_density_interpretation": (
                "total_ice_mass_per_outer_spheroid_volume"
            ),
            "component_volume_fraction_conversion": (
                "bulk_density_divided_by_ice_material_density"
            ),
            "mixing_equation": (
                "sum_f_j_times_eps_j_minus_eps_eff_over_eps_j_plus_2eps_eff_equals_zero"
            ),
            "root_selection": (
                "vacuum_to_constituents_homotopy_passive_continuous_branch"
            ),
            "temperature_range_k": [190.0, 273.15],
            "temperature_evidence": (
                "matzler_2006_formula_warren_brandt_2008_reports_accurate_fit_"
                "190_to_258_k_warm_extension_declared_to_273p15_k"
            ),
            "applicability": (
                "quasistatic_spherical_air_in_ice_or_ice_in_air_"
                "topology_neutral_homogeneous_effective_medium"
            ),
        }
        for key, expected in dry_contract.items():
            if dielectric[key] != expected:
                raise GeneratorError(f"dielectric.{key} must equal {expected!r}")
        if (
            _number(
                dielectric["ice_material_density_kg_m3"],
                "dielectric.ice_material_density_kg_m3",
                positive=True,
            )
            != 917.0
        ):
            raise GeneratorError("dry property ice density must be exactly 917 kg m^-3")
        if _positive_integer(dielectric["homotopy_steps"], "dielectric.homotopy_steps") < 16:
            raise GeneratorError("dielectric.homotopy_steps must be at least 16")
        if (
            _positive_integer(
                dielectric["newton_max_iterations"],
                "dielectric.newton_max_iterations",
            )
            < 16
        ):
            raise GeneratorError("dielectric.newton_max_iterations must be at least 16")
        tolerance = _number(
            dielectric["newton_relative_tolerance"],
            "dielectric.newton_relative_tolerance",
            positive=True,
        )
        if tolerance > 1.0e-10:
            raise GeneratorError("dielectric Newton tolerance must be <= 1e-10")
    else:
        raise GeneratorError(f"unsupported dielectric.model {model!r}")

    axes = config["axes"]
    if not isinstance(axes, list):
        raise GeneratorError("axes must be an array")
    if model == WET_PROPERTY_DIELECTRIC_MODEL:
        expected_kinds = list(WET_PROPERTY_AXIS_ORDER)
    elif model == DRY_PROPERTY_DIELECTRIC_MODEL:
        expected_kinds = list(DRY_PROPERTY_AXIS_ORDER)
    elif model == TEMPERATURE_WATER_DIELECTRIC_MODEL:
        expected_kinds = [
            "equivolume_diameter",
            "temperature",
            "minor_to_major_axis_ratio",
            "frequency",
            "radar_elevation",
        ]
    else:
        expected_kinds = ["equivolume_diameter"]
        if model == "maxwell_garnett_ice_host_water_inclusion":
            expected_kinds.append("liquid_mass_fraction")
        expected_kinds.extend(CONVENTIONAL_AXIS_SUFFIX)
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
        if expected_kind == "temperature" and any(not (190.0 <= v <= 313.15) for v in numeric):
            raise GeneratorError("configured temperatures must lie in [190, 313.15] K")
        if expected_kind == "bulk_density" and any(not (1.5 <= v <= 917.0) for v in numeric):
            raise GeneratorError("bulk densities must lie in [1.5, 917.0] kg m^-3")
        if expected_kind == "condensed_volume_fraction" and any(
            not (0.0 < v <= 1.0) for v in numeric
        ):
            raise GeneratorError("condensed volume fractions must lie in (0, 1]")
        if expected_kind == "liquid_mass_fraction" and any(not (0.0 <= v <= 1.0) for v in numeric):
            raise GeneratorError("liquid mass fractions must lie in [0, 1]")
        if expected_kind == "minor_to_major_axis_ratio" and any(not (0.0 < v <= 1.0) for v in numeric):
            raise GeneratorError("minor-to-major ratios must lie in (0, 1]")
        if expected_kind == "frequency":
            if _is_property_aware(config) or _is_residual_rain(config):
                if (
                    len(numeric) != 1
                    or numeric[0] not in EXACT_PROPERTY_BAND_FREQUENCIES_HZ.values()
                ):
                    raise GeneratorError(
                        "view-aware frequency must be a singleton exactly at "
                        "2.8, 5.6, or 9.4 GHz; frequency interpolation is forbidden"
                    )
            elif any(not (2.0e9 <= v <= 4.0e9) for v in numeric):
                raise GeneratorError(
                    "legacy conventional frequency nodes must remain in S band [2, 4] GHz"
                )
        if expected_kind == "radar_elevation" and any(not (-90.0 <= v <= 90.0) for v in numeric):
            raise GeneratorError("radar elevations must lie in [-90, 90] degrees")

    if _is_property_aware(config) or _is_residual_rain(config):
        exact_frequency_hz = axis_coordinates(config, "frequency")[0]
        band = next(
            name
            for name, frequency_hz in EXACT_PROPERTY_BAND_FREQUENCIES_HZ.items()
            if frequency_hz == exact_frequency_hz
        )
        if f"-{band}band-" not in config["table_id"]:
            raise GeneratorError(
                f"table_id must contain '-{band}band-' for exact frequency "
                f"{exact_frequency_hz} Hz"
            )
        if _is_residual_rain(config):
            frequency_range_hz = dielectric["frequency_range_hz"]
            if not (frequency_range_hz[0] <= exact_frequency_hz <= frequency_range_hz[1]):
                raise GeneratorError(
                    "residual-rain dielectric frequency range does not contain the "
                    "exact configured pack frequency"
                )
            if band in ("c", "x") and frequency_range_hz != [2.0e9, 10.0e9]:
                raise GeneratorError(
                    "C/X residual-rain configs must declare the [2, 10] GHz "
                    "Liebe-model applicability range"
                )
        temperatures = axis_coordinates(config, "temperature")
        elevations = axis_coordinates(config, "radar_elevation")
        if elevations[0] != -0.5 or elevations[-1] != 20.0:
            raise GeneratorError(
                "property-aware radar elevation axis must cover exactly -0.5 through 20 degrees"
            )
        if _is_property_aware(config):
            ice_density = float(dielectric["ice_material_density_kg_m3"])
            if _property_phase_regime(config) == "dry":
                bulk_densities = axis_coordinates(config, "bulk_density")
                if temperatures[0] != 190.0 or temperatures[-1] != 273.15:
                    raise GeneratorError(
                        "dry property temperature axis must cover exactly 190 through 273.15 K"
                    )
                if any(bulk_density > ice_density for bulk_density in bulk_densities):
                    raise GeneratorError("dry bulk density exceeds solid-ice density")
                if bulk_densities[0] != 1.5 or bulk_densities[-1] != 917.0:
                    raise GeneratorError(
                        "dry bulk-density axis must cover exactly 1.5 through 917 kg m^-3"
                    )
            else:
                if temperatures[0] != 269.15 or temperatures[-1] != 275.15:
                    raise GeneratorError(
                        "wet property temperature axis must cover exactly 269.15 through 275.15 K"
                    )
                liquid_fractions = axis_coordinates(config, "liquid_mass_fraction")
                if liquid_fractions[0] != 0.0 or liquid_fractions[-1] <= 0.0:
                    raise GeneratorError(
                        "wet liquid-fraction axis must include zero boundary support and positive states"
                    )
                condensed = axis_coordinates(config, "condensed_volume_fraction")
                if condensed[0] != 0.0015 or condensed[-1] != 1.0:
                    raise GeneratorError(
                        "wet condensed-volume axis must cover exactly 0.0015 through 1"
                    )
        elif temperatures[0] != 250.0 or temperatures[-1] != 313.15:
            raise GeneratorError(
                "view-aware liquid-rain temperature axis must cover exactly 250 through 313.15 K"
            )
    elif axis_coordinates(config, "radar_elevation") != [0.0]:
        raise GeneratorError(
            "legacy conventional configs are restricted to singleton 0-degree elevation"
        )

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

    if _uses_material_state_grouping(config):
        if orientation["model"] != "gaussian_canting":
            raise GeneratorError("view-aware tables require Gaussian canting")
        exact_orientation = {
            "mean_deg": 0.0,
            "standard_deviation_deg": 20.0,
            "alpha_quadrature_points": 5,
            "beta_quadrature_points": 10,
            "quadrature_method": "pytmatrix_orient_averaged_fixed_gautschi",
            "reference_symmetry_axis": "vertical_at_zero_canting",
        }
        for key, expected in exact_orientation.items():
            if orientation[key] != expected:
                raise GeneratorError(
                    f"view-aware orientation.{key} must equal {expected!r}"
                )

    radar = _require_object(config["radar"], "radar")
    base_radar_keys = (
        "speed_of_light_m_s",
        "reference_water_dielectric_factor_squared",
        "length_unit_passed_to_pytmatrix",
        "backscatter_geometry_deg",
        "forward_scatter_geometry_deg",
        "covariance_phase_convention",
        "solver",
    )
    property_radar_keys = base_radar_keys + (
        "beam_elevation_transform",
        "polarization_basis",
        "view_applicability",
    )
    radar_keys = property_radar_keys
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
    geometry_contract = {
        "beam_elevation_transform": (
            "pytmatrix_theta0_90_minus_e_theta_back_90_plus_e_"
            "theta_forward_90_minus_e_degrees"
        ),
        "polarization_basis": "pytmatrix_local_horizontal_vertical_scattering_basis",
        "view_applicability": (
            "ppi_beam_elevation_minus0p5_to_20_axisymmetric_gaussian_odf_"
            "not_general_body_frame"
            if _uses_material_state_grouping(config)
            else "horizontal_singleton_zero_degree_axis"
        ),
    }
    for key, expected in geometry_contract.items():
        if radar[key] != expected:
            raise GeneratorError(f"radar.{key} must equal {expected!r}")
    solver = _require_object(radar["solver"], "radar.solver")
    _keys(solver, required=("shape", "ddelt", "ndgs"), allowed=("shape", "ddelt", "ndgs"), path="radar.solver")
    if solver["shape"] != "spheroid":
        raise GeneratorError("solver shape must be spheroid")
    if _number(solver["ddelt"], "radar.solver.ddelt", positive=True) != 0.001:
        raise GeneratorError("radar.solver.ddelt must be exactly 0.001")
    expected_ndgs = (
        PROPERTY_SOLVER_NDGS
        if _uses_material_state_grouping(config)
        else CONVENTIONAL_SOLVER_NDGS
    )
    if _positive_integer(solver["ndgs"], "radar.solver.ndgs") != expected_ndgs:
        raise GeneratorError(
            f"radar.solver.ndgs must be exactly {expected_ndgs} for this table"
        )

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
            "drag_transition_boundary_policy",
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
        if terminal["drag_transition_boundary_policy"] != (
            "select_exact_transition_reynolds_boundary_when_piecewise_drag_"
            "residual_jump_straddles_zero"
        ):
            raise GeneratorError("unsupported terminal drag-transition boundary policy")
    else:
        raise GeneratorError(f"unsupported terminal_velocity.law {law!r}")

    temporal = _require_object(config["temporal"], "temporal")
    _keys(temporal, required=("sampling",), allowed=("sampling",), path="temporal")
    if temporal["sampling"] != "instantaneous":
        raise GeneratorError("temporal sampling must be instantaneous")
    execution = _require_object(config["execution"], "execution")
    base_execution_keys = (
        "point_timeout_seconds",
        "process_isolation",
        "result_collection_order",
        "partial_grid_policy",
        "thread_count_per_process",
    )
    execution_keys = (
        base_execution_keys + ("grouping",)
        if _uses_material_state_grouping(config)
        else base_execution_keys
    )
    _keys(execution, required=execution_keys, allowed=execution_keys, path="execution")
    _positive_integer(execution["point_timeout_seconds"], "execution.point_timeout_seconds")
    expected_isolation = (
        "fresh_python_subprocess_per_material_state_group"
        if _uses_material_state_grouping(config)
        else "fresh_python_subprocess_per_grid_point"
    )
    if execution["process_isolation"] != expected_isolation:
        raise GeneratorError(
            f"execution.process_isolation must equal {expected_isolation!r}"
        )
    if execution["result_collection_order"] != "declared_axis_order_last_axis_fastest":
        raise GeneratorError("result collection order is not canonical")
    if execution["partial_grid_policy"] != "reject_entire_lut":
        raise GeneratorError("partial grids must be rejected")
    if execution["thread_count_per_process"] != 1:
        raise GeneratorError("thread_count_per_process must equal 1")
    if _uses_material_state_grouping(config):
        grouping = _require_object(execution["grouping"], "execution.grouping")
        material_state_axis_kinds = (
            (
                ["temperature", "bulk_density", "frequency"]
                if _property_phase_regime(config) == "dry"
                else [
                    "temperature",
                    "condensed_volume_fraction",
                    "liquid_mass_fraction",
                    "frequency",
                ]
            )
            if _is_property_aware(config)
            else ["temperature", "frequency"]
        )
        grouping_contract = {
            "model": "fresh_crash_isolated_material_state_process",
            "material_state_axis_kinds": material_state_axis_kinds,
            "tmatrix_state_axis_kinds": [
                "equivolume_diameter",
                "minor_to_major_axis_ratio",
            ],
            "geometry_axis_kind": "radar_elevation",
            "partial_group_policy": "reject_entire_lut",
        }
        grouping_keys = tuple(grouping_contract) + (
            "maximum_points_per_process",
            "group_timeout_seconds",
        )
        _keys(
            grouping,
            required=grouping_keys,
            allowed=grouping_keys,
            path="execution.grouping",
        )
        for key, expected in grouping_contract.items():
            if grouping[key] != expected:
                raise GeneratorError(f"execution.grouping.{key} must equal {expected!r}")
        maximum_points = _positive_integer(
            grouping["maximum_points_per_process"],
            "execution.grouping.maximum_points_per_process",
        )
        _positive_integer(
            grouping["group_timeout_seconds"],
            "execution.grouping.group_timeout_seconds",
        )
        in_process_points = (
            len(axis_coordinates(config, "equivolume_diameter"))
            * len(axis_coordinates(config, "minor_to_major_axis_ratio"))
            * len(axis_coordinates(config, "radar_elevation"))
        )
        if in_process_points > maximum_points:
            raise GeneratorError(
                "property-aware material-state group exceeds maximum_points_per_process"
            )
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


def _ice_permittivity_matzler_2006(temperature_k: float, frequency_hz: float) -> complex:
    """Pure-ice relative permittivity, Matzler (2006), PyTMatrix +i-loss."""
    temperature_k = _number(temperature_k, "temperature_k", positive=True)
    frequency_hz = _number(frequency_hz, "frequency_hz", positive=True)
    frequency_ghz = frequency_hz * 1.0e-9
    if not 190.0 <= temperature_k <= 273.15:
        raise GeneratorError("Matzler ice model is restricted here to [190, 273.15] K")
    if not 0.01 <= frequency_ghz <= 300.0:
        raise GeneratorError("Matzler ice model is restricted to [0.01, 300] GHz")
    theta = 300.0 / temperature_k - 1.0
    alpha = (0.00504 + 0.0062 * theta) * math.exp(-22.1 * theta)
    exp_b = math.exp(335.0 / temperature_k)
    beta = (
        0.0207
        * exp_b
        / (temperature_k * (exp_b - 1.0) * (exp_b - 1.0))
        + 1.16e-11 * frequency_ghz * frequency_ghz
        + math.exp(-9.963 + 0.0372 * (temperature_k - 273.16))
    )
    real = 3.1884 + 9.1e-4 * (temperature_k - 273.0)
    imaginary = alpha / frequency_ghz + beta * frequency_ghz
    if not all(math.isfinite(value) and value >= 0.0 for value in (real, imaginary)):
        raise GeneratorError("Matzler ice model produced a non-passive value")
    return complex(real, imaginary)


def _water_permittivity_liebe_1991(
    temperature_k: float, frequency_hz: float
) -> complex:
    """Liebe-Hufford-Manabe (1991) double-Debye water, +i-loss convention."""
    temperature_k = _number(temperature_k, "temperature_k", positive=True)
    frequency_hz = _number(frequency_hz, "frequency_hz", positive=True)
    frequency_ghz = frequency_hz * 1.0e-9
    if not 250.0 <= temperature_k <= 313.15:
        raise GeneratorError(
            "this research use of Liebe-Hufford-Manabe water is restricted to [250, 313.15] K"
        )
    theta = 1.0 - 300.0 / temperature_k
    epsilon_0 = 77.66 - 103.3 * theta
    epsilon_1 = 0.0671 * epsilon_0
    epsilon_2 = 3.52 + 7.52 * theta
    gamma_1 = 20.20 + 146.4 * theta + 316.0 * theta * theta
    gamma_2 = 39.8 * gamma_1
    if gamma_1 <= 0.0 or gamma_2 <= 0.0:
        raise GeneratorError("Liebe-Hufford-Manabe relaxation frequency is nonpositive")
    result = (
        complex(epsilon_2, 0.0)
        + (epsilon_0 - epsilon_1) / complex(1.0, -frequency_ghz / gamma_1)
        + (epsilon_1 - epsilon_2) / complex(1.0, -frequency_ghz / gamma_2)
    )
    if not all(math.isfinite(value) for value in (result.real, result.imag)):
        raise GeneratorError("Liebe-Hufford-Manabe water model produced nonfinite output")
    if result.real <= 0.0 or result.imag < 0.0:
        raise GeneratorError("Liebe-Hufford-Manabe water model produced non-passive output")
    return result


def _component_volume_fractions(
    bulk_density_kg_m3: float,
    liquid_mass_fraction: float,
    ice_density_kg_m3: float,
    water_density_kg_m3: float,
) -> tuple[float, float, float]:
    bulk_density_kg_m3 = _number(
        bulk_density_kg_m3, "bulk_density_kg_m3", positive=True
    )
    liquid_mass_fraction = _number(liquid_mass_fraction, "liquid_mass_fraction")
    if not 0.0 <= liquid_mass_fraction <= 1.0:
        raise GeneratorError("liquid mass fraction must lie in [0, 1]")
    ice_density_kg_m3 = _number(
        ice_density_kg_m3, "ice_density_kg_m3", positive=True
    )
    water_density_kg_m3 = _number(
        water_density_kg_m3, "water_density_kg_m3", positive=True
    )
    ice_fraction = (
        bulk_density_kg_m3 * (1.0 - liquid_mass_fraction) / ice_density_kg_m3
    )
    water_fraction = (
        bulk_density_kg_m3 * liquid_mass_fraction / water_density_kg_m3
    )
    air_fraction = 1.0 - ice_fraction - water_fraction
    tolerance = 64.0 * sys.float_info.epsilon
    if air_fraction < -tolerance:
        raise GeneratorError(
            "bulk density and liquid mass fraction imply negative pore-air volume"
        )
    if air_fraction < 0.0:
        air_fraction = 0.0
    fractions = (air_fraction, ice_fraction, water_fraction)
    if any(not math.isfinite(value) or value < 0.0 for value in fractions):
        raise GeneratorError("component volume fractions are invalid")
    if abs(sum(fractions) - 1.0) > 128.0 * sys.float_info.epsilon:
        raise GeneratorError("component volume fractions do not sum to one")
    return fractions


def _wet_component_volume_fractions(
    condensed_volume_fraction: float,
    liquid_mass_fraction: float,
    ice_density_kg_m3: float,
    water_density_kg_m3: float,
) -> tuple[tuple[float, float, float], float]:
    condensed = _number(
        condensed_volume_fraction, "condensed_volume_fraction", positive=True
    )
    if condensed > 1.0:
        raise GeneratorError("condensed volume fraction must not exceed one")
    liquid = _number(liquid_mass_fraction, "liquid_mass_fraction")
    if not 0.0 <= liquid <= 1.0:
        raise GeneratorError("liquid mass fraction must lie in [0,1]")
    ice_density = _number(ice_density_kg_m3, "ice_density_kg_m3", positive=True)
    water_density = _number(
        water_density_kg_m3, "water_density_kg_m3", positive=True
    )
    ice_specific_volume = (1.0 - liquid) / ice_density
    water_specific_volume = liquid / water_density
    total_specific_volume = ice_specific_volume + water_specific_volume
    if total_specific_volume <= 0.0 or not math.isfinite(total_specific_volume):
        raise GeneratorError("wet component total specific volume is invalid")
    ice_fraction = condensed * ice_specific_volume / total_specific_volume
    water_fraction = condensed * water_specific_volume / total_specific_volume
    air_fraction = 1.0 - condensed
    fractions = (air_fraction, ice_fraction, water_fraction)
    if any(not math.isfinite(value) or value < 0.0 for value in fractions):
        raise GeneratorError("wet component volume fractions are invalid")
    if abs(sum(fractions) - 1.0) > 128.0 * sys.float_info.epsilon:
        raise GeneratorError("wet component volume fractions do not sum to one")
    bulk_density = condensed / total_specific_volume
    if not math.isfinite(bulk_density) or bulk_density <= 0.0:
        raise GeneratorError("reconstructed wet bulk density is invalid")
    return fractions, bulk_density


def residual_rain_mass_after_wet_pairing(
    total_rain_mass: float, paired_liquid_masses: Sequence[float]
) -> float:
    """Allocate rain exactly once after wet-frozen pairing; reject over-pairing."""
    total = _number(total_rain_mass, "total_rain_mass")
    if total < 0.0:
        raise GeneratorError("total rain mass must be nonnegative")
    paired_values = [
        _number(value, f"paired_liquid_masses[{index}]")
        for index, value in enumerate(paired_liquid_masses)
    ]
    if any(value < 0.0 for value in paired_values):
        raise GeneratorError("paired liquid masses must be nonnegative")
    paired = math.fsum(paired_values)
    if paired > total:
        raise GeneratorError("paired liquid mass exceeds total rain mass")
    residual = total - paired
    if not math.isfinite(residual) or residual < 0.0:
        raise GeneratorError("residual rain mass is invalid")
    return residual


def _bruggeman_residual(
    effective_permittivity: complex,
    constituent_permittivities: Sequence[complex],
    volume_fractions: Sequence[float],
) -> complex:
    return sum(
        fraction
        * (constituent - effective_permittivity)
        / (constituent + 2.0 * effective_permittivity)
        for constituent, fraction in zip(
            constituent_permittivities, volume_fractions
        )
    )


def _bruggeman_newton(
    initial: complex,
    constituent_permittivities: Sequence[complex],
    volume_fractions: Sequence[float],
    *,
    maximum_iterations: int,
    tolerance: float,
) -> complex | None:
    value = initial
    for _ in range(maximum_iterations):
        residual = _bruggeman_residual(
            value, constituent_permittivities, volume_fractions
        )
        if abs(residual) <= tolerance:
            return value
        derivative = -3.0 * sum(
            fraction
            * constituent
            / (constituent + 2.0 * value) ** 2
            for constituent, fraction in zip(
                constituent_permittivities, volume_fractions
            )
        )
        if not math.isfinite(derivative.real) or not math.isfinite(derivative.imag):
            return None
        if abs(derivative) <= sys.float_info.min:
            return None
        step = residual / derivative
        accepted = False
        for line_search in range(24):
            scale = 2.0 ** (-line_search)
            candidate = value - scale * step
            if (
                not math.isfinite(candidate.real)
                or not math.isfinite(candidate.imag)
                or candidate.real <= 0.0
                or candidate.imag < -tolerance
            ):
                continue
            constituent_scale = max(
                1.0,
                abs(candidate),
                *(abs(component) for component in constituent_permittivities),
            )
            minimum_denominator = min(
                abs(component + 2.0 * candidate)
                for component in constituent_permittivities
            )
            if minimum_denominator <= 1.0e-12 * constituent_scale:
                continue
            candidate_residual = _bruggeman_residual(
                candidate, constituent_permittivities, volume_fractions
            )
            if abs(candidate_residual) < abs(residual):
                value = candidate
                accepted = True
                break
        if not accepted:
            return None
    residual = _bruggeman_residual(value, constituent_permittivities, volume_fractions)
    return value if abs(residual) <= tolerance else None


def _bruggeman_continuation(
    constituent_permittivities: Sequence[complex],
    volume_fractions: Sequence[float],
    *,
    initial_steps: int,
    maximum_iterations: int,
    tolerance: float,
) -> complex:
    value = complex(1.0, 0.0)
    progress = 0.0
    step = 1.0 / initial_steps
    minimum_step = 1.0 / (initial_steps * 4096.0)
    while progress < 1.0:
        candidate_progress = min(1.0, progress + step)
        staged = [
            1.0 + candidate_progress * (component - 1.0)
            for component in constituent_permittivities
        ]
        candidate = _bruggeman_newton(
            value,
            staged,
            volume_fractions,
            maximum_iterations=maximum_iterations,
            tolerance=tolerance,
        )
        if candidate is None:
            step *= 0.5
            if step < minimum_step:
                raise GeneratorError(
                    "symmetric Bruggeman passive-branch homotopy did not converge"
                )
            continue
        value = candidate
        progress = candidate_progress
        step = min(1.0 / initial_steps, step * 2.0)
    return value


def _symmetric_bruggeman_permittivity(
    constituent_permittivities: Sequence[complex],
    volume_fractions: Sequence[float],
    *,
    homotopy_steps: int,
    maximum_iterations: int,
    tolerance: float,
) -> complex:
    if len(constituent_permittivities) != len(volume_fractions):
        raise GeneratorError("Bruggeman constituents and fractions differ in length")
    active = [
        (complex(permittivity), float(fraction))
        for permittivity, fraction in zip(
            constituent_permittivities, volume_fractions
        )
        if float(fraction) > 0.0
    ]
    if not active:
        raise GeneratorError("Bruggeman mixture has no active constituent")
    total = sum(fraction for _, fraction in active)
    active = [(permittivity, fraction / total) for permittivity, fraction in active]
    for permittivity, fraction in active:
        if (
            not math.isfinite(permittivity.real)
            or not math.isfinite(permittivity.imag)
            or permittivity.real <= 0.0
            or permittivity.imag < 0.0
            or not math.isfinite(fraction)
            or fraction <= 0.0
        ):
            raise GeneratorError("Bruggeman mixture contains an invalid passive constituent")
    if len(active) == 1:
        return active[0][0]
    permittivities = [item[0] for item in active]
    fractions = [item[1] for item in active]
    result = _bruggeman_continuation(
        permittivities,
        fractions,
        initial_steps=homotopy_steps,
        maximum_iterations=maximum_iterations,
        tolerance=tolerance,
    )
    refined = _bruggeman_continuation(
        permittivities,
        fractions,
        initial_steps=2 * homotopy_steps,
        maximum_iterations=maximum_iterations,
        tolerance=tolerance,
    )
    agreement_tolerance = 256.0 * tolerance * max(1.0, abs(result), abs(refined))
    if abs(result - refined) > agreement_tolerance:
        raise GeneratorError("Bruggeman homotopy step refinement selected another root")
    residual = _bruggeman_residual(refined, permittivities, fractions)
    if abs(residual) > tolerance:
        raise GeneratorError("Bruggeman rational residual exceeds configured tolerance")
    if refined.real <= 0.0 or refined.imag < -tolerance:
        raise GeneratorError("Bruggeman root is not on the passive positive-real branch")
    return complex(refined.real, max(refined.imag, 0.0))


def _passive_refractive_index(permittivity: complex) -> complex:
    result = permittivity**0.5
    if result.real < 0.0:
        result = -result
    if result.imag < 0.0:
        result = result.conjugate()
    if result.real < 0.0 or result.imag < 0.0:
        raise GeneratorError("could not select passive refractive-index square root")
    return result


def _material(config: dict[str, Any], coordinates: dict[str, float]) -> tuple[complex, float]:
    dielectric = config["dielectric"]
    if dielectric["model"] == "explicit_homogeneous":
        return (
            _complex_index(dielectric["refractive_index"], "dielectric.refractive_index"),
            float(dielectric["mass_density_kg_m3"]),
        )

    if dielectric["model"] == TEMPERATURE_WATER_DIELECTRIC_MODEL:
        permittivity = _water_permittivity_liebe_1991(
            coordinates["temperature"], coordinates["frequency"]
        )
        return (
            _passive_refractive_index(permittivity),
            float(dielectric["mass_density_kg_m3"]),
        )

    if dielectric["model"] == WET_PROPERTY_DIELECTRIC_MODEL:
        temperature_k = coordinates["temperature"]
        frequency_hz = coordinates["frequency"]
        condensed_volume_fraction = coordinates["condensed_volume_fraction"]
        liquid_mass_fraction = coordinates["liquid_mass_fraction"]
        ice_density = float(dielectric["ice_material_density_kg_m3"])
        water_density = float(dielectric["liquid_water_density_kg_m3"])
        fractions, bulk_density = _wet_component_volume_fractions(
            condensed_volume_fraction,
            liquid_mass_fraction,
            ice_density,
            water_density,
        )
        permittivity = _symmetric_bruggeman_permittivity(
            (
                _complex_index(
                    dielectric["air_relative_permittivity"],
                    "dielectric.air_relative_permittivity",
                ),
                _ice_permittivity_matzler_2006(
                    min(temperature_k, 273.15), frequency_hz
                ),
                _water_permittivity_liebe_1991(temperature_k, frequency_hz),
            ),
            fractions,
            homotopy_steps=int(dielectric["homotopy_steps"]),
            maximum_iterations=int(dielectric["newton_max_iterations"]),
            tolerance=float(dielectric["newton_relative_tolerance"]),
        )
        return _passive_refractive_index(permittivity), bulk_density

    if dielectric["model"] == DRY_PROPERTY_DIELECTRIC_MODEL:
        temperature_k = coordinates["temperature"]
        frequency_hz = coordinates["frequency"]
        bulk_density = coordinates["bulk_density"]
        ice_density = float(dielectric["ice_material_density_kg_m3"])
        ice_fraction = bulk_density / ice_density
        if not 0.0 < ice_fraction <= 1.0:
            raise GeneratorError("dry-property ice volume fraction is outside (0,1]")
        permittivity = _symmetric_bruggeman_permittivity(
            (
                _complex_index(
                    dielectric["air_relative_permittivity"],
                    "dielectric.air_relative_permittivity",
                ),
                _ice_permittivity_matzler_2006(temperature_k, frequency_hz),
            ),
            (1.0 - ice_fraction, ice_fraction),
            homotopy_steps=int(dielectric["homotopy_steps"]),
            maximum_iterations=int(dielectric["newton_max_iterations"]),
            tolerance=float(dielectric["newton_relative_tolerance"]),
        )
        return _passive_refractive_index(permittivity), bulk_density

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
    return _passive_refractive_index(effective_permittivity), mixture_density


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
    def drag_at_speed(speed: float) -> float:
        reynolds = max(air_density * speed * diameter_m / viscosity, 1.0e-15)
        if reynolds < transition:
            return (24.0 / reynolds) * (1.0 + 0.15 * reynolds**0.687)
        return high_re_drag

    force_scale = (4.0 * gravity * diameter_m * density_difference) / (
        3.0 * air_density
    )

    transition_speed = transition * viscosity / (air_density * diameter_m)
    low_re_transition_drag = (24.0 / transition) * (
        1.0 + 0.15 * transition**0.687
    )
    residual_below_transition = (
        transition_speed * transition_speed * low_re_transition_drag - force_scale
    )
    residual_above_transition = (
        transition_speed * transition_speed * high_re_drag - force_scale
    )
    if residual_below_transition <= 0.0 <= residual_above_transition:
        # The piecewise Cd approximation has a small upward jump at Re=1000.
        # When that jump straddles zero there is no exact force-balance root;
        # the declared policy selects the transition boundary itself.
        return transition_speed

    def residual(speed: float) -> float:
        return speed * speed * drag_at_speed(speed) - force_scale

    lower = 0.0
    upper = 1.0
    bracket_iterations = 0
    while residual(upper) < 0.0 and bracket_iterations < iterations:
        upper *= 2.0
        bracket_iterations += 1
    if residual(upper) < 0.0:
        raise GeneratorError(
            f"terminal-speed root could not be bracketed at D={diameter_m} m"
        )
    for _ in range(iterations - bracket_iterations):
        midpoint = 0.5 * (lower + upper)
        if upper - lower <= tolerance * max(midpoint, 1.0):
            return midpoint
        if residual(midpoint) < 0.0:
            lower = midpoint
        else:
            upper = midpoint
    raise GeneratorError(
        f"terminal-speed bisection did not converge at D={diameter_m} m"
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


def _validate_coordinates(config: dict[str, Any], coordinates: dict[str, float]) -> None:
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


def _radar_geometries(
    config: dict[str, Any], coordinates: dict[str, float]
) -> tuple[tuple[float, ...], tuple[float, ...]]:
    elevation_deg = float(coordinates["radar_elevation"])
    if not -90.0 <= elevation_deg <= 90.0:
        raise GeneratorError("radar elevation is outside PyTMatrix zenith-angle transform")
    radar_config = config["radar"]
    back_reference = tuple(
        float(value) for value in radar_config["backscatter_geometry_deg"]
    )
    forward_reference = tuple(
        float(value) for value in radar_config["forward_scatter_geometry_deg"]
    )
    if back_reference != (90.0, 90.0, 0.0, 180.0, 0.0, 0.0):
        raise GeneratorError("backscatter reference geometry changed after validation")
    if forward_reference != (90.0, 90.0, 0.0, 0.0, 0.0, 0.0):
        raise GeneratorError("forward reference geometry changed after validation")
    backscatter = (
        90.0 - elevation_deg,
        90.0 + elevation_deg,
        0.0,
        180.0,
        0.0,
        0.0,
    )
    forward = (
        90.0 - elevation_deg,
        90.0 - elevation_deg,
        0.0,
        0.0,
        0.0,
        0.0,
    )
    return backscatter, forward


def _prepare_scatterer(
    config: dict[str, Any],
    coordinates: dict[str, float],
    material: tuple[complex, float] | None = None,
) -> tuple[Any, float]:
    # Imports live only in isolated worker processes. A fatal Fortran STOP or
    # native crash therefore cannot leave the parent with a partial table.
    from pytmatrix import orientation as pytmatrix_orientation  # type: ignore[import-not-found]
    from pytmatrix.tmatrix import Scatterer  # type: ignore[import-not-found]

    _validate_coordinates(config, coordinates)

    diameter_m = coordinates["equivolume_diameter"]
    frequency_hz = coordinates["frequency"]
    minor_to_major = coordinates["minor_to_major_axis_ratio"]
    refractive_index, particle_density = material or _material(config, coordinates)
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
    return scatterer, particle_density


def _evaluate_prepared_scatterer(
    config: dict[str, Any],
    coordinates: dict[str, float],
    scatterer: Any,
    particle_density: float,
) -> list[float]:
    from pytmatrix import radar  # type: ignore[import-not-found]

    backscatter_geometry, forward_geometry = _radar_geometries(config, coordinates)
    scatterer.set_geometry(backscatter_geometry)
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

    scatterer.set_geometry(forward_geometry)
    kdp_single = float(radar.Kdp(scatterer))
    ah_single = _nonnegative(float(radar.Ai(scatterer, h_pol=True)), "AH")
    av_single = _nonnegative(float(radar.Ai(scatterer, h_pol=False)), "AV")
    if not math.isfinite(kdp_single):
        raise GeneratorError("KDP is non-finite")

    population = config["particle_population"]
    number_density = float(population["normalization_number_concentration_m3"])
    diameter_m = coordinates["equivolume_diameter"]
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


def compute_point(config: dict[str, Any], coordinates: dict[str, float]) -> list[float]:
    validate_config(config)
    scatterer, particle_density = _prepare_scatterer(config, coordinates)
    return _evaluate_prepared_scatterer(
        config, coordinates, scatterer, particle_density
    )


def _compute_material_state_group_unchecked(
    config: dict[str, Any], points: Sequence[dict[str, float]]
) -> list[list[float]]:
    if not _uses_material_state_grouping(config):
        raise GeneratorError("material-state grouping is not configured for this table")
    if not points:
        raise GeneratorError("material-state group is empty")
    grouping = config["execution"]["grouping"]
    material_axis_kinds = tuple(grouping["material_state_axis_kinds"])
    material_key = tuple(points[0][kind] for kind in material_axis_kinds)
    for point in points:
        _validate_coordinates(config, point)
        if tuple(point[kind] for kind in material_axis_kinds) != material_key:
            raise GeneratorError("material-state group mixes material coordinates")
    material = _material(config, points[0])
    tmatrix_axis_kinds = tuple(grouping["tmatrix_state_axis_kinds"])
    current_tmatrix_key: tuple[float, ...] | None = None
    scatterer: Any | None = None
    particle_density = material[1]
    results: list[list[float]] = []
    for point in points:
        tmatrix_key = tuple(point[kind] for kind in tmatrix_axis_kinds)
        if tmatrix_key != current_tmatrix_key:
            scatterer, particle_density = _prepare_scatterer(
                config, point, material=material
            )
            current_tmatrix_key = tmatrix_key
        if scatterer is None:
            raise GeneratorError("internal grouped scatterer was not initialized")
        results.append(
            _evaluate_prepared_scatterer(
                config, point, scatterer, particle_density
            )
        )
    return results


def compute_material_state_group(
    config: dict[str, Any], points: Sequence[dict[str, float]]
) -> list[list[float]]:
    """Evaluate one crash-isolated material group, reusing T matrices by view angle."""
    validate_config(config)
    return _compute_material_state_group_unchecked(config, points)


def compute_solver_ndgs_comparison_group(
    config: dict[str, Any],
    points: Sequence[dict[str, float]],
    solver_ndgs: int,
) -> list[list[float]]:
    """Evaluate a validated property group at a predeclared comparison ndgs."""
    validate_config(config)
    if solver_ndgs not in CONVERGENCE_SOLVER_NDGS:
        raise GeneratorError(
            f"comparison solver ndgs must be one of {CONVERGENCE_SOLVER_NDGS}"
        )
    comparison = copy.deepcopy(config)
    comparison["radar"]["solver"]["ndgs"] = solver_ndgs
    return _compute_material_state_group_unchecked(comparison, points)


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


def run_isolated_material_state_group(
    config: dict[str, Any],
    points: Sequence[dict[str, float]],
    timeout_seconds: int,
) -> list[list[float]]:
    request = json.dumps(
        {"config": config, "points": points},
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
            [sys.executable, str(Path(__file__).resolve()), "_group_worker"],
            input=request,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise GeneratorError(
            f"material-state group of {len(points)} points exceeded {timeout_seconds} s"
        ) from error
    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    if completed.returncode != 0:
        raise GeneratorError(
            f"material-state group worker exited {completed.returncode}; "
            f"stdout={stdout[-4000:]!r}; stderr={stderr[-4000:]!r}"
        )
    result_lines = [
        line for line in stdout.splitlines() if line.startswith(GROUP_MARKER)
    ]
    if len(result_lines) != 1:
        raise GeneratorError(
            f"material-state group emitted {len(result_lines)} result markers; "
            f"stdout={stdout[-4000:]!r}"
        )
    try:
        decoded = json.loads(result_lines[0][len(GROUP_MARKER) :])
    except json.JSONDecodeError as error:
        raise GeneratorError("material-state group emitted invalid result JSON") from error
    if not isinstance(decoded, list) or len(decoded) != len(points):
        raise GeneratorError("material-state group result has incorrect point count")
    results: list[list[float]] = []
    for point_index, components in enumerate(decoded):
        if not isinstance(components, list):
            raise GeneratorError(
                f"material-state group point {point_index} is not an array"
            )
        values = [float(value) for value in components]
        validate_components(values)
        results.append(values)
    return results


def run_isolated_solver_ndgs_comparison_group(
    config: dict[str, Any],
    points: Sequence[dict[str, float]],
    solver_ndgs: int,
    timeout_seconds: int,
) -> list[list[float]]:
    request = json.dumps(
        {"config": config, "points": points, "solver_ndgs": solver_ndgs},
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
            [sys.executable, str(Path(__file__).resolve()), "_ndgs_group_worker"],
            input=request,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise GeneratorError(
            f"ndgs={solver_ndgs} comparison group of {len(points)} points "
            f"exceeded {timeout_seconds} s"
        ) from error
    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    if completed.returncode != 0:
        raise GeneratorError(
            f"ndgs={solver_ndgs} comparison worker exited {completed.returncode}; "
            f"stdout={stdout[-4000:]!r}; stderr={stderr[-4000:]!r}"
        )
    result_lines = [
        line for line in stdout.splitlines() if line.startswith(GROUP_MARKER)
    ]
    if len(result_lines) != 1:
        raise GeneratorError(
            f"ndgs={solver_ndgs} comparison group emitted {len(result_lines)} "
            f"result markers; stdout={stdout[-4000:]!r}"
        )
    try:
        decoded = json.loads(result_lines[0][len(GROUP_MARKER) :])
    except json.JSONDecodeError as error:
        raise GeneratorError(
            f"ndgs={solver_ndgs} comparison group emitted invalid result JSON"
        ) from error
    if not isinstance(decoded, list) or len(decoded) != len(points):
        raise GeneratorError(
            f"ndgs={solver_ndgs} comparison group result has incorrect point count"
        )
    results: list[list[float]] = []
    for point_index, components in enumerate(decoded):
        if not isinstance(components, list):
            raise GeneratorError(
                f"ndgs={solver_ndgs} comparison point {point_index} is not an array"
            )
        values = [float(value) for value in components]
        validate_components(values)
        results.append(values)
    return results


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


def _group_worker() -> int:
    try:
        request = json.loads(
            sys.stdin.buffer.read().decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite_constant,
        )
        if not isinstance(request, dict) or set(request) != {"config", "points"}:
            raise GeneratorError("group worker request has incorrect fields")
        raw_points = request["points"]
        if not isinstance(raw_points, list):
            raise GeneratorError("group worker points must be an array")
        points = [
            {
                str(key): _number(value, f"group.points[{index}].{key}")
                for key, value in _require_object(
                    raw_point, f"group.points[{index}]"
                ).items()
            }
            for index, raw_point in enumerate(raw_points)
        ]
        results = compute_material_state_group(
            _require_object(request["config"], "group.config"), points
        )
        print(
            GROUP_MARKER
            + json.dumps(results, separators=(",", ":"), allow_nan=False),
            flush=True,
        )
        return 0
    except BaseException as error:
        print(f"group worker failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


def _ndgs_group_worker() -> int:
    try:
        request = json.loads(
            sys.stdin.buffer.read().decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite_constant,
        )
        if not isinstance(request, dict) or set(request) != {
            "config",
            "points",
            "solver_ndgs",
        }:
            raise GeneratorError("ndgs comparison worker request has incorrect fields")
        raw_points = request["points"]
        if not isinstance(raw_points, list):
            raise GeneratorError("ndgs comparison worker points must be an array")
        points = [
            {
                str(key): _number(value, f"ndgs.points[{index}].{key}")
                for key, value in _require_object(
                    raw_point, f"ndgs.points[{index}]"
                ).items()
            }
            for index, raw_point in enumerate(raw_points)
        ]
        results = compute_solver_ndgs_comparison_group(
            _require_object(request["config"], "ndgs.config"),
            points,
            _positive_integer(request["solver_ndgs"], "ndgs.solver_ndgs"),
        )
        print(
            GROUP_MARKER
            + json.dumps(results, separators=(",", ":"), allow_nan=False),
            flush=True,
        )
        return 0
    except BaseException as error:
        print(
            f"ndgs comparison worker failed: {type(error).__name__}: {error}",
            file=sys.stderr,
        )
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
    model = dielectric["model"]
    if model == "maxwell_garnett_ice_host_water_inclusion":
        melting = {"model": "homogeneous_effective_medium", "rule": "maxwell_garnett"}
    elif model == WET_PROPERTY_DIELECTRIC_MODEL:
        melting = {"model": "homogeneous_effective_medium", "rule": "bruggeman"}
    elif model in (DRY_PROPERTY_DIELECTRIC_MODEL, TEMPERATURE_WATER_DIELECTRIC_MODEL):
        melting = {"model": "dry"}
    elif model == "explicit_homogeneous":
        melting = {"model": "dry"}
    else:
        raise GeneratorError(f"unsupported science metadata dielectric model {model!r}")
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


def scope_metadata(config: dict[str, Any]) -> dict[str, Any]:
    if _is_property_aware(config):
        descriptor = config["particle_population"]["state_descriptor"]
        return {
            "microphysics_family": PROPERTY_FAMILY,
            "phase_regime": _property_phase_regime(config),
            "source_closed_state_families": ["p3", "ishmael"],
            "p3_coverage": False,
            "ishmael_coverage": False,
            "p3_ishmael_style_closed_state_coordinate_coverage": True,
            "scheme_native_psd_coverage": False,
            "psd": (
                "characteristic_particle_monodisperse_node_normalized_to_"
                "exactly_1_per_m3"
            ),
            "rime_axes_explicit": False,
            "rime_mapping": descriptor["rime_axes"],
            "rime_effect_on_dielectric": descriptor["rime_effect_on_dielectric"],
            "density_applicability": descriptor["density_applicability"],
            "radar_view": config["radar"]["view_applicability"],
            "extrapolation": "forbidden",
        }
    if _is_residual_rain(config):
        coexistence = config["particle_population"]["coexistence_descriptor"]
        return {
            "microphysics_family": "conventional",
            "category": "rain",
            "coexistence_role": coexistence["role"],
            "standalone_rain_coverage": True,
            "residual_after_wet_pairing_coverage": True,
            "p3_coverage": False,
            "ishmael_coverage": False,
            "psd": "monodisperse_node_normalized_to_exactly_1_per_m3",
            "mass_allocation_rule": coexistence["allocation_rule"],
            "double_count_policy": coexistence["double_count_policy"],
            "over_pairing_policy": coexistence["over_pairing_policy"],
            "radar_view": config["radar"]["view_applicability"],
            "extrapolation": "forbidden",
        }
    return {
        "microphysics_family": "conventional",
        "p3_coverage": False,
        "ishmael_coverage": False,
        "psd": "monodisperse_node_normalized_to_exactly_1_per_m3",
        "radar_view": config["radar"]["view_applicability"],
        "extrapolation": "forbidden",
    }


def _compute_isolated_grid(
    config: dict[str, Any], coordinates: Sequence[dict[str, float]]
) -> tuple[list[list[float]], int, int]:
    """Compute isolated solver points concurrently and restore flat grid order."""
    timeout_seconds = int(config["execution"]["point_timeout_seconds"])
    if _uses_material_state_grouping(config):
        grouping = config["execution"]["grouping"]
        material_axes = tuple(grouping["material_state_axis_kinds"])
        grouped: dict[tuple[float, ...], list[tuple[int, dict[str, float]]]] = {}
        for flat_index, point in enumerate(coordinates):
            key = tuple(point[kind] for kind in material_axes)
            grouped.setdefault(key, []).append((flat_index, point))
        group_items = list(grouped.values())
        collected: list[list[float] | None] = [None] * len(coordinates)
        group_timeout = int(grouping["group_timeout_seconds"])

        def evaluate_group(
            indexed_entries: tuple[int, list[tuple[int, dict[str, float]]]],
        ) -> tuple[int, list[tuple[int, dict[str, float]]], list[list[float]]]:
            group_index, entries = indexed_entries
            group_points = [point for _, point in entries]
            try:
                group_values = run_isolated_material_state_group(
                    config, group_points, group_timeout
                )
            except GeneratorError as error:
                raise GeneratorError(
                    f"material-state group {group_index} failed: {error}"
                ) from error
            return group_index, entries, group_values

        with concurrent.futures.ThreadPoolExecutor(
            max_workers=GENERATION_WORKERS
        ) as executor:
            futures = [
                executor.submit(evaluate_group, item)
                for item in enumerate(group_items)
            ]
            for future in concurrent.futures.as_completed(futures):
                _, entries, group_values = future.result()
                for (flat_index, _), point_values in zip(entries, group_values):
                    collected[flat_index] = point_values
        if any(point is None for point in collected):
            raise GeneratorError("grouped generation left an uncomputed grid point")
        values = [point for point in collected if point is not None]
        return (
            values,
            len(grouped),
            max(len(entries) for entries in grouped.values()),
        )

    collected = [None] * len(coordinates)

    def evaluate_point(
        indexed_point: tuple[int, dict[str, float]],
    ) -> tuple[int, list[float]]:
        flat_index, point = indexed_point
        try:
            return flat_index, run_isolated_point(config, point, timeout_seconds)
        except GeneratorError as error:
            raise GeneratorError(f"flat grid point {flat_index} failed: {error}") from error

    with concurrent.futures.ThreadPoolExecutor(
        max_workers=GENERATION_WORKERS
    ) as executor:
        futures = [
            executor.submit(evaluate_point, item)
            for item in enumerate(coordinates)
        ]
        for future in concurrent.futures.as_completed(futures):
            flat_index, point_values = future.result()
            collected[flat_index] = point_values
    if any(point is None for point in collected):
        raise GeneratorError("point generation left an uncomputed grid point")
    return ([point for point in collected if point is not None], len(coordinates), 1)


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
    values, isolated_process_count, maximum_points_per_process = (
        _compute_isolated_grid(config, coordinates)
    )

    header_axes = [
        {
            "kind": axis["kind"],
            "unit": axis["unit"],
            "coordinates": [float(value) for value in axis["coordinates"]],
        }
        for axis in config["axes"]
    ]
    request = {
        "axes": [
            {
                "kind": axis["kind"],
                "unit": axis["unit"],
                "coordinate_f64_bits_hex": [
                    f64_bits_hex(value) for value in axis["coordinates"]
                ],
            }
            for axis in header_axes
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
        "axes": header_axes,
        "grid_point_count": len(values),
        "isolated_solver_process_count": isolated_process_count,
        "maximum_solver_points_per_process": maximum_points_per_process,
        "generation_worker_limit": GENERATION_WORKERS,
        "generation_effective_worker_count": min(
            GENERATION_WORKERS, isolated_process_count
        ),
        "solver_failures": [],
        "tool_file_sha256": hash_tool_files(tool_root),
        "scope": scope_metadata(config),
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
    if arguments == ["_group_worker"]:
        return _group_worker()
    if arguments == ["_ndgs_group_worker"]:
        return _ndgs_group_worker()
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
