#!/usr/bin/env python3
"""Select deterministic one-axis-at-a-time LUT grid-design audit nodes."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
from typing import Any, Sequence


ASSET_DIRECTORIES = (
    "conventional_liquid_rain_sband_unvalidated",
    "conventional_dry_ice_spheroids_sband_unvalidated",
    "conventional_wet_hail_sband_unvalidated",
    "property_p3_ishmael_dry_oblate_sband_unvalidated",
    "property_p3_ishmael_dry_prolate_sband_unvalidated",
    "property_p3_ishmael_wet_oblate_sband_unvalidated",
    "property_p3_ishmael_wet_prolate_sband_unvalidated",
    "property_rain_sband_unvalidated",
)
MATERIAL_OR_SHAPE_AXES = {
    "bulk_density",
    "condensed_volume_fraction",
    "liquid_mass_fraction",
    "minor_to_major_axis_ratio",
}
SINGLE_FRACTION_AXES = {
    "equivolume_diameter",
    "temperature",
    "radar_elevation",
}
MULTI_FRACTIONS = (0.25, 0.5, 0.75)
SINGLE_FRACTIONS = (0.5,)


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_grid_design_generator", path)
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


def reference_node(axes: list[dict[str, Any]]) -> dict[str, float]:
    result = {}
    for axis in axes:
        coordinates = [float(value) for value in axis["coordinates"]]
        result[axis["kind"]] = coordinates[len(coordinates) // 2]
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-root", type=Path, required=True)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    generator = load_generator(args.tool_root.resolve())
    tables = []
    for asset_directory in ASSET_DIRECTORIES:
        config_path = args.asset_root.resolve() / asset_directory / "config.json"
        config_bytes, _, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        axes = config["axes"]
        reference = reference_node(axes)
        nodes = []
        metadata = []
        for axis in axes:
            kind = axis["kind"]
            coordinates = [float(value) for value in axis["coordinates"]]
            if len(coordinates) == 1:
                continue
            if kind in MATERIAL_OR_SHAPE_AXES:
                fractions = MULTI_FRACTIONS
            elif kind in SINGLE_FRACTION_AXES:
                fractions = SINGLE_FRACTIONS
            else:
                raise RuntimeError(f"unclassified grid-design axis {kind}")
            for interval, (lower, upper) in enumerate(
                zip(coordinates, coordinates[1:])
            ):
                for fraction in fractions:
                    node = dict(reference)
                    node[kind] = lower + fraction * (upper - lower)
                    nodes.append(node)
                    metadata.append(
                        {
                            "varied_axis": kind,
                            "interval_index": interval,
                            "lower": lower,
                            "upper": upper,
                            "within_cell_fraction": fraction,
                        }
                    )
        tables.append(
            {
                "asset_directory": asset_directory,
                "config_sha256_at_selection": sha256_bytes(config_bytes),
                "nodes": nodes,
                "node_metadata": metadata,
            }
        )
    request = {
        "schema": 1,
        "classification": "held_out_from_lut_interpolation_check_only",
        "selection_protocol": (
            "Deterministic grid-design audit selected before refinement. Exactly one axis "
            "varies at a time while every other non-singleton axis is held at its declared "
            "interior midpoint-index LUT coordinate. Diameter, temperature, and elevation "
            "use each cell midpoint; material and shape axes use fixed 0.25, 0.5, and 0.75 "
            "within-cell fractions. Selection reads no scattering outputs. Results are grid "
            "design evidence and are not a final held-out validation set."
        ),
        "selection_seed": "none-deterministic-one-axis-cell-audit-v1",
        "selector_filename": Path(__file__).name,
        "selector_source_sha256": sha256_bytes(Path(__file__).read_bytes()),
        "thresholds": {
            "zh": {"relative": 0.5, "absolute": 1e-12},
            "zv": {"relative": 0.5, "absolute": 1e-12},
            "hh_vv_covariance_real": {"relative": 0.5, "absolute": 1e-12},
            "hh_vv_covariance_imaginary": {"relative": 0.5, "absolute": 1e-12},
            "kdp": {"relative": 0.5, "absolute": 1e-12},
            "ah": {"relative": 0.5, "absolute": 1e-12},
            "av": {"relative": 0.5, "absolute": 1e-12},
            "fall_speed_first_moment": {"relative": 0.75, "absolute": 1e-12},
            "fall_speed_second_moment": {"relative": 0.75, "absolute": 1e-12},
        },
        "tables": tables,
    }
    write_json(args.output.resolve(), request)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
