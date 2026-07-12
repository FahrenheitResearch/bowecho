#!/usr/bin/env python3
"""Audit refined diameter slabs across every other-axis cell simultaneously."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import importlib.util
import itertools
import json
import math
import os
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable, Sequence


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
FRACTIONS = (0.2, 0.5, 0.8)
FRACTION_PERMUTATIONS = tuple(itertools.permutations(FRACTIONS))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_generator(tool_root: Path) -> Any:
    path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location(
        "brslut_cross_cell_audit_generator", path
    )
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


def point_key(point: dict[str, float], axis_kinds: Sequence[str]) -> tuple[float, ...]:
    return tuple(float(point[kind]) for kind in axis_kinds)


def interval_permutation(
    seed: str,
    stable_table_salt: str,
    table_id: str,
    axis_kind: str,
    lower: float,
    upper: float,
) -> tuple[float, float, float]:
    encoded = "\0".join(
        (
            seed,
            stable_table_salt,
            table_id,
            axis_kind,
            float(lower).hex(),
            float(upper).hex(),
        )
    ).encode("utf-8")
    index = int.from_bytes(hashlib.sha256(encoded).digest()[:8], "big") % len(
        FRACTION_PERMUTATIONS
    )
    return FRACTION_PERMUTATIONS[index]


def chunks(values: Sequence[Any], size: int) -> Iterable[Sequence[Any]]:
    for offset in range(0, len(values), size):
        yield values[offset : offset + size]


def evaluate_points(
    generator: Any,
    config: dict[str, Any],
    points: Sequence[dict[str, float]],
    worker_limit: int,
) -> tuple[list[list[float] | None], list[dict[str, Any]]]:
    execution = config["execution"]
    grouping = execution.get("grouping")
    if grouping is None:
        timeout = int(execution["point_timeout_seconds"])

        def evaluate(point: dict[str, float]) -> list[float]:
            return generator.run_isolated_point(config, point, timeout)

        restored: list[list[float] | None] = [None] * len(points)
        failures: list[dict[str, Any]] = []
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=min(worker_limit, len(points))
        ) as executor:
            futures = {
                executor.submit(evaluate, point): index
                for index, point in enumerate(points)
            }
            for future in concurrent.futures.as_completed(futures):
                index = futures[future]
                try:
                    restored[index] = future.result()
                except BaseException as error:
                    failures.append(
                        {
                            "point_index": index,
                            "error": f"{type(error).__name__}: {error}",
                        }
                    )
        return restored, sorted(failures, key=lambda item: item["point_index"])

    material_axes = tuple(
        str(kind) for kind in grouping["material_state_axis_kinds"]
    )
    grouped: dict[tuple[float, ...], list[tuple[int, dict[str, float]]]] = defaultdict(
        list
    )
    for index, point in enumerate(points):
        grouped[tuple(point[kind] for kind in material_axes)].append((index, point))
    maximum = int(grouping["maximum_points_per_process"])
    tasks: list[list[tuple[int, dict[str, float]]]] = []
    for key in sorted(grouped):
        tasks.extend(list(chunks(grouped[key], maximum)))
    timeout = int(grouping["group_timeout_seconds"])

    def evaluate_group(
        task: list[tuple[int, dict[str, float]]],
    ) -> tuple[list[int], list[list[float]]]:
        indices = [index for index, _ in task]
        task_points = [point for _, point in task]
        return (
            indices,
            generator.run_isolated_material_state_group(config, task_points, timeout),
        )

    restored: list[list[float] | None] = [None] * len(points)
    failures: list[dict[str, Any]] = []
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=min(worker_limit, len(tasks))
    ) as executor:
        futures = {
            executor.submit(evaluate_group, task): [index for index, _ in task]
            for task in tasks
        }
        for future in concurrent.futures.as_completed(futures):
            expected_indices = futures[future]
            try:
                indices, values = future.result()
                if indices != expected_indices or len(indices) != len(values):
                    raise RuntimeError("group result length or ordering mismatch")
                for index, result in zip(indices, values):
                    restored[index] = result
            except BaseException as error:
                message = f"{type(error).__name__}: {error}"
                failures.extend(
                    {"point_index": index, "error": message}
                    for index in expected_indices
                )
    return restored, sorted(failures, key=lambda item: item["point_index"])


def brackets(
    coordinates: Sequence[float], value: float
) -> tuple[tuple[float, float], ...]:
    values = [float(item) for item in coordinates]
    if len(values) == 1:
        if value != values[0]:
            raise RuntimeError("singleton coordinate differs")
        return ((values[0], 1.0),)
    if value in values:
        return ((value, 1.0),)
    upper_index = next(
        (index for index, candidate in enumerate(values) if candidate > value), None
    )
    if upper_index is None or upper_index == 0:
        raise RuntimeError(f"coordinate {value} is outside candidate grid")
    lower = values[upper_index - 1]
    upper = values[upper_index]
    fraction = (value - lower) / (upper - lower)
    return ((lower, 1.0 - fraction), (upper, fraction))


def interpolate(
    axes: Sequence[dict[str, Any]],
    grid_values: dict[tuple[float, ...], list[float]],
    point: dict[str, float],
) -> list[float]:
    axis_kinds = tuple(str(axis["kind"]) for axis in axes)
    per_axis = [
        brackets([float(value) for value in axis["coordinates"]], point[axis["kind"]])
        for axis in axes
    ]
    result = [0.0] * len(COMPONENT_NAMES)
    for corner in itertools.product(*per_axis):
        key = tuple(value for value, _ in corner)
        weight = math.prod(axis_weight for _, axis_weight in corner)
        values = grid_values.get(key)
        if values is None:
            raise RuntimeError(
                f"missing interpolation corner {dict(zip(axis_kinds, key))}"
            )
        for index, value in enumerate(values):
            result[index] += weight * float(value)
    return result


def errors(
    direct: Sequence[float],
    interpolated: Sequence[float],
    relative_budgets: dict[str, float],
    absolute_floor: float,
) -> tuple[list[dict[str, Any]], bool, float]:
    records = []
    passed = True
    worst_ratio = 0.0
    for name, expected, actual in zip(COMPONENT_NAMES, direct, interpolated):
        absolute = abs(float(actual) - float(expected))
        relative = absolute / max(abs(float(expected)), absolute_floor)
        budget = float(relative_budgets[name])
        within = absolute <= absolute_floor + budget * abs(float(expected))
        ratio = relative / budget
        worst_ratio = max(worst_ratio, ratio)
        passed = passed and within
        records.append(
            {
                "component": name,
                "absolute_error": absolute,
                "relative_error_with_absolute_floor": relative,
                "relative_budget": budget,
                "ratio_to_budget": ratio,
                "within_design_budget": within,
            }
        )
    return records, passed, worst_ratio


def leaf_depth(parent: tuple[float, float], leaf: tuple[float, float]) -> int:
    ratio = (parent[1] - parent[0]) / (leaf[1] - leaf[0])
    depth = int(round(math.log2(ratio)))
    if depth < 1 or not math.isclose(ratio, 2.0**depth, rel_tol=2e-12):
        raise RuntimeError(f"diameter leaf {leaf} is not a dyadic child of {parent}")
    return depth


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def exact_sha(path: Path, expected: str, label: str) -> None:
    actual = sha256_file(path)
    if actual != expected:
        raise RuntimeError(f"{label} SHA-256 mismatch: expected {expected}, got {actual}")


def intervals_for_value(
    coordinates: Sequence[float], value: float
) -> list[tuple[int, float, float]]:
    values = [float(item) for item in coordinates]
    if len(values) < 2:
        raise RuntimeError("cannot locate an interval on a singleton axis")
    containing = [
        (index, lower, upper)
        for index, (lower, upper) in enumerate(zip(values, values[1:]))
        if lower <= value <= upper
    ]
    if containing:
        return containing
    raise RuntimeError(f"coordinate {value} is outside the candidate grid")


def containing_leaves_for_value(
    leaves: Sequence[tuple[float, float]], value: float
) -> list[tuple[float, float]]:
    containing = [leaf for leaf in leaves if leaf[0] <= value <= leaf[1]]
    if not containing:
        raise RuntimeError(f"diameter coordinate {value} is outside candidate leaves")
    # A carried sample at an inserted knot is exact in diameter and touches
    # both adjacent cells. Preserve both in the failure lineage rather than
    # assigning the point arbitrarily to one side.
    return containing


def failed_components(error_records: Sequence[dict[str, Any]]) -> list[str]:
    return [
        str(record["component"])
        for record in error_records
        if not record["within_design_budget"]
    ]


def load_prior_regressions(
    prior: dict[str, Any], prior_sha256: str
) -> dict[str, list[dict[str, Any]]]:
    """Return every historically failing cross-cell coordinate, never just the
    failures that remain at the immediately preceding stage.
    """

    by_directory: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for table in prior.get("tables", []):
        directory = str(table["asset_directory"])
        inherited = table.get("accumulated_cross_cell_development_regressions", [])
        for item in inherited:
            by_directory[directory].append(
                {
                    "coordinates": {
                        str(key): float(value)
                        for key, value in item["coordinates"].items()
                    },
                    "first_failure_report_sha256": str(
                        item.get("first_failure_report_sha256", prior_sha256)
                    ),
                }
            )
        for item in table.get("failing_cross_cell_samples", []):
            by_directory[directory].append(
                {
                    "coordinates": {
                        str(key): float(value)
                        for key, value in item["coordinates"].items()
                    },
                    "first_failure_report_sha256": prior_sha256,
                }
            )

    deduplicated: dict[str, list[dict[str, Any]]] = {}
    for directory, items in by_directory.items():
        unique: dict[tuple[tuple[str, str], ...], dict[str, Any]] = {}
        for item in items:
            key = tuple(
                sorted(
                    (kind, float(value).hex())
                    for kind, value in item["coordinates"].items()
                )
            )
            unique.setdefault(key, item)
        deduplicated[directory] = list(unique.values())
    return deduplicated


def compact_failure(
    point: dict[str, float],
    metadata: dict[str, Any],
    error_records: Sequence[dict[str, Any]],
    worst_ratio: float,
) -> dict[str, Any]:
    return {
        **metadata,
        "coordinates": point,
        "failed_components": failed_components(error_records),
        "worst_ratio_to_design_budget": worst_ratio,
        "within_design_budget": False,
    }


def non_diameter_config_projection_sha256(
    config: dict[str, Any], parent_interval: tuple[float, float]
) -> str:
    projected = json.loads(json.dumps(config, allow_nan=False))
    diameter_axes = [
        axis
        for axis in projected["axes"]
        if axis["kind"] == "equivolume_diameter"
    ]
    if len(diameter_axes) != 1:
        raise RuntimeError("candidate must contain exactly one diameter axis")
    coordinates = [float(value) for value in diameter_axes[0]["coordinates"]]
    lower_index = coordinates.index(parent_interval[0])
    upper_index = coordinates.index(parent_interval[1])
    diameter_axes[0]["coordinates"] = (
        coordinates[:lower_index]
        + [
            parent_interval[0],
            "<stage-controlled-diameter-interior>",
            parent_interval[1],
        ]
        + coordinates[upper_index + 1 :]
    )
    encoded = json.dumps(
        projected, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-root", type=Path, required=True)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--protocol", type=Path, required=True)
    parser.add_argument("--protocol-amendment", type=Path, required=True)
    parser.add_argument("--legacy-protocol", type=Path, required=True)
    parser.add_argument("--legacy-depth1-report", type=Path, required=True)
    parser.add_argument("--stage-manifest", type=Path, required=True)
    parser.add_argument("--parent-failure-report", type=Path, required=True)
    parser.add_argument("--parent-nodes", type=Path, required=True)
    parser.add_argument("--prior-audit-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)

    tool_root = args.tool_root.resolve()
    asset_root = args.asset_root.resolve()
    environment_path = args.environment_report.resolve()
    protocol_path = args.protocol.resolve()
    amendment_path = args.protocol_amendment.resolve()
    legacy_protocol_path = args.legacy_protocol.resolve()
    legacy_depth1_path = args.legacy_depth1_report.resolve()
    stage_path = args.stage_manifest.resolve()
    parent_path = args.parent_failure_report.resolve()
    parent_nodes_path = args.parent_nodes.resolve()
    prior_path = args.prior_audit_report.resolve()
    output_path = args.output.resolve()
    if output_path.exists():
        raise RuntimeError(f"refusing to overwrite existing stage report: {output_path}")
    source_path = Path(__file__).resolve()
    generator_path = tool_root / "generate_lut.py"
    initial_hashes = {
        "audit_source_sha256": sha256_file(source_path),
        "generator_source_sha256": sha256_file(generator_path),
        "environment_report_sha256": sha256_file(environment_path),
        "protocol_sha256": sha256_file(protocol_path),
        "protocol_amendment_sha256": sha256_file(amendment_path),
        "legacy_protocol_sha256": sha256_file(legacy_protocol_path),
        "legacy_depth1_report_sha256": sha256_file(legacy_depth1_path),
        "stage_manifest_sha256": sha256_file(stage_path),
        "parent_failure_report_sha256": sha256_file(parent_path),
        "parent_nodes_sha256": sha256_file(parent_nodes_path),
        "prior_audit_report_sha256": sha256_file(prior_path),
    }
    protocol = json.loads(protocol_path.read_text(encoding="utf-8"))
    amendment = json.loads(amendment_path.read_text(encoding="utf-8"))
    stage = json.loads(stage_path.read_text(encoding="utf-8"))
    legacy_protocol_document = json.loads(
        legacy_protocol_path.read_text(encoding="utf-8")
    )
    parent = json.loads(parent_path.read_text(encoding="utf-8"))
    prior = json.loads(prior_path.read_text(encoding="utf-8"))
    legacy_depth1 = json.loads(legacy_depth1_path.read_text(encoding="utf-8"))
    require(protocol.get("schema") == 2, "cross-cell protocol schema must equal 2")
    require(
        protocol.get("protocol_id")
        == "pytmatrix-0.3.3-refined-grid-v10-cross-cell-design-v2",
        "unrecognized cross-cell protocol id",
    )
    require(stage.get("schema") == 1, "stage manifest schema must equal 1")
    require(amendment.get("schema") == 1, "protocol amendment schema must equal 1")
    require(
        amendment.get("amendment_id")
        == "pytmatrix-0.3.3-refined-grid-v10-integrity-amendment-v1",
        "unrecognized protocol amendment id",
    )
    require(
        amendment.get("protocol_sha256") == initial_hashes["protocol_sha256"],
        "protocol amendment names a different base protocol",
    )
    require(
        amendment.get("previous_audit_source_sha256")
        == protocol.get("audit_source_sha256"),
        "protocol amendment does not descend from the frozen depth-2 source",
    )
    require(
        amendment.get("audit_source_sha256")
        == initial_hashes["audit_source_sha256"],
        "protocol amendment does not freeze this audit source",
    )
    require(
        stage.get("protocol_sha256") == initial_hashes["protocol_sha256"],
        "stage manifest names a different protocol",
    )
    require(
        stage.get("protocol_amendment_sha256")
        == initial_hashes["protocol_amendment_sha256"],
        "stage manifest names a different protocol amendment",
    )
    require(
        stage.get("audit_source_sha256") == initial_hashes["audit_source_sha256"],
        "stage manifest does not freeze this audit source",
    )
    require(
        stage.get("generator_source_sha256")
        == initial_hashes["generator_source_sha256"],
        "stage manifest does not freeze this generator",
    )
    require(
        stage.get("environment_report_sha256")
        == initial_hashes["environment_report_sha256"],
        "stage manifest does not freeze this environment report",
    )
    require(
        stage.get("prior_audit_report_sha256")
        == initial_hashes["prior_audit_report_sha256"],
        "stage manifest names a different prior audit report",
    )
    require(
        stage.get("expected_output_filename") == output_path.name,
        "output filename differs from the frozen stage manifest",
    )
    legacy = protocol["legacy_protocol"]
    exact_sha(
        legacy_protocol_path,
        str(legacy["sha256"]),
        "legacy depth-1 protocol",
    )
    exact_sha(
        legacy_depth1_path,
        str(legacy["depth1_report_sha256"]),
        "legacy depth-1 audit report",
    )
    if prior.get("schema") == 1:
        require(
            prior.get("protocol_sha256") == legacy["sha256"],
            "prior depth-1 report does not bind the declared legacy protocol",
        )
        require(
            initial_hashes["prior_audit_report_sha256"]
            == legacy["depth1_report_sha256"],
            "prior report differs from the frozen legacy depth-1 evidence",
        )
        require(
            prior.get("audit_source_sha256")
            == legacy["depth1_audit_source_sha256"],
            "prior depth-1 report used an unexpected audit source",
        )
        require(
            int(prior.get("total_cross_cell_sample_count", -1))
            == int(legacy["depth1_cross_cell_sample_count"]),
            "prior depth-1 sample count differs from the protocol lineage",
        )
    elif prior.get("schema") == 2:
        prior_v9_regressions = [
            item
            for table in prior.get("tables", [])
            for item in table.get("v9_development_regressions", [])
        ]
        require(
            prior.get("report_id")
            == "pytmatrix-0.3.3-refined-v10-cross-cell-design-audit-v2",
            "prior schema-2 report has an unrecognized report id",
        )
        require(
            prior.get("protocol_sha256") == initial_hashes["protocol_sha256"],
            "prior schema-2 report binds a different base protocol",
        )
        require(
            prior.get("audit_source_sha256")
            in {
                amendment["previous_audit_source_sha256"],
                amendment["audit_source_sha256"],
            },
            "prior schema-2 report does not bind a recognized audit source",
        )
        require(
            prior.get("generator_source_sha256")
            == initial_hashes["generator_source_sha256"],
            "prior schema-2 report used a different generator",
        )
        require(
            prior.get("environment_report_sha256")
            == initial_hashes["environment_report_sha256"],
            "prior schema-2 report used a different environment",
        )
        require(
            prior.get("stage_manifest_sha256")
            == stage["prior_stage_manifest_sha256"],
            "prior schema-2 report does not bind the declared parent stage",
        )
        require(
            max(
                int(table["maximum_observed_diameter_depth"])
                for table in prior["tables"]
            )
            == int(stage["expected_parent_maximum_diameter_depth"]),
            "prior schema-2 report depth differs from the stage manifest",
        )
        require(
            int(prior.get("total_solver_failure_count", 0)) == 0,
            "prior schema-2 report contains solver failures; refinement is forbidden",
        )
        require(
            int(prior.get("total_v9_direct_reproduction_mismatch_count", 0)) == 0,
            "prior schema-2 report contains direct-bit mismatches; refinement is forbidden",
        )
        require(
            int(prior.get("total_v9_interpolation_failure_count", 0)) == 0,
            "prior schema-2 report contains interpolation implementation failures",
        )
        require(
            all(
                item.get("recomputed_direct_matches_v9_exact_bits") is True
                for item in prior_v9_regressions
            ),
            "prior schema-2 report contains a direct-bit reproduction mismatch",
        )
        require(
            not any(
                special
                in {
                    str(component)
                    for component in item.get("failed_components", [])
                }
                for item in prior_v9_regressions
                for special in ("solver_failure", "interpolation_failure")
            ),
            "prior schema-2 v9 regression contains a source/interpolation failure",
        )
    else:
        raise RuntimeError("prior audit report has an unsupported schema")
    require(
        prior.get("cross_cell_design_check_passed") is False,
        "prior audit report is not a failed refinement stage",
    )
    declared_parent = protocol["parent_v9_failure"]
    if initial_hashes["parent_failure_report_sha256"] != declared_parent["report_sha256"]:
        raise RuntimeError("parent v9 failure report hash differs from protocol")
    if initial_hashes["parent_nodes_sha256"] != declared_parent["nodes_sha256"]:
        raise RuntimeError("parent v9 node-request hash differs from protocol")
    require(
        parent.get("node_request_sha256") == declared_parent["nodes_sha256"],
        "parent v9 report does not bind the declared node request",
    )
    if parent.get("interpolation_check_passed") is not False:
        raise RuntimeError("parent report is not a failed held-out report")
    environment = json.loads(environment_path.read_text(encoding="utf-8"))
    if (
        environment.get("tool_file_sha256", {}).get("generate_lut.py")
        != initial_hashes["generator_source_sha256"]
    ):
        raise RuntimeError("environment does not describe current generator")
    require(
        protocol.get("generator_source_sha256")
        == initial_hashes["generator_source_sha256"],
        "protocol generator hash differs from current source",
    )
    require(
        protocol.get("environment_report_sha256")
        == initial_hashes["environment_report_sha256"],
        "protocol environment hash differs from current report",
    )
    require(
        parent.get("generator_source_sha256")
        == initial_hashes["generator_source_sha256"],
        "parent v9 report used a different generator",
    )
    require(
        parent.get("environment_report_sha256")
        == initial_hashes["environment_report_sha256"],
        "parent v9 report used a different environment",
    )
    require(
        parent.get("validation_source_sha256")
        == protocol["parent_v9_failure"]["validation_source_sha256"],
        "parent v9 report used an unexpected validation source",
    )
    generator = load_generator(tool_root)
    audit = protocol["cross_cell_audit"]
    seed = str(audit["selection_seed"])
    worker_limit = int(audit["worker_limit"])
    relative_budgets = {
        str(key): float(value) for key, value in audit["relative_budgets"].items()
    }
    absolute_floor = float(audit["absolute_floor"])
    expected_budgets = {
        name: (0.2 if name.startswith("fall_speed_") else 0.15)
        for name in COMPONENT_NAMES
    }
    require(
        relative_budgets == expected_budgets,
        "v10 design budgets must remain 15 percent EM / 20 percent fall moment",
    )
    require(worker_limit == 12, "v10 cross-cell worker limit must equal 12")
    require(absolute_floor == 1e-12, "v10 absolute error floor must equal 1e-12")
    require(
        tuple(float(value) for value in audit["base_within_cell_fractions"])
        == FRACTIONS,
        "protocol within-cell fractions differ from the implementation",
    )
    require(
        int(audit["sample_count_per_cross_cell"]) == len(FRACTIONS),
        "protocol sample count per cross-cell differs from the implementation",
    )
    maximum_depth = int(protocol["recursive_diameter_rule"]["maximum_depth_relative_to_v9_parent"])
    require(maximum_depth == 3, "v10 maximum recursive diameter depth must equal 3")

    parent_failed_by_directory = {}
    for table in parent["tables"]:
        failed = [
            node
            for node in table["nodes"]
            if not node["within_predeclared_interpolation_thresholds"]
        ]
        if failed:
            parent_failed_by_directory[table["asset_directory"]] = failed
    declared_directories = {
        item["asset_directory"] for item in protocol["initial_refinements"]
    }
    if set(parent_failed_by_directory) != declared_directories:
        raise RuntimeError("protocol table set differs from parent v9 failures")
    parent_tables = {
        str(table["asset_directory"]): table for table in parent["tables"]
    }
    for refinement in protocol["initial_refinements"]:
        directory = str(refinement["asset_directory"])
        require(directory in parent_tables, f"parent v9 report omits {directory}")
        require(
            parent_tables[directory].get("config_sha256")
            == refinement["v9_config_sha256"],
            f"parent v9 config hash differs for {directory}",
        )

    stage_tables = {
        str(table["asset_directory"]): table for table in stage["tables"]
    }
    require(
        set(stage_tables) == declared_directories,
        "stage manifest table set differs from the v10 refinement set",
    )
    legacy_initial_refinements = {
        str(item["asset_directory"]): item
        for item in legacy_protocol_document["initial_refinements"]
    }
    legacy_depth1_tables = {
        str(table["asset_directory"]): table for table in legacy_depth1["tables"]
    }
    require(
        set(legacy_initial_refinements) == declared_directories
        and set(legacy_depth1_tables) == declared_directories,
        "legacy protocol/report table sets differ from v10",
    )
    for refinement in protocol["initial_refinements"]:
        directory = str(refinement["asset_directory"])
        stable_salt = str(refinement["initial_v10_config_sha256"])
        require(
            stable_salt
            == legacy_initial_refinements[directory]["initial_v10_config_sha256"]
            == legacy_depth1_tables[directory]["config_sha256"],
            f"{directory}: stable permutation salt differs from depth-1 config evidence",
        )
    projection_hashes = {
        str(key): str(value)
        for key, value in amendment["non_diameter_config_projection_sha256"].items()
    }
    require(
        set(projection_hashes) == declared_directories,
        "protocol amendment projection table set differs from v10",
    )
    prior_sha256 = initial_hashes["prior_audit_report_sha256"]
    prior_regressions = load_prior_regressions(prior, prior_sha256)
    prior_tables = {
        str(table["asset_directory"]): table for table in prior["tables"]
    }
    require(
        set(prior_tables) == declared_directories,
        "prior audit table set differs from the v10 refinement set",
    )
    require(
        sum(len(items) for items in prior_regressions.values())
        == int(stage["expected_accumulated_prior_failure_count"]),
        "prior failing-coordinate count differs from the frozen stage manifest",
    )

    table_reports = []
    config_hashes: dict[Path, str] = {}
    all_passed = True
    total_cross_cells = 0
    total_samples = 0
    total_unique_points = 0
    total_prior_regressions = 0
    total_solver_failures = 0
    total_v9_direct_reproduction_mismatches = 0
    total_v9_interpolation_failures = 0
    stage_operation = str(stage["operation"])
    require(
        stage_operation == "diameter_bisection",
        "this audit runner accepts diameter-bisection stages only",
    )
    for refinement in protocol["initial_refinements"]:
        directory_name = refinement["asset_directory"]
        stage_table = stage_tables[directory_name]
        config_path = asset_root / directory_name / "config.json"
        config_bytes, _, config = generator.parse_exact_json(config_path)
        generator.validate_config(config)
        config_sha256 = hashlib.sha256(config_bytes).hexdigest()
        require(
            config_sha256 == stage_table["config_sha256"],
            f"{directory_name}: live candidate differs from frozen stage config",
        )
        config_hashes[config_path] = config_sha256
        axes = list(config["axes"])
        axis_kinds = tuple(str(axis["kind"]) for axis in axes)
        axes_by_kind = {str(axis["kind"]): axis for axis in axes}
        diameter_axis = axes_by_kind["equivolume_diameter"]
        diameter_coordinates = [
            float(value) for value in diameter_axis["coordinates"]
        ]
        parent_interval = tuple(
            float(value) for value in refinement["parent_diameter_interval"]
        )
        if len(parent_interval) != 2:
            raise RuntimeError("parent diameter interval must have two endpoints")
        projection_sha256 = non_diameter_config_projection_sha256(
            config, parent_interval
        )
        require(
            projection_sha256 == projection_hashes[directory_name]
            and projection_sha256
            == stage_table["non_diameter_config_projection_sha256"],
            f"{directory_name}: a config field outside the controlled diameter slab changed",
        )
        if parent_interval[0] not in diameter_coordinates or parent_interval[1] not in diameter_coordinates:
            raise RuntimeError("candidate omitted a parent diameter endpoint")
        diameter_leaves = [
            (lower, upper)
            for lower, upper in zip(diameter_coordinates, diameter_coordinates[1:])
            if lower >= parent_interval[0] and upper <= parent_interval[1]
        ]
        if not diameter_leaves:
            raise RuntimeError("candidate contains no diameter leaves in parent interval")
        if diameter_leaves[0][0] != parent_interval[0] or diameter_leaves[-1][1] != parent_interval[1]:
            raise RuntimeError("diameter leaves do not tile the parent interval")
        if any(
            left[1] != right[0]
            for left, right in zip(diameter_leaves, diameter_leaves[1:])
        ):
            raise RuntimeError("diameter leaves contain a gap")
        declared_leaves = [
            (float(item["lower"]), float(item["upper"]), int(item["depth"]))
            for item in stage_table["diameter_leaves"]
        ]
        actual_leaves = [
            (lower, upper, leaf_depth(parent_interval, (lower, upper)))
            for lower, upper in diameter_leaves
        ]
        require(
            actual_leaves == declared_leaves,
            f"{directory_name}: diameter leaves differ from frozen stage manifest",
        )
        prior_table = prior_tables[directory_name]
        prior_leaf_bounds = [
            (float(item["lower"]), float(item["upper"]))
            for item in prior_table["diameter_leaves"]
        ]
        prior_failing_bounds = {
            (float(item["lower"]), float(item["upper"]))
            for item in prior_table["failing_diameter_leaves"]
        }
        split_parent_bounds = {
            (float(item["lower"]), float(item["upper"]))
            for item in stage_table["split_parent_leaves"]
        }
        if prior_failing_bounds:
            require(
                split_parent_bounds == prior_failing_bounds,
                f"{directory_name}: diameter stage must split every and only failed parent leaf",
            )
        else:
            require(
                not split_parent_bounds,
                f"{directory_name}: passing parent table must remain unchanged",
            )
        expected_boundaries = {value for leaf in prior_leaf_bounds for value in leaf}
        expected_boundaries.update(
            lower + (upper - lower) / 2.0
            for lower, upper in split_parent_bounds
        )
        require(
            set(value for leaf in diameter_leaves for value in leaf)
            == expected_boundaries,
            f"{directory_name}: stage mutation is not exactly the declared failing-leaf bisection",
        )
        require(
            max(depth for _, _, depth in actual_leaves) <= maximum_depth,
            f"{directory_name}: candidate exceeds maximum diameter depth",
        )
        other_axes = [
            axis
            for axis in axes
            if axis["kind"] != "equivolume_diameter"
            and len(axis["coordinates"]) > 1
        ]
        other_intervals = [
            [
                (index, float(lower), float(upper))
                for index, (lower, upper) in enumerate(
                    zip(axis["coordinates"], axis["coordinates"][1:])
                )
            ]
            for axis in other_axes
        ]
        other_combo_count = math.prod(len(items) for items in other_intervals)
        require(
            other_combo_count == int(stage_table["other_axis_interval_combo_count"]),
            f"{directory_name}: other-axis combination count changed",
        )
        cross_cell_count = len(diameter_leaves) * other_combo_count
        expected_sample_count = cross_cell_count * len(FRACTIONS)
        require(
            cross_cell_count == int(stage_table["cross_cell_count"])
            and expected_sample_count == int(stage_table["cross_cell_sample_count"]),
            f"{directory_name}: cross-cell/sample count differs from stage manifest",
        )

        target_grid_coordinates: dict[str, list[float]] = {}
        for axis in axes:
            kind = str(axis["kind"])
            coordinates = [float(value) for value in axis["coordinates"]]
            if kind == "equivolume_diameter":
                coordinates = [
                    value
                    for value in coordinates
                    if parent_interval[0] <= value <= parent_interval[1]
                ]
            target_grid_coordinates[kind] = coordinates
        grid_points = [
            dict(zip(axis_kinds, values))
            for values in itertools.product(
                *(target_grid_coordinates[kind] for kind in axis_kinds)
            )
        ]

        sample_points: list[dict[str, float]] = []
        sample_metadata: list[dict[str, Any]] = []
        for diameter_leaf_index, diameter_leaf in enumerate(diameter_leaves):
            for other_choice in itertools.product(*other_intervals):
                intervals = {
                    "equivolume_diameter": (
                        diameter_leaf_index,
                        diameter_leaf[0],
                        diameter_leaf[1],
                    )
                }
                for axis, interval in zip(other_axes, other_choice):
                    intervals[str(axis["kind"])] = interval
                for sample_index in range(len(FRACTIONS)):
                    point = {}
                    selected_fractions = {}
                    for axis in axes:
                        kind = str(axis["kind"])
                        coordinates = [float(value) for value in axis["coordinates"]]
                        if len(coordinates) == 1:
                            point[kind] = coordinates[0]
                            continue
                        interval_index, lower, upper = intervals[kind]
                        permutation = interval_permutation(
                            seed,
                            str(refinement["initial_v10_config_sha256"]),
                            str(config["table_id"]),
                            kind,
                            lower,
                            upper,
                        )
                        fraction = permutation[sample_index]
                        point[kind] = lower + fraction * (upper - lower)
                        selected_fractions[kind] = fraction
                    sample_points.append(point)
                    sample_metadata.append(
                        {
                            "sample_index_within_cross_cell": sample_index,
                            "diameter_leaf_index": diameter_leaf_index,
                            "diameter_leaf": list(diameter_leaf),
                            "diameter_leaf_depth": leaf_depth(
                                parent_interval, diameter_leaf
                            ),
                            "other_axis_interval_indices": {
                                str(axis["kind"]): interval[0]
                                for axis, interval in zip(other_axes, other_choice)
                            },
                            "selected_fractions": selected_fractions,
                        }
                    )
        if len(sample_points) != expected_sample_count:
            raise RuntimeError("internal cross-cell sample count mismatch")

        regression_nodes = parent_failed_by_directory[directory_name]
        regression_points = [
            {str(key): float(value) for key, value in node["coordinates"].items()}
            for node in regression_nodes
        ]
        accumulated_regression_inputs = prior_regressions.get(directory_name, [])
        accumulated_regression_points = [
            item["coordinates"] for item in accumulated_regression_inputs
        ]
        require(
            len(accumulated_regression_points)
            == int(stage_table["accumulated_prior_failure_count"]),
            f"{directory_name}: accumulated prior failure count changed",
        )
        combined_by_key: dict[tuple[float, ...], dict[str, float]] = {}
        for point in itertools.chain(
            grid_points,
            sample_points,
            regression_points,
            accumulated_regression_points,
        ):
            combined_by_key.setdefault(point_key(point, axis_kinds), point)
        combined_points = list(combined_by_key.values())
        combined_values, solver_failures = evaluate_points(
            generator, config, combined_points, worker_limit
        )
        value_by_key = {
            point_key(point, axis_kinds): values
            for point, values in zip(combined_points, combined_values)
            if values is not None
        }
        point_kind_by_key: dict[tuple[float, ...], set[str]] = defaultdict(set)
        for kind, points in (
            ("grid_corner", grid_points),
            ("cross_cell_sample", sample_points),
            ("v9_regression", regression_points),
            ("accumulated_cross_cell_regression", accumulated_regression_points),
        ):
            for point in points:
                point_kind_by_key[point_key(point, axis_kinds)].add(kind)
        solver_failure_reports = []
        for failure in solver_failures:
            point = combined_points[int(failure["point_index"])]
            key = point_key(point, axis_kinds)
            solver_failure_reports.append(
                {
                    "coordinates": point,
                    "point_kinds": sorted(point_kind_by_key[key]),
                    "error": failure["error"],
                }
            )
        total_solver_failures += len(solver_failure_reports)
        grid_values = {
            point_key(point, axis_kinds): value_by_key[point_key(point, axis_kinds)]
            for point in grid_points
            if point_key(point, axis_kinds) in value_by_key
        }

        component_maxima: dict[str, dict[str, Any]] = {}
        failing_samples = []
        failing_leaves: set[tuple[float, float]] = set()
        table_worst_ratio = 0.0
        grid_solver_blocked = any(
            "grid_corner" in failure["point_kinds"]
            for failure in solver_failure_reports
        )
        if grid_solver_blocked:
            failing_leaves.update(diameter_leaves)
        for point, metadata in zip(sample_points, sample_metadata):
            key = point_key(point, axis_kinds)
            if grid_solver_blocked or key not in value_by_key:
                leaf = tuple(float(value) for value in metadata["diameter_leaf"])
                failing_leaves.add(leaf)
                failing_samples.append(
                    {
                        **metadata,
                        "coordinates": point,
                        "failed_components": ["solver_failure"],
                        "worst_ratio_to_design_budget": None,
                        "within_design_budget": False,
                    }
                )
                continue
            direct = value_by_key[key]
            try:
                interpolated = interpolate(axes, grid_values, point)
            except BaseException as error:
                leaf = tuple(float(value) for value in metadata["diameter_leaf"])
                failing_leaves.add(leaf)
                failing_samples.append(
                    {
                        **metadata,
                        "coordinates": point,
                        "failed_components": ["interpolation_failure"],
                        "error": f"{type(error).__name__}: {error}",
                        "worst_ratio_to_design_budget": None,
                        "within_design_budget": False,
                    }
                )
                continue
            error_records, sample_passed, worst_ratio = errors(
                direct, interpolated, relative_budgets, absolute_floor
            )
            table_worst_ratio = max(table_worst_ratio, worst_ratio)
            for record, expected, actual in zip(
                error_records, direct, interpolated
            ):
                name = record["component"]
                previous = component_maxima.get(name)
                if (
                    previous is None
                    or record["relative_error_with_absolute_floor"]
                    > previous["relative_error_with_absolute_floor"]
                ):
                    component_maxima[name] = {
                        **record,
                        "coordinates": point,
                        "direct": float(expected),
                        "interpolated": float(actual),
                        "diameter_leaf": metadata["diameter_leaf"],
                    }
            if not sample_passed:
                leaf = tuple(float(value) for value in metadata["diameter_leaf"])
                failing_leaves.add(leaf)
                failing_samples.append(
                    compact_failure(point, metadata, error_records, worst_ratio)
                )

        regression_reports = []
        regressions_passed = True
        v9_direct_reproduction_mismatch_count = 0
        v9_interpolation_failure_count = 0
        for parent_node, point in zip(regression_nodes, regression_points):
            key = point_key(point, axis_kinds)
            diameter_value = point["equivolume_diameter"]
            containing_leaf = next(
                leaf
                for leaf in diameter_leaves
                if leaf[0] < diameter_value < leaf[1]
            )
            if grid_solver_blocked or key not in value_by_key:
                regressions_passed = False
                regression_reports.append(
                    {
                        "parent_node_index": int(parent_node["node_index"]),
                        "coordinates": point,
                        "recomputed_direct_matches_v9_exact_bits": False,
                        "failed_components": ["solver_failure"],
                        "containing_diameter_leaf": list(containing_leaf),
                        "interpolation_within_design_budget": False,
                        "within_design_budget": False,
                    }
                )
                continue
            direct = value_by_key[key]
            parent_direct = [
                float(parent_node["direct_pytmatrix"][name])
                for name in COMPONENT_NAMES
            ]
            exact_reproduction = all(
                current.hex() == previous.hex()
                for current, previous in zip(direct, parent_direct)
            )
            if not exact_reproduction:
                v9_direct_reproduction_mismatch_count += 1
            try:
                interpolated = interpolate(axes, grid_values, point)
            except BaseException as error:
                v9_interpolation_failure_count += 1
                regressions_passed = False
                regression_reports.append(
                    {
                        "parent_node_index": int(parent_node["node_index"]),
                        "coordinates": point,
                        "recomputed_direct_matches_v9_exact_bits": exact_reproduction,
                        "direct": dict(zip(COMPONENT_NAMES, direct)),
                        "failed_components": ["interpolation_failure"],
                        "error": f"{type(error).__name__}: {error}",
                        "containing_diameter_leaf": list(containing_leaf),
                        "interpolation_within_design_budget": False,
                        "within_design_budget": False,
                    }
                )
                continue
            error_records, interpolation_passed, worst_ratio = errors(
                direct, interpolated, relative_budgets, absolute_floor
            )
            regression_passed = interpolation_passed and exact_reproduction
            table_worst_ratio = max(table_worst_ratio, worst_ratio)
            regressions_passed = regressions_passed and regression_passed
            if not interpolation_passed:
                failing_leaves.add(containing_leaf)
            regression_reports.append(
                {
                    "parent_node_index": int(parent_node["node_index"]),
                    "coordinates": point,
                    "recomputed_direct_matches_v9_exact_bits": exact_reproduction,
                    "direct": dict(zip(COMPONENT_NAMES, direct)),
                    "interpolated": dict(zip(COMPONENT_NAMES, interpolated)),
                    "errors": error_records,
                    "containing_diameter_leaf": list(containing_leaf),
                    "interpolation_within_design_budget": interpolation_passed,
                    "within_design_budget": regression_passed,
                }
            )

        accumulated_regression_reports = []
        accumulated_regressions_passed = True
        for inherited, point in zip(
            accumulated_regression_inputs, accumulated_regression_points
        ):
            key = point_key(point, axis_kinds)
            diameter_value = point["equivolume_diameter"]
            containing_leaves = containing_leaves_for_value(
                diameter_leaves, diameter_value
            )
            record: dict[str, Any] = {
                "coordinates": point,
                "first_failure_report_sha256": inherited[
                    "first_failure_report_sha256"
                ],
                "containing_diameter_leaves": [
                    list(leaf) for leaf in containing_leaves
                ],
                "diameter_is_exact_candidate_knot": len(containing_leaves) > 1,
            }
            if grid_solver_blocked or key not in value_by_key:
                passed = False
                record.update(
                    {
                        "failed_components": ["solver_failure"],
                        "worst_ratio_to_design_budget": None,
                        "within_design_budget": False,
                    }
                )
            else:
                direct = value_by_key[key]
                try:
                    interpolated = interpolate(axes, grid_values, point)
                    error_records, passed, worst_ratio = errors(
                        direct, interpolated, relative_budgets, absolute_floor
                    )
                    table_worst_ratio = max(table_worst_ratio, worst_ratio)
                    record.update(
                        {
                            "failed_components": failed_components(error_records),
                            "worst_ratio_to_design_budget": worst_ratio,
                            "within_design_budget": passed,
                        }
                    )
                except BaseException as error:
                    passed = False
                    record.update(
                        {
                            "failed_components": ["interpolation_failure"],
                            "error": f"{type(error).__name__}: {error}",
                            "worst_ratio_to_design_budget": None,
                            "within_design_budget": False,
                        }
                    )
            if not passed:
                failing_leaves.update(containing_leaves)
            accumulated_regressions_passed = accumulated_regressions_passed and passed
            accumulated_regression_reports.append(record)

        table_passed = (
            not failing_samples
            and regressions_passed
            and accumulated_regressions_passed
            and not solver_failure_reports
        )
        all_passed = all_passed and table_passed
        total_cross_cells += cross_cell_count
        total_samples += len(sample_points)
        total_unique_points += len(combined_points)
        total_prior_regressions += len(accumulated_regression_reports)
        total_v9_direct_reproduction_mismatches += (
            v9_direct_reproduction_mismatch_count
        )
        total_v9_interpolation_failures += v9_interpolation_failure_count
        implicated_intervals: dict[str, set[tuple[int, float, float]]] = {
            str(axis["kind"]): set() for axis in other_axes
        }
        failing_coordinates = [
            item["coordinates"] for item in failing_samples
        ] + [
            item["coordinates"]
            for item in accumulated_regression_reports
            if not item["within_design_budget"]
        ]
        for point in failing_coordinates:
            for axis in other_axes:
                kind = str(axis["kind"])
                implicated_intervals[kind].update(
                    intervals_for_value(axis["coordinates"], float(point[kind]))
                )
        maximum_observed_depth = max(depth for _, _, depth in actual_leaves)
        if solver_failure_reports:
            next_action = "resolve_solver_failures_without_refining_the_grid"
        elif v9_direct_reproduction_mismatch_count:
            next_action = (
                "resolve_exact_reproduction_or_environment_mismatch_without_refining_grid"
            )
        elif v9_interpolation_failure_count:
            next_action = "resolve_interpolation_implementation_failure_without_refining_grid"
        elif table_passed:
            next_action = "freeze_table_grid_for_solver_convergence"
        elif maximum_observed_depth < maximum_depth:
            next_action = "bisect_only_reported_failing_diameter_leaves"
        else:
            next_action = "invoke_predeclared_non_diameter_fallback"
        table_reports.append(
            {
                "asset_directory": directory_name,
                "table_id": config["table_id"],
                "config_sha256": config_sha256,
                "non_diameter_config_projection_sha256": projection_sha256,
                "stable_permutation_salt": refinement[
                    "initial_v10_config_sha256"
                ],
                "parent_v9_config_sha256": refinement["v9_config_sha256"],
                "parent_diameter_interval": list(parent_interval),
                "diameter_leaves": [
                    {
                        "lower": lower,
                        "upper": upper,
                        "depth": leaf_depth(parent_interval, (lower, upper)),
                    }
                    for lower, upper in diameter_leaves
                ],
                "other_nonsingleton_axes": [
                    str(axis["kind"]) for axis in other_axes
                ],
                "other_axis_interval_combo_count": other_combo_count,
                "cross_cell_count": cross_cell_count,
                "cross_cell_sample_count": len(sample_points),
                "candidate_grid_corner_point_count": len(grid_points),
                "evaluated_unique_point_count": len(combined_points),
                "component_maxima": component_maxima,
                "worst_ratio_to_design_budget": table_worst_ratio,
                "failing_cross_cell_sample_count": len(failing_samples),
                "failing_cross_cell_samples": failing_samples,
                "v9_development_regressions": regression_reports,
                "v9_direct_reproduction_mismatch_count": v9_direct_reproduction_mismatch_count,
                "v9_interpolation_failure_count": v9_interpolation_failure_count,
                "accumulated_cross_cell_development_regression_count": len(
                    accumulated_regression_reports
                ),
                "accumulated_cross_cell_development_regressions": accumulated_regression_reports,
                "solver_failure_count": len(solver_failure_reports),
                "solver_failures": solver_failure_reports,
                "failing_diameter_leaves": [
                    {
                        "lower": leaf[0],
                        "upper": leaf[1],
                        "depth": leaf_depth(parent_interval, leaf),
                    }
                    for leaf in sorted(failing_leaves)
                ],
                "maximum_observed_diameter_depth": maximum_observed_depth,
                "maximum_allowed_diameter_depth": maximum_depth,
                "implicated_non_diameter_intervals": {
                    kind: [
                        {"index": index, "lower": lower, "upper": upper}
                        for index, lower, upper in sorted(intervals)
                    ]
                    for kind, intervals in implicated_intervals.items()
                },
                "next_action": next_action,
                "cross_cell_design_check_passed": table_passed,
            }
        )

    final_hashes = {
        "audit_source_sha256": sha256_file(source_path),
        "generator_source_sha256": sha256_file(generator_path),
        "environment_report_sha256": sha256_file(environment_path),
        "protocol_sha256": sha256_file(protocol_path),
        "protocol_amendment_sha256": sha256_file(amendment_path),
        "legacy_protocol_sha256": sha256_file(legacy_protocol_path),
        "legacy_depth1_report_sha256": sha256_file(legacy_depth1_path),
        "stage_manifest_sha256": sha256_file(stage_path),
        "parent_failure_report_sha256": sha256_file(parent_path),
        "parent_nodes_sha256": sha256_file(parent_nodes_path),
        "prior_audit_report_sha256": sha256_file(prior_path),
    }
    if final_hashes != initial_hashes:
        raise RuntimeError("source, environment, protocol, or parent report changed")
    for path, initial_sha256 in config_hashes.items():
        if sha256_file(path) != initial_sha256:
            raise RuntimeError(f"config changed during audit: {path}")

    require(
        total_cross_cells == int(stage["expected_total_cross_cell_count"]),
        "evaluated cross-cell count differs from stage manifest",
    )
    require(
        total_samples == int(stage["expected_total_cross_cell_sample_count"]),
        "evaluated sample count differs from stage manifest",
    )
    require(
        total_prior_regressions
        == int(stage["expected_accumulated_prior_failure_count"]),
        "evaluated accumulated-regression count differs from stage manifest",
    )
    if total_solver_failures:
        overall_next_action = "resolve_solver_failures_without_refining_the_grid"
    elif total_v9_direct_reproduction_mismatches:
        overall_next_action = (
            "resolve_exact_reproduction_or_environment_mismatch_without_refining_grid"
        )
    elif total_v9_interpolation_failures:
        overall_next_action = (
            "resolve_interpolation_implementation_failure_without_refining_grid"
        )
    elif all_passed:
        overall_next_action = "freeze_grid_and_begin_solver_convergence"
    elif max(
        table["maximum_observed_diameter_depth"] for table in table_reports
    ) < maximum_depth:
        overall_next_action = "bisect_only_reported_failing_diameter_leaves"
    else:
        overall_next_action = "invoke_predeclared_non_diameter_fallback"
    report = {
        "schema": 2,
        "report_id": "pytmatrix-0.3.3-refined-v10-cross-cell-design-audit-v2",
        "stage_id": stage["stage_id"],
        "classification": "grid_design_development_audit_not_validation",
        "scientifically_independent": False,
        "production_validation": False,
        "cross_cell_design_check_passed": all_passed,
        "selection_seed": seed,
        "selection_protocol": audit["permutation_rule"],
        "stable_permutation_salt_protocol": audit[
            "stable_permutation_salt_protocol"
        ],
        "base_within_cell_fractions": list(FRACTIONS),
        "relative_budgets": relative_budgets,
        "absolute_floor": absolute_floor,
        "worker_limit": worker_limit,
        "effective_worker_count": worker_limit,
        "total_cross_cell_count": total_cross_cells,
        "total_cross_cell_sample_count": total_samples,
        "total_accumulated_cross_cell_development_regression_count": total_prior_regressions,
        "total_solver_failure_count": total_solver_failures,
        "total_v9_direct_reproduction_mismatch_count": total_v9_direct_reproduction_mismatches,
        "total_v9_interpolation_failure_count": total_v9_interpolation_failures,
        "total_evaluated_unique_point_count": total_unique_points,
        "maximum_allowed_diameter_depth": maximum_depth,
        "next_action": overall_next_action,
        **initial_hashes,
        "tables": table_reports,
    }
    write_json(output_path, report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
