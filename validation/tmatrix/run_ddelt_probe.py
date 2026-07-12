#!/usr/bin/env python3
"""Persist ddelt sensitivity at dry-prolate points rejected by convergence."""

from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import os
import subprocess
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
DDELT_VALUES = (0.001, 0.00025, 1.0e-5, 1.0e-6, 1.0e-7, 1.0e-8)
MARKER = "BRSLUT_DDELT_PROBE_RESULT="


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location("brslut_ddelt_probe_generator", path)
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


def worker() -> int:
    try:
        request = json.loads(sys.stdin.buffer.read())
        generator = load_generator(Path(request["tool_root"]).resolve())
        comparison = copy.deepcopy(request["config"])
        comparison["radar"]["solver"]["ddelt"] = float(request["ddelt"])
        probe_diameter = float(request["coordinates"]["equivolume_diameter"])
        for axis in comparison["axes"]:
            if axis["kind"] == "equivolume_diameter":
                axis["coordinates"] = sorted(
                    {float(value) for value in axis["coordinates"]} | {probe_diameter}
                )
        result = generator._compute_material_state_group_unchecked(
            comparison,
            [{key: float(value) for key, value in request["coordinates"].items()}],
        )[0]
        print(MARKER + json.dumps(result, separators=(",", ":")), flush=True)
        return 0
    except BaseException as error:
        print(f"ddelt probe worker failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


def run_worker(
    tool_root: Path,
    config: dict[str, Any],
    coordinates: dict[str, float],
    ddelt: float,
    timeout: int,
) -> dict[str, Any]:
    command = [sys.executable, str(Path(__file__).resolve()), "_worker"]
    request = json.dumps(
        {
            "tool_root": str(tool_root),
            "config": config,
            "coordinates": coordinates,
            "ddelt": ddelt,
        },
        sort_keys=True,
        separators=(",", ":"),
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
    completed = subprocess.run(
        command,
        input=request,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
        env=environment,
    )
    stdout = completed.stdout.decode("utf-8", errors="replace")
    stderr = completed.stderr.decode("utf-8", errors="replace")
    markers = [line for line in stdout.splitlines() if line.startswith(MARKER)]
    record: dict[str, Any] = {
        "ddelt": ddelt,
        "command": command,
        "returncode": completed.returncode,
        "stdout": stdout,
        "stderr": stderr,
        "result_marker_count": len(markers),
        "completed": completed.returncode == 0 and len(markers) == 1,
    }
    if record["completed"]:
        values = [float(value) for value in json.loads(markers[0][len(MARKER) :])]
        record["components"] = dict(zip(COMPONENT_NAMES, values))
    return record


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
    probe_path = Path(__file__).resolve()
    environment_path = args.environment_report.resolve()
    failure_path = args.failure_report.resolve()
    config_path = (
        args.asset_root.resolve()
        / "property_p3_ishmael_dry_prolate_sband_unvalidated"
        / "config.json"
    )
    initial_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "probe_source_sha256": sha256_file(probe_path),
        "environment_report_sha256": sha256_file(environment_path),
        "failure_report_sha256": sha256_file(failure_path),
        "config_sha256": sha256_file(config_path),
    }
    environment = json.loads(environment_path.read_text(encoding="utf-8"))
    if environment.get("tool_file_sha256", {}).get("generate_lut.py") != initial_hashes[
        "generator_source_sha256"
    ]:
        raise RuntimeError("environment report does not describe this generator source")
    generator = load_generator(tool_root)
    config_bytes, _, config = generator.parse_exact_json(config_path)
    generator.validate_config(config)
    failure = json.loads(failure_path.read_text(encoding="utf-8"))
    table = next(
        table
        for table in failure["tables"]
        if table["asset_directory"]
        == "property_p3_ishmael_dry_prolate_sband_unvalidated"
    )
    points = []
    seen = set()
    for rejected in table["recorded_component_tolerance_failures"]:
        coordinates = {
            key: float(value) for key, value in rejected["coordinates"].items()
        }
        key = tuple(sorted(coordinates.items()))
        if key not in seen:
            seen.add(key)
            points.append(coordinates)
    point_reports = []
    timeout = int(config["execution"]["point_timeout_seconds"])
    for coordinates in points:
        runs = [
            run_worker(tool_root, config, coordinates, ddelt, timeout)
            for ddelt in DDELT_VALUES
        ]
        baseline = runs[0].get("components")
        for run in runs:
            if baseline is not None and "components" in run:
                run["absolute_difference_from_ddelt_0p001"] = {
                    name: abs(run["components"][name] - baseline[name])
                    for name in COMPONENT_NAMES
                }
        point_reports.append({"coordinates": coordinates, "runs": runs})
    final_hashes = {
        "generator_source_sha256": sha256_file(generator_path),
        "probe_source_sha256": sha256_file(probe_path),
        "environment_report_sha256": sha256_file(environment_path),
        "failure_report_sha256": sha256_file(failure_path),
        "config_sha256": sha256_file(config_path),
    }
    if final_hashes != initial_hashes:
        raise RuntimeError("probe inputs changed while solver work was running")
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-dry-prolate-ddelt-sensitivity-v1",
        "classification": "solver_domain_design_probe_only",
        "scientifically_independent": False,
        "production_validation": False,
        "ddelt_values": list(DDELT_VALUES),
        "point_count": len(point_reports),
        "config_sha256": hashlib.sha256(config_bytes).hexdigest(),
        "source_hashes": initial_hashes,
        "points": point_reports,
        "interpretation_limit": (
            "This records sensitivity and native completion only. It does not establish "
            "an asymptotic limit or identify the cause of the ddelt=1e-8 native exits."
        ),
    }
    write_json(args.output.resolve(), report)
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["_worker"]:
        raise SystemExit(worker())
    raise SystemExit(main())
