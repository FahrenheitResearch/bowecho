#!/usr/bin/env python3
"""Select deterministic off-grid nodes without evaluating scattering outputs."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
from typing import Any, Sequence


SEED = "bowecho-pytmatrix-0.3.3-post-grid-heldout-v5-refined-v10-final"
CONVENTIONAL_ASSET_DIRECTORIES = (
    "conventional_liquid_rain_sband_unvalidated",
    "conventional_dry_ice_spheroids_sband_unvalidated",
    "conventional_wet_hail_sband_unvalidated",
)
PROPERTY_ASSET_DIRECTORIES = (
    "property_p3_ishmael_dry_oblate_sband_unvalidated",
    "property_p3_ishmael_dry_prolate_sband_unvalidated",
    "property_p3_ishmael_wet_oblate_sband_unvalidated",
    "property_p3_ishmael_wet_prolate_sband_unvalidated",
    "property_rain_sband_unvalidated",
)
TABLE_SETS = {
    "conventional": CONVENTIONAL_ASSET_DIRECTORIES,
    "property": PROPERTY_ASSET_DIRECTORIES,
    "all": CONVENTIONAL_ASSET_DIRECTORIES + PROPERTY_ASSET_DIRECTORIES,
}
DIAMETER_QUANTILES = (0.08, 0.24, 0.40, 0.58, 0.76, 0.92)


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_selector_generator", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fraction(table_id: str, node_index: int, axis_kind: str) -> float:
    digest = hashlib.sha256(
        f"{SEED}\0{table_id}\0{node_index}\0{axis_kind}".encode("utf-8")
    ).digest()
    unit = int.from_bytes(digest[:8], "big") / float(1 << 64)
    return 0.2 + 0.6 * unit


def select_axis(
    table_id: str,
    node_index: int,
    kind: str,
    coordinates: list[float],
) -> float:
    if len(coordinates) == 1:
        return coordinates[0]
    if kind == "equivolume_diameter":
        quantile = DIAMETER_QUANTILES[node_index]
        interval = min(int(quantile * (len(coordinates) - 1)), len(coordinates) - 2)
    else:
        interval = node_index % (len(coordinates) - 1)
    lower = coordinates[interval]
    upper = coordinates[interval + 1]
    return lower + fraction(table_id, node_index, kind) * (upper - lower)


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
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--asset-set", choices=tuple(TABLE_SETS), default="all")
    args = parser.parse_args(argv)
    generator = load_generator(args.tool_root.resolve())
    tables = []
    for asset_directory in TABLE_SETS[args.asset_set]:
        config_path = args.asset_root.resolve() / asset_directory / "config.json"
        config_bytes, _, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        axes = config["axes"]
        nodes = []
        for node_index in range(len(DIAMETER_QUANTILES)):
            node = {}
            for axis in axes:
                coordinates = [float(value) for value in axis["coordinates"]]
                node[axis["kind"]] = select_axis(
                    config["table_id"], node_index, axis["kind"], coordinates
                )
            nodes.append(node)
        tables.append(
            {
                "asset_directory": asset_directory,
                "config_sha256_at_selection": sha256_bytes(config_bytes),
                "nodes": nodes,
            }
        )
    request = {
        "schema": 1,
        "classification": "held_out_from_lut_interpolation_check_only",
        "selection_protocol": (
            "Grid/config bytes were frozen before selection. This script uses only axis "
            "coordinates, a public fixed seed, fixed diameter quantiles, and SHA-256-derived "
            "within-cell fractions in [0.2,0.8]. It does not import PyTMatrix or inspect "
            "scattering outputs. No selected-node result may be used to alter these tables."
        ),
        "selection_seed": SEED,
        "asset_set": args.asset_set,
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
