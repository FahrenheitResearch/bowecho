#!/usr/bin/env python3
"""Create fail-closed refined-v10 property solver-convergence evidence.

The two property configurations changed by the frozen v10 grid-design protocol
are recomputed at every Cartesian grid node with PyTMatrix ndgs=12 and ndgs=14.
The three unchanged property-table sections are copied exactly from the frozen,
passing v9 convergence report after all source, environment, configuration, and
report hashes have been verified.  The resulting document is deliberately a
truthful composite report, not independent scattering validation.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import hashlib
import importlib.util
import json
import math
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


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

RECOMPUTED_DIRECTORIES = (
    "property_p3_ishmael_dry_oblate_sband_unvalidated",
    "property_rain_sband_unvalidated",
)
REUSED_DIRECTORIES = (
    "property_p3_ishmael_dry_prolate_sband_unvalidated",
    "property_p3_ishmael_wet_oblate_sband_unvalidated",
    "property_p3_ishmael_wet_prolate_sband_unvalidated",
)
PROPERTY_DIRECTORIES = RECOMPUTED_DIRECTORIES + REUSED_DIRECTORIES
GRID_DESIGN_DIRECTORIES = (
    "conventional_wet_hail_sband_unvalidated",
    *RECOMPUTED_DIRECTORIES,
)
EXPECTED_CONFIG_DIRECTORIES = frozenset(
    (*GRID_DESIGN_DIRECTORIES, *REUSED_DIRECTORIES)
)

SOLVER_NDGS = (12, 14)
RELATIVE_TOLERANCE = 1.0e-3
ABSOLUTE_TOLERANCE = 1.0e-12
WORKER_LIMIT = 12
MAX_RECORDED_FAILURES = 100

FROZEN_V9_REPORT_SHA256 = (
    "352e259f02b606ac71579b7d3b4591c3088b41f8e74becd949a27e5721030067"
)
FROZEN_OCI_INDEX_DIGEST = (
    "sha256:8fa858ff203a6d4148d8e8fbea8ac556694216ebfaf5cbc5913f9e84362a135d"
)
FROZEN_LINUX_AMD64_RUNTIME_IMAGE_CONFIG_DIGEST = (
    "sha256:5798717aaf97894751513c11e177394d5dc0592e21ea48baae42186d9f97ac9e"
)

SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
IMAGE_DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")


class ConvergenceError(RuntimeError):
    """Raised when a frozen input or convergence invariant is violated."""


@dataclass(frozen=True)
class InputSnapshot:
    path: Path
    data: bytes
    sha256: str


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def canonical_json_sha256(value: Any) -> str:
    encoded = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode("utf-8")
    return sha256_bytes(encoded)


def sha256_argument(value: str) -> str:
    if SHA256_RE.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("expected a lowercase 64-character SHA-256")
    return value


def image_digest_argument(value: str) -> str:
    if IMAGE_DIGEST_RE.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("expected sha256:<64 lowercase hex characters>")
    return value


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ConvergenceError(message)


def require_mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConvergenceError(f"{label} must be a JSON object")
    return value


def reject_duplicate_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ConvergenceError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def reject_nonfinite_constant(value: str) -> None:
    raise ConvergenceError(f"non-finite JSON number {value!r} is forbidden")


def parse_snapshot_json(snapshot: InputSnapshot, label: str) -> dict[str, Any]:
    try:
        text = snapshot.data.decode("utf-8", errors="strict")
        value = json.loads(
            text,
            object_pairs_hook=reject_duplicate_pairs,
            parse_constant=reject_nonfinite_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ConvergenceError) as error:
        raise ConvergenceError(f"{label} is not strict JSON: {error}") from error
    return require_mapping(value, label)


def take_snapshot(path: Path, expected_sha256: str, label: str) -> InputSnapshot:
    resolved = path.resolve()
    if not resolved.is_file():
        raise ConvergenceError(f"{label} does not exist as a file: {resolved}")
    data = resolved.read_bytes()
    actual = sha256_bytes(data)
    if actual != expected_sha256:
        raise ConvergenceError(
            f"{label} SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        )
    return InputSnapshot(resolved, data, actual)


def recheck_snapshots(snapshots: Iterable[InputSnapshot]) -> None:
    for snapshot in snapshots:
        try:
            current = snapshot.path.read_bytes()
        except OSError as error:
            raise ConvergenceError(
                f"input disappeared while convergence ran: {snapshot.path}: {error}"
            ) from error
        if current != snapshot.data:
            raise ConvergenceError(
                f"input changed while convergence ran: {snapshot.path}"
            )


def write_json_atomic_no_overwrite(path: Path, value: Any) -> None:
    """Publish complete JSON atomically while refusing to replace any file."""
    encoded = (
        json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n"
    ).encode("utf-8")
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        raise ConvergenceError(f"refusing to overwrite existing output: {path}")

    temporary = path.with_name(
        f".{path.name}.{os.getpid()}.{os.urandom(8).hex()}.tmp"
    )
    descriptor = -1
    linked = False
    try:
        descriptor = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        with os.fdopen(descriptor, "wb", closefd=True) as stream:
            descriptor = -1
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        # A same-directory hard link is an atomic create-if-absent operation.
        # Unlike os.replace(), it cannot silently overwrite a raced output.
        os.link(temporary, path)
        linked = True
        try:
            directory_descriptor = os.open(path.parent, os.O_RDONLY)
        except OSError:
            directory_descriptor = -1
        if directory_descriptor >= 0:
            try:
                try:
                    os.fsync(directory_descriptor)
                except OSError:
                    # Some Docker Desktop bind filesystems reject directory
                    # fsync even though the complete file and atomic link are
                    # already durable at the file layer.
                    pass
            finally:
                os.close(directory_descriptor)
    except FileExistsError as error:
        raise ConvergenceError(f"refusing to overwrite existing output: {path}") from error
    except OSError as error:
        raise ConvergenceError(f"failed to atomically publish {path}: {error}") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        if not linked and path.exists():
            # os.link either creates the complete destination or creates nothing.
            # This branch only protects against an unexpected platform behavior.
            raise ConvergenceError(f"output publication did not complete safely: {path}")


def load_generator(tool_root: Path) -> Any:
    generator_path = tool_root / "generate_lut.py"
    spec = importlib.util.spec_from_file_location(
        "brslut_generate_lut_refined_v10", generator_path
    )
    if spec is None or spec.loader is None:
        raise ConvergenceError(f"cannot import {generator_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def parse_expected_configs(values: Sequence[str]) -> dict[str, str]:
    result: dict[str, str] = {}
    for value in values:
        directory, separator, digest = value.partition("=")
        if not separator or not directory or SHA256_RE.fullmatch(digest) is None:
            raise ConvergenceError(
                "--expected-config-sha256 must be DIRECTORY=<lowercase SHA-256>"
            )
        if directory in result:
            raise ConvergenceError(
                f"duplicate --expected-config-sha256 for {directory}"
            )
        result[directory] = digest
    if set(result) != EXPECTED_CONFIG_DIRECTORIES:
        missing = sorted(EXPECTED_CONFIG_DIRECTORIES - set(result))
        extra = sorted(set(result) - EXPECTED_CONFIG_DIRECTORIES)
        raise ConvergenceError(
            "expected config hashes must cover exactly the frozen v10 evidence set; "
            f"missing={missing}, extra={extra}"
        )
    return result


def unique_table_map(report: Mapping[str, Any], label: str) -> dict[str, dict[str, Any]]:
    tables = report.get("tables")
    if not isinstance(tables, list):
        raise ConvergenceError(f"{label}.tables must be an array")
    result: dict[str, dict[str, Any]] = {}
    for index, raw_table in enumerate(tables):
        table = require_mapping(raw_table, f"{label}.tables[{index}]")
        directory = table.get("asset_directory")
        if not isinstance(directory, str) or not directory:
            raise ConvergenceError(
                f"{label}.tables[{index}].asset_directory must be a string"
            )
        if directory in result:
            raise ConvergenceError(f"{label} repeats table {directory}")
        result[directory] = table
    return result


def validate_thread_environment(environment: Mapping[str, Any]) -> None:
    thread_environment = require_mapping(
        environment.get("thread_environment"), "environment.thread_environment"
    )
    for key, expected_value in thread_environment.items():
        require(
            isinstance(expected_value, str),
            f"environment.thread_environment.{key} must be a string",
        )
        require(
            os.environ.get(key) == expected_value,
            f"process environment {key} must equal {expected_value!r}",
        )


def validate_environment_runtime(
    environment: Mapping[str, Any],
) -> list[InputSnapshot]:
    require(
        environment.get("artifact_classification")
        == "reproducibility_environment_not_scientific_validation",
        "environment report has an unexpected classification",
    )
    require(environment.get("target") == "x86_64", "environment target is not x86_64")
    runtime_files = (
        ("python_executable", "python_executable_sha256"),
        ("numpy_core_extension", "numpy_core_extension_sha256"),
        ("pytmatrix_fortran_extension", "pytmatrix_fortran_extension_sha256"),
    )
    runtime_snapshots: list[InputSnapshot] = []
    for path_key, digest_key in runtime_files:
        runtime_path_value = environment.get(path_key)
        expected_digest = environment.get(digest_key)
        require(
            isinstance(runtime_path_value, str) and runtime_path_value,
            f"environment.{path_key} is missing",
        )
        require(
            isinstance(expected_digest, str)
            and SHA256_RE.fullmatch(expected_digest) is not None,
            f"environment.{digest_key} is not a SHA-256",
        )
        runtime_snapshots.append(
            take_snapshot(
                Path(runtime_path_value),
                expected_digest,
                f"environment runtime {path_key}",
            )
        )
    validate_thread_environment(environment)
    return runtime_snapshots


def validate_lineage(
    lineage: Mapping[str, Any],
    *,
    expected_environment_sha256: str,
    expected_generator_sha256: str,
    expected_oci_index_digest: str,
    expected_runtime_image_config_digest: str,
) -> None:
    execution = require_mapping(lineage.get("execution"), "lineage.execution")
    require(
        execution.get("frozen_attested_image_index_id")
        == expected_oci_index_digest,
        "reproduction lineage OCI index digest mismatch",
    )
    require(
        execution.get("stable_linux_amd64_runtime_image_id")
        == expected_runtime_image_config_digest,
        "reproduction lineage linux/amd64 runtime image-config digest mismatch",
    )
    require(
        execution.get("environment_report_sha256") == expected_environment_sha256,
        "reproduction lineage environment hash mismatch",
    )
    require(
        execution.get("generator_source_sha256") == expected_generator_sha256,
        "reproduction lineage generator hash mismatch",
    )


def validate_protocol(protocol: Mapping[str, Any]) -> None:
    require(protocol.get("schema") == 2, "v10 protocol schema must equal 2")
    require(
        protocol.get("protocol_id")
        == "pytmatrix-0.3.3-refined-grid-v10-cross-cell-design-v2",
        "v10 protocol id is not the frozen schema-2 protocol",
    )
    require(
        protocol.get("classification")
        == "predeclared_grid_design_protocol_not_validation",
        "v10 protocol has an unexpected classification",
    )
    require(protocol.get("scientifically_independent") is False, "protocol independence flag changed")
    require(protocol.get("production_validation") is False, "protocol production flag changed")
    solver_rule = require_mapping(
        protocol.get("solver_freeze_rule"), "protocol.solver_freeze_rule"
    )
    require(
        solver_rule.get("recompute") == list(RECOMPUTED_DIRECTORIES),
        "protocol recompute set changed",
    )
    require(
        solver_rule.get("reuse_by_exact_hash_and_report_reference")
        == list(REUSED_DIRECTORIES),
        "protocol reuse set changed",
    )
    require(
        solver_rule.get("reused_v9_report_sha256") == FROZEN_V9_REPORT_SHA256,
        "protocol v9 convergence reference changed",
    )
    require(
        solver_rule.get("comparison_solver_ndgs") == list(SOLVER_NDGS),
        "protocol solver comparison changed",
    )
    require(
        solver_rule.get("require_truthful_composite_report") is True,
        "protocol no longer requires a truthful composite report",
    )


def validate_stage_manifest(
    stage: Mapping[str, Any],
    expected_configs: Mapping[str, str],
    *,
    expected_protocol_sha256: str,
    expected_audit_source_sha256: str,
    expected_generator_sha256: str,
    expected_environment_sha256: str,
    expected_output_filename: str,
) -> dict[str, dict[str, Any]]:
    require(stage.get("schema") == 1, "v10 stage-manifest schema must equal 1")
    stage_classification = stage.get("classification")
    require(
        isinstance(stage_classification, str)
        and stage_classification.startswith("frozen_")
        and stage_classification.endswith("grid_design_stage_not_validation"),
        "v10 stage manifest has an unexpected classification",
    )
    require(
        stage.get("protocol_sha256") == expected_protocol_sha256,
        "stage manifest protocol hash mismatch",
    )
    require(
        stage.get("audit_source_sha256") == expected_audit_source_sha256,
        "stage manifest audit-source hash mismatch",
    )
    require(
        stage.get("generator_source_sha256") == expected_generator_sha256,
        "stage manifest generator hash mismatch",
    )
    require(
        stage.get("environment_report_sha256") == expected_environment_sha256,
        "stage manifest environment hash mismatch",
    )
    require(
        stage.get("expected_output_filename") == expected_output_filename,
        "stage manifest names a different final audit output",
    )
    prior_name = stage.get("prior_audit_report_file")
    prior_sha256 = stage.get("prior_audit_report_sha256")
    require(
        isinstance(prior_name, str) and prior_name and Path(prior_name).name == prior_name,
        "stage manifest prior-audit filename is invalid",
    )
    require(
        isinstance(prior_sha256, str) and SHA256_RE.fullmatch(prior_sha256) is not None,
        "stage manifest prior-audit SHA-256 is invalid",
    )

    tables = unique_table_map(stage, "v10 stage manifest")
    require(
        set(tables) == set(GRID_DESIGN_DIRECTORIES),
        "stage-manifest table set is not the frozen v10 refinement set",
    )
    for directory, table in tables.items():
        require(
            table.get("config_sha256") == expected_configs[directory],
            f"stage-manifest config hash mismatch for {directory}",
        )
    return tables


def validate_grid_design_report(
    report: Mapping[str, Any],
    protocol: Mapping[str, Any],
    stage: Mapping[str, Any],
    stage_tables: Mapping[str, Mapping[str, Any]],
    expected_configs: Mapping[str, str],
    *,
    expected_generator_sha256: str,
    expected_environment_sha256: str,
    expected_audit_source_sha256: str,
    expected_protocol_sha256: str,
    expected_stage_sha256: str,
    expected_prior_audit_sha256: str,
) -> dict[str, dict[str, Any]]:
    require(report.get("schema") == 2, "grid-design report schema must equal 2")
    require(
        report.get("report_id")
        == "pytmatrix-0.3.3-refined-v10-cross-cell-design-audit-v2",
        "grid-design report id is not the frozen schema-2 audit",
    )
    require(
        report.get("classification") == "grid_design_development_audit_not_validation",
        "grid-design report has an unexpected classification",
    )
    require(report.get("scientifically_independent") is False, "grid-design independence flag changed")
    require(report.get("production_validation") is False, "grid-design production flag changed")
    require(
        report.get("cross_cell_design_check_passed") is True,
        "final v10 grid-design report did not pass",
    )
    require(
        report.get("next_action") == "freeze_grid_and_begin_solver_convergence",
        "final v10 grid-design report is not ready for solver convergence",
    )
    require(
        int(report.get("total_solver_failure_count", -1)) == 0,
        "final v10 grid-design report records solver failures",
    )
    require(
        report.get("generator_source_sha256") == expected_generator_sha256,
        "grid-design report generator hash mismatch",
    )
    require(
        report.get("environment_report_sha256") == expected_environment_sha256,
        "grid-design report environment hash mismatch",
    )
    require(
        report.get("audit_source_sha256") == expected_audit_source_sha256,
        "grid-design report audit-source hash mismatch",
    )
    require(
        report.get("protocol_sha256") == expected_protocol_sha256,
        "grid-design report protocol hash mismatch",
    )
    require(
        report.get("stage_manifest_sha256") == expected_stage_sha256,
        "grid-design report stage-manifest hash mismatch",
    )
    require(
        report.get("stage_id") == stage.get("stage_id"),
        "grid-design report stage id mismatch",
    )
    require(
        report.get("prior_audit_report_sha256") == expected_prior_audit_sha256,
        "grid-design report prior-audit hash mismatch",
    )
    parent = require_mapping(
        protocol.get("parent_v9_failure"), "protocol.parent_v9_failure"
    )
    legacy = require_mapping(protocol.get("legacy_protocol"), "protocol.legacy_protocol")
    require(
        report.get("parent_failure_report_sha256") == parent.get("report_sha256"),
        "grid-design report parent-failure hash mismatch",
    )
    require(
        report.get("parent_nodes_sha256") == parent.get("nodes_sha256"),
        "grid-design report parent-node hash mismatch",
    )
    require(
        report.get("legacy_protocol_sha256") == legacy.get("sha256"),
        "grid-design report legacy-protocol hash mismatch",
    )

    tables = unique_table_map(report, "grid-design report")
    require(
        set(tables) == set(GRID_DESIGN_DIRECTORIES),
        "grid-design report table set is not the frozen v10 refinement set",
    )
    for directory in GRID_DESIGN_DIRECTORIES:
        table = tables[directory]
        require(
            table.get("config_sha256") == expected_configs[directory],
            f"grid-design report config hash mismatch for {directory}",
        )
        require(
            table.get("cross_cell_design_check_passed") is True,
            f"grid-design table did not pass: {directory}",
        )
        require(
            table.get("next_action") == "freeze_table_grid_for_solver_convergence",
            f"grid-design table is not ready to freeze: {directory}",
        )
        require(
            int(table.get("failing_cross_cell_sample_count", -1)) == 0,
            f"grid-design table retains failing samples: {directory}",
        )
        require(
            table.get("failing_cross_cell_samples") == [],
            f"grid-design table retains recorded failures: {directory}",
        )
        require(
            table.get("failing_diameter_leaves") == [],
            f"grid-design table retains failing diameter leaves: {directory}",
        )
        require(
            int(table.get("solver_failure_count", -1)) == 0,
            f"grid-design table records solver failures: {directory}",
        )
        require(
            table.get("solver_failures") == [],
            f"grid-design table retains solver failure records: {directory}",
        )
        regressions = table.get("v9_development_regressions")
        require(
            isinstance(regressions, list) and regressions,
            f"grid-design table omits v9 development regressions: {directory}",
        )
        for regression_index, raw_regression in enumerate(regressions):
            regression = require_mapping(
                raw_regression,
                f"grid-design report {directory} v9 regression {regression_index}",
            )
            require(
                regression.get("recomputed_direct_matches_v9_exact_bits") is True,
                f"v9 direct regression did not reproduce exact bits: {directory}",
            )
            require(
                regression.get("within_design_budget") is True,
                f"v9 development regression did not pass: {directory}",
            )

        accumulated = table.get("accumulated_cross_cell_development_regressions")
        require(
            isinstance(accumulated, list),
            f"grid-design table accumulated regressions are not an array: {directory}",
        )
        require(
            int(table.get("accumulated_cross_cell_development_regression_count", -1))
            == len(accumulated),
            f"grid-design table accumulated-regression count mismatch: {directory}",
        )
        for regression_index, raw_regression in enumerate(accumulated):
            regression = require_mapping(
                raw_regression,
                f"grid-design report {directory} accumulated regression {regression_index}",
            )
            require(
                regression.get("within_design_budget") is True,
                f"accumulated cross-cell regression did not pass: {directory}",
            )

        stage_table = stage_tables[directory]
        for report_key, stage_key in (
            ("cross_cell_count", "cross_cell_count"),
            ("cross_cell_sample_count", "cross_cell_sample_count"),
            (
                "accumulated_cross_cell_development_regression_count",
                "accumulated_prior_failure_count",
            ),
        ):
            require(
                int(table.get(report_key, -1)) == int(stage_table.get(stage_key, -2)),
                f"grid-design table differs from stage manifest for {directory}: {report_key}",
            )

    for total_key, table_key in (
        ("total_cross_cell_count", "cross_cell_count"),
        ("total_cross_cell_sample_count", "cross_cell_sample_count"),
        ("total_evaluated_unique_point_count", "evaluated_unique_point_count"),
    ):
        expected_total = sum(int(table[table_key]) for table in tables.values())
        require(
            int(report.get(total_key, -1)) == expected_total,
            f"grid-design report {total_key} is internally inconsistent",
        )
    accumulated_total = sum(
        int(table["accumulated_cross_cell_development_regression_count"])
        for table in tables.values()
    )
    require(
        int(
            report.get(
                "total_accumulated_cross_cell_development_regression_count", -1
            )
        )
        == accumulated_total
        == int(stage.get("expected_accumulated_prior_failure_count", -2)),
        "grid-design accumulated-regression total is internally inconsistent",
    )
    return tables


def validate_v9_report(
    report: Mapping[str, Any],
    *,
    expected_generator_sha256: str,
    expected_environment_sha256: str,
) -> dict[str, dict[str, Any]]:
    require(report.get("solver_convergence_check_passed") is True, "v9 convergence report did not pass")
    require(report.get("scientifically_independent") is False, "v9 independence flag changed")
    require(report.get("production_validation") is False, "v9 production flag changed")
    require(
        report.get("generator_source_sha256") == expected_generator_sha256,
        "v9 convergence generator hash mismatch",
    )
    require(
        report.get("environment_report_sha256") == expected_environment_sha256,
        "v9 convergence environment hash mismatch",
    )
    require(report.get("comparison_solver_ndgs") == list(SOLVER_NDGS), "v9 solver comparison changed")
    require(report.get("selected_final_solver_ndgs") == SOLVER_NDGS[1], "v9 final ndgs changed")
    require(report.get("selected_final_solver_ddelt") == 0.001, "v9 final ddelt changed")
    require(report.get("relative_tolerance") == RELATIVE_TOLERANCE, "v9 relative tolerance changed")
    require(report.get("absolute_tolerance") == ABSOLUTE_TOLERANCE, "v9 absolute tolerance changed")
    require(report.get("worker_limit") == WORKER_LIMIT, "v9 worker limit changed")

    tables = unique_table_map(report, "v9 convergence report")
    require(set(tables) == set(PROPERTY_DIRECTORIES), "v9 convergence table set changed")
    total_points = 0
    total_comparisons = 0
    for directory, table in tables.items():
        points = int(table.get("grid_point_count", -1))
        compared = int(table.get("compared_grid_point_count", -1))
        comparisons = int(table.get("component_comparison_count", -1))
        require(points >= 1 and compared == points, f"v9 table was not fully compared: {directory}")
        require(comparisons == points * len(COMPONENT_NAMES), f"v9 comparison count mismatch: {directory}")
        require(table.get("within_predeclared_solver_convergence_tolerance") is True, f"v9 table did not pass: {directory}")
        require(int(table.get("solver_group_failure_count", -1)) == 0, f"v9 table has solver failures: {directory}")
        require(int(table.get("component_tolerance_failure_count", -1)) == 0, f"v9 table has tolerance failures: {directory}")
        require(table.get("solver_group_failures") == [], f"v9 table records solver failures: {directory}")
        require(table.get("recorded_component_tolerance_failures") == [], f"v9 table records tolerance failures: {directory}")
        require(table.get("configured_solver", {}).get("ndgs") == SOLVER_NDGS[1], f"v9 table ndgs mismatch: {directory}")
        require(table.get("configured_solver", {}).get("ddelt") == 0.001, f"v9 table ddelt mismatch: {directory}")
        total_points += points
        total_comparisons += comparisons
    require(int(report.get("total_grid_point_count", -1)) == total_points, "v9 total grid count mismatch")
    require(int(report.get("total_component_comparison_count", -1)) == total_comparisons, "v9 total comparison count mismatch")
    return tables


def validate_group_output(
    output: Any,
    point_count: int,
    label: str,
) -> list[list[float]]:
    if not isinstance(output, list) or len(output) != point_count:
        raise ConvergenceError(
            f"{label} returned {len(output) if isinstance(output, list) else 'non-list'} "
            f"rows for {point_count} points"
        )
    normalized: list[list[float]] = []
    for point_index, raw_components in enumerate(output):
        if not isinstance(raw_components, list) or len(raw_components) != len(COMPONENT_NAMES):
            raise ConvergenceError(
                f"{label} point {point_index} has an invalid component count"
            )
        components = [float(value) for value in raw_components]
        if any(not math.isfinite(value) for value in components):
            raise ConvergenceError(
                f"{label} point {point_index} contains a non-finite component"
            )
        normalized.append(components)
    return normalized


def recompute_table(
    generator: Any,
    directory: str,
    config_bytes: bytes,
    config: dict[str, Any],
) -> dict[str, Any]:
    coordinates = list(generator.point_coordinates(config))
    grouping = require_mapping(config["execution"].get("grouping"), f"{directory}.execution.grouping")
    material_axes_value = grouping.get("material_state_axis_kinds")
    require(
        isinstance(material_axes_value, list)
        and material_axes_value
        and all(isinstance(value, str) for value in material_axes_value),
        f"{directory}: invalid material-state grouping",
    )
    material_axes = tuple(material_axes_value)
    grouped: dict[tuple[float, ...], list[tuple[int, dict[str, float]]]] = {}
    for flat_index, point in enumerate(coordinates):
        key = tuple(float(point[kind]) for kind in material_axes)
        grouped.setdefault(key, []).append((flat_index, point))
    group_items = list(grouped.items())
    require(group_items, f"{directory}: configuration has no grid points")
    group_results: list[dict[str, Any] | None] = [None] * len(group_items)

    def evaluate_group(
        indexed_group: tuple[
            int,
            tuple[tuple[float, ...], list[tuple[int, dict[str, float]]]],
        ],
    ) -> tuple[int, dict[str, Any]]:
        group_index, (material_key, entries) = indexed_group
        points = [point for _, point in entries]
        timeout = int(grouping["group_timeout_seconds"])
        try:
            lower = generator.run_isolated_solver_ndgs_comparison_group(
                config, points, SOLVER_NDGS[0], timeout
            )
            upper = generator.run_isolated_solver_ndgs_comparison_group(
                config, points, SOLVER_NDGS[1], timeout
            )
            return group_index, {
                "material_key": material_key,
                "entries": entries,
                "lower": validate_group_output(
                    lower, len(points), f"{directory} ndgs={SOLVER_NDGS[0]} group {group_index}"
                ),
                "upper": validate_group_output(
                    upper, len(points), f"{directory} ndgs={SOLVER_NDGS[1]} group {group_index}"
                ),
            }
        except BaseException as error:  # Match the frozen v9 failure-capture policy.
            return group_index, {
                "material_key": material_key,
                "entries": entries,
                "failure": f"{type(error).__name__}: {error}",
            }

    completed_groups = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=WORKER_LIMIT) as executor:
        futures = [
            executor.submit(evaluate_group, item) for item in enumerate(group_items)
        ]
        for future in concurrent.futures.as_completed(futures):
            group_index, result = future.result()
            require(
                group_results[group_index] is None,
                f"{directory}: convergence executor returned group {group_index} twice",
            )
            group_results[group_index] = result
            completed_groups += 1
            print(
                f"refined-v10 solver convergence {directory}: "
                f"{completed_groups}/{len(group_items)} material groups",
                flush=True,
            )

    worst_by_component: list[dict[str, Any] | None] = [None] * len(COMPONENT_NAMES)
    failure_count = 0
    recorded_failures: list[dict[str, Any]] = []
    solver_group_failures: list[dict[str, Any]] = []
    table_passed = True
    compared_points = 0

    for group_index, result_value in enumerate(group_results):
        if result_value is None:
            raise ConvergenceError(
                f"{directory}: convergence executor omitted group {group_index}"
            )
        result = result_value
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

        entries = result["entries"]
        lower_rows = result["lower"]
        upper_rows = result["upper"]
        require(
            len(entries) == len(lower_rows) == len(upper_rows),
            f"{directory}: group {group_index} result length mismatch",
        )
        for (flat_index, point), lower, upper in zip(
            entries, lower_rows, upper_rows
        ):
            compared_points += 1
            for component_index, (name, lower_value, upper_value) in enumerate(
                zip(COMPONENT_NAMES, lower, upper)
            ):
                absolute_difference = abs(upper_value - lower_value)
                scale = max(abs(lower_value), abs(upper_value))
                if component_index in (2, 3):
                    scale = max(
                        math.hypot(lower[2], lower[3]),
                        math.hypot(upper[2], upper[3]),
                    )
                allowed = ABSOLUTE_TOLERANCE + RELATIVE_TOLERANCE * scale
                agreement_ratio = absolute_difference / allowed
                within = agreement_ratio <= 1.0
                record = {
                    "component": name,
                    "flat_grid_index": flat_index,
                    "coordinates": point,
                    "lower_ndgs": SOLVER_NDGS[0],
                    "upper_ndgs": SOLVER_NDGS[1],
                    "lower_value": lower_value,
                    "upper_value": upper_value,
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
                    if len(recorded_failures) < MAX_RECORDED_FAILURES:
                        recorded_failures.append(record)

    grid_points = len(coordinates)
    if compared_points != grid_points:
        table_passed = False
    component_comparisons = compared_points * len(COMPONENT_NAMES)
    return {
        "table_id": config["table_id"],
        "asset_directory": directory,
        "config_sha256": sha256_bytes(config_bytes),
        "configured_solver": copy.deepcopy(config["radar"]["solver"]),
        "grid_point_count": grid_points,
        "compared_grid_point_count": compared_points,
        "material_group_count": len(group_items),
        "worker_limit": WORKER_LIMIT,
        "effective_worker_count": min(WORKER_LIMIT, len(group_items)),
        "component_comparison_count": component_comparisons,
        "solver_group_failure_count": len(solver_group_failures),
        "solver_group_failures": solver_group_failures,
        "component_tolerance_failure_count": failure_count,
        "recorded_component_tolerance_failures": recorded_failures,
        "recorded_failure_limit": MAX_RECORDED_FAILURES,
        "worst_comparison_by_component": worst_by_component,
        "within_predeclared_solver_convergence_tolerance": table_passed,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-root", type=Path, required=True)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--reproduction-lineage", type=Path, required=True)
    parser.add_argument("--protocol", type=Path, required=True)
    parser.add_argument("--stage-manifest", type=Path, required=True)
    parser.add_argument(
        "--cross-cell-audit-report",
        "--grid-design-report",
        dest="grid_design_report",
        type=Path,
        required=True,
    )
    parser.add_argument(
        "--cross-cell-audit-source",
        "--grid-design-source",
        dest="grid_design_source",
        type=Path,
        required=True,
    )
    parser.add_argument("--v9-convergence-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-validation-source-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-generator-source-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-environment-report-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-reproduction-lineage-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-protocol-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-stage-manifest-sha256", type=sha256_argument, required=True)
    parser.add_argument(
        "--expected-cross-cell-audit-report-sha256",
        "--expected-grid-design-report-sha256",
        dest="expected_grid_design_report_sha256",
        type=sha256_argument,
        required=True,
    )
    parser.add_argument(
        "--expected-cross-cell-audit-source-sha256",
        "--expected-grid-design-source-sha256",
        dest="expected_grid_design_source_sha256",
        type=sha256_argument,
        required=True,
    )
    parser.add_argument("--expected-v9-convergence-report-sha256", type=sha256_argument, required=True)
    parser.add_argument("--expected-oci-index-digest", type=image_digest_argument, required=True)
    parser.add_argument("--expected-runtime-image-config-digest", type=image_digest_argument, required=True)
    parser.add_argument(
        "--expected-config-sha256",
        action="append",
        default=[],
        metavar="DIRECTORY=SHA256",
        help="repeat exactly once for the six frozen config inputs",
    )
    return parser


def run(args: argparse.Namespace) -> bool:
    output = args.output.resolve()
    if output.exists():
        raise ConvergenceError(f"refusing to overwrite existing output: {output}")
    require(
        args.expected_v9_convergence_report_sha256 == FROZEN_V9_REPORT_SHA256,
        "the CLI v9 report hash is not the frozen v9 convergence report",
    )
    require(
        args.expected_oci_index_digest == FROZEN_OCI_INDEX_DIGEST,
        "the CLI OCI index digest is not the frozen attested index",
    )
    require(
        args.expected_runtime_image_config_digest
        == FROZEN_LINUX_AMD64_RUNTIME_IMAGE_CONFIG_DIGEST,
        "the CLI runtime image digest is not the frozen linux/amd64 image config",
    )
    expected_configs = parse_expected_configs(args.expected_config_sha256)

    tool_root = args.tool_root.resolve()
    asset_root = args.asset_root.resolve()
    generator_path = tool_root / "generate_lut.py"
    snapshots: list[InputSnapshot] = []

    def snapshot(path: Path, digest: str, label: str) -> InputSnapshot:
        result = take_snapshot(path, digest, label)
        snapshots.append(result)
        return result

    validation_snapshot = snapshot(
        Path(__file__),
        args.expected_validation_source_sha256,
        "validation source",
    )
    generator_snapshot = snapshot(
        generator_path,
        args.expected_generator_source_sha256,
        "generator source",
    )
    environment_snapshot = snapshot(
        args.environment_report,
        args.expected_environment_report_sha256,
        "environment report",
    )
    lineage_snapshot = snapshot(
        args.reproduction_lineage,
        args.expected_reproduction_lineage_sha256,
        "reproduction lineage",
    )
    protocol_snapshot = snapshot(
        args.protocol,
        args.expected_protocol_sha256,
        "v10 protocol",
    )
    stage_snapshot = snapshot(
        args.stage_manifest,
        args.expected_stage_manifest_sha256,
        "v10 stage manifest",
    )
    grid_design_snapshot = snapshot(
        args.grid_design_report,
        args.expected_grid_design_report_sha256,
        "final v10 grid-design report",
    )
    grid_design_source_snapshot = snapshot(
        args.grid_design_source,
        args.expected_grid_design_source_sha256,
        "grid-design audit source",
    )
    v9_snapshot = snapshot(
        args.v9_convergence_report,
        args.expected_v9_convergence_report_sha256,
        "v9 convergence report",
    )

    config_snapshots: dict[str, InputSnapshot] = {}
    for directory in sorted(EXPECTED_CONFIG_DIRECTORIES):
        config_snapshots[directory] = snapshot(
            asset_root / directory / "config.json",
            expected_configs[directory],
            f"config {directory}",
        )

    input_paths = {item.path for item in snapshots}
    require(output not in input_paths, "output aliases a frozen input")

    environment = parse_snapshot_json(environment_snapshot, "environment report")
    lineage = parse_snapshot_json(lineage_snapshot, "reproduction lineage")
    protocol = parse_snapshot_json(protocol_snapshot, "v10 protocol")
    stage = parse_snapshot_json(stage_snapshot, "v10 stage manifest")
    grid_design = parse_snapshot_json(grid_design_snapshot, "final v10 grid-design report")
    v9_report = parse_snapshot_json(v9_snapshot, "v9 convergence report")

    prior_audit_name = stage.get("prior_audit_report_file")
    prior_audit_sha256 = stage.get("prior_audit_report_sha256")
    require(
        isinstance(prior_audit_name, str)
        and prior_audit_name
        and Path(prior_audit_name).name == prior_audit_name,
        "stage manifest prior-audit filename is invalid",
    )
    require(
        isinstance(prior_audit_sha256, str)
        and SHA256_RE.fullmatch(prior_audit_sha256) is not None,
        "stage manifest prior-audit SHA-256 is invalid",
    )
    prior_audit_snapshot = snapshot(
        stage_snapshot.path.parent / prior_audit_name,
        prior_audit_sha256,
        "prior cross-cell audit report",
    )
    parse_snapshot_json(prior_audit_snapshot, "prior cross-cell audit report")

    snapshots.extend(validate_environment_runtime(environment))
    require(
        output not in {item.path for item in snapshots},
        "output aliases a frozen runtime input",
    )
    require(
        environment.get("tool_file_sha256", {}).get("generate_lut.py")
        == generator_snapshot.sha256,
        "environment report does not describe the frozen generator",
    )
    require(
        environment.get("container_image_id") == args.expected_oci_index_digest,
        "environment report OCI index identity mismatch",
    )
    validate_lineage(
        lineage,
        expected_environment_sha256=environment_snapshot.sha256,
        expected_generator_sha256=generator_snapshot.sha256,
        expected_oci_index_digest=args.expected_oci_index_digest,
        expected_runtime_image_config_digest=args.expected_runtime_image_config_digest,
    )
    validate_protocol(protocol)
    require(
        protocol.get("audit_source_sha256") == grid_design_source_snapshot.sha256,
        "v10 protocol audit-source hash mismatch",
    )
    require(
        protocol.get("generator_source_sha256") == generator_snapshot.sha256,
        "v10 protocol generator hash mismatch",
    )
    require(
        protocol.get("environment_report_sha256") == environment_snapshot.sha256,
        "v10 protocol environment hash mismatch",
    )
    stage_tables = validate_stage_manifest(
        stage,
        expected_configs,
        expected_protocol_sha256=protocol_snapshot.sha256,
        expected_audit_source_sha256=grid_design_source_snapshot.sha256,
        expected_generator_sha256=generator_snapshot.sha256,
        expected_environment_sha256=environment_snapshot.sha256,
        expected_output_filename=grid_design_snapshot.path.name,
    )
    grid_design_tables = validate_grid_design_report(
        grid_design,
        protocol,
        stage,
        stage_tables,
        expected_configs,
        expected_generator_sha256=generator_snapshot.sha256,
        expected_environment_sha256=environment_snapshot.sha256,
        expected_audit_source_sha256=grid_design_source_snapshot.sha256,
        expected_protocol_sha256=protocol_snapshot.sha256,
        expected_stage_sha256=stage_snapshot.sha256,
        expected_prior_audit_sha256=prior_audit_snapshot.sha256,
    )
    v9_tables = validate_v9_report(
        v9_report,
        expected_generator_sha256=generator_snapshot.sha256,
        expected_environment_sha256=environment_snapshot.sha256,
    )

    generator = load_generator(tool_root)
    configs: dict[str, tuple[bytes, dict[str, Any]]] = {}
    for directory, config_snapshot in config_snapshots.items():
        config_bytes, _, config = generator.parse_exact_json(config_snapshot.path)
        require(
            config_bytes == config_snapshot.data,
            f"config changed between snapshot and parse: {directory}",
        )
        generator.validate_config(config)
        if directory in PROPERTY_DIRECTORIES:
            require(
                int(config["radar"]["solver"]["ndgs"]) == SOLVER_NDGS[1],
                f"{directory}: configured final ndgs must equal {SOLVER_NDGS[1]}",
            )
            require(
                float(config["radar"]["solver"]["ddelt"]) == 0.001,
                f"{directory}: configured final ddelt must equal 0.001",
            )
        configs[directory] = (config_bytes, config)

    for directory in RECOMPUTED_DIRECTORIES:
        require(
            grid_design_tables[directory]["config_sha256"]
            == expected_configs[directory],
            f"successful grid-design report is not frozen to {directory}",
        )
    for directory in REUSED_DIRECTORIES:
        require(
            v9_tables[directory]["config_sha256"] == expected_configs[directory],
            f"unchanged config no longer matches its exact v9 section: {directory}",
        )
        _, config = configs[directory]
        point_count = sum(1 for _ in generator.point_coordinates(config))
        require(
            point_count == int(v9_tables[directory]["grid_point_count"]),
            f"unchanged config point count differs from its v9 section: {directory}",
        )

    recomputed_tables: dict[str, dict[str, Any]] = {}
    for directory in RECOMPUTED_DIRECTORIES:
        config_bytes, config = configs[directory]
        recomputed_tables[directory] = recompute_table(
            generator, directory, config_bytes, config
        )

    table_reports: list[dict[str, Any]] = []
    evidence_sources: list[dict[str, Any]] = []
    for directory in PROPERTY_DIRECTORIES:
        if directory in recomputed_tables:
            table = recomputed_tables[directory]
            table_reports.append(table)
            evidence_sources.append(
                {
                    "asset_directory": directory,
                    "evidence_operation": "recomputed_all_grid_nodes_ndgs12_vs_ndgs14",
                    "source_report_sha256": None,
                    "source_table_section_canonical_sha256": None,
                    "config_sha256": expected_configs[directory],
                }
            )
        else:
            # Do not annotate or otherwise mutate this section: exact v9 table-section
            # reuse is what the frozen protocol permits.
            reused = copy.deepcopy(v9_tables[directory])
            require(
                reused == v9_tables[directory],
                f"failed to preserve exact v9 table section: {directory}",
            )
            table_reports.append(reused)
            evidence_sources.append(
                {
                    "asset_directory": directory,
                    "evidence_operation": "exact_v9_table_section_reuse",
                    "source_report_sha256": v9_snapshot.sha256,
                    "source_table_section_canonical_sha256": canonical_json_sha256(reused),
                    "config_sha256": expected_configs[directory],
                }
            )

    all_passed = all(
        table.get("within_predeclared_solver_convergence_tolerance") is True
        for table in table_reports
    )
    total_grid_points = sum(int(table["grid_point_count"]) for table in table_reports)
    total_component_comparisons = sum(
        int(table["component_comparison_count"]) for table in table_reports
    )
    report = {
        "schema": 1,
        "report_id": "pytmatrix-0.3.3-property-refined-v10-composite-ndgs12-to14-v1",
        "classification": "native_solver_resolution_convergence_composite_check_only",
        "crate_table_validation_status_after_report": "research_only_unvalidated",
        "scientifically_independent": False,
        "production_validation": False,
        "solver_convergence_check_passed": all_passed,
        "comparison_solver_ndgs": list(SOLVER_NDGS),
        "selected_final_solver_ndgs": SOLVER_NDGS[1],
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
        "relative_tolerance": RELATIVE_TOLERANCE,
        "absolute_tolerance": ABSOLUTE_TOLERANCE,
        "scope": (
            "Every Cartesian grid point in the two v10-changed property configs "
            "was freshly recomputed. Exact passing v9 table sections are reused only "
            "for the three property configs whose bytes and hashes did not change."
        ),
        "worker_limit": WORKER_LIMIT,
        "effective_worker_count": WORKER_LIMIT,
        "recomputed_asset_directories": list(RECOMPUTED_DIRECTORIES),
        "reused_asset_directories": list(REUSED_DIRECTORIES),
        "table_evidence_sources": evidence_sources,
        "total_grid_point_count": total_grid_points,
        "total_component_comparison_count": total_component_comparisons,
        "validation_source_sha256": validation_snapshot.sha256,
        "generator_source_sha256": generator_snapshot.sha256,
        "environment_report_sha256": environment_snapshot.sha256,
        "reproduction_lineage_file": lineage_snapshot.path.name,
        "reproduction_lineage_sha256": lineage_snapshot.sha256,
        "frozen_attested_oci_index_digest": args.expected_oci_index_digest,
        "linux_amd64_runtime_image_config_digest": args.expected_runtime_image_config_digest,
        "v10_protocol_file": protocol_snapshot.path.name,
        "v10_protocol_sha256": protocol_snapshot.sha256,
        "v10_stage_manifest_file": stage_snapshot.path.name,
        "v10_stage_manifest_sha256": stage_snapshot.sha256,
        "v10_stage_id": stage["stage_id"],
        "prior_cross_cell_audit_report_file": prior_audit_snapshot.path.name,
        "prior_cross_cell_audit_report_sha256": prior_audit_snapshot.sha256,
        "final_grid_design_report_file": grid_design_snapshot.path.name,
        "final_grid_design_report_sha256": grid_design_snapshot.sha256,
        "grid_design_audit_source_sha256": grid_design_source_snapshot.sha256,
        "grid_design_cross_cell_sample_count": grid_design[
            "total_cross_cell_sample_count"
        ],
        "reused_v9_convergence_report_file": v9_snapshot.path.name,
        "reused_v9_convergence_report_sha256": v9_snapshot.sha256,
        "config_sha256": dict(sorted(expected_configs.items())),
        "composite_truthfulness": (
            "The table_evidence_sources array distinguishes fresh v10 computations "
            "from exact v9 section reuse. The report does not characterize reused "
            "tables as freshly recomputed."
        ),
        "independence_limit": (
            "Both compared solver resolutions use the same PyTMatrix implementation. "
            "This is numerical shape-integration convergence evidence, not independent "
            "scattering validation. Reused v9 sections have the same limitation."
        ),
        "tables": table_reports,
    }

    recheck_snapshots(snapshots)
    validate_thread_environment(environment)
    if output.exists():
        raise ConvergenceError(f"refusing to overwrite raced output: {output}")
    write_json_atomic_no_overwrite(output, report)
    return all_passed


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        passed = run(args)
    except ConvergenceError as error:
        parser.exit(2, f"refined-v10 composite convergence failed closed: {error}\n")
    if not passed:
        print(
            "refined-v10 composite convergence report was retained with failures",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
