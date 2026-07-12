#!/usr/bin/env python3
"""Generate one exact-frequency five-role research T-matrix pack.

This wrapper retargets the locked property-table configs to exactly one of the
declared 2.8, 5.6, or 9.4 GHz research frequencies and delegates every LUT
node to generate_lut.py. It never interpolates frequency and always emits an
``unvalidated_research`` pack; scientific validation is a separate process.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Sequence

import generate_lut


PACK_SCHEMA = 1
PACK_VALIDATION_STATUS = "unvalidated_research"
PACK_ID_PATTERN = re.compile(r"^[A-Za-z0-9._-]{1,128}$")
DIGEST_PATTERN = re.compile(r"^[0-9a-f]{64}$")


@dataclass(frozen=True)
class RoleSpec:
    role: str
    directory: str
    template_directory: str


ROLE_SPECS = (
    RoleSpec(
        "dry_oblate",
        "dry_oblate",
        "property_p3_ishmael_dry_oblate_sband_unvalidated",
    ),
    RoleSpec(
        "dry_prolate",
        "dry_prolate",
        "property_p3_ishmael_dry_prolate_sband_unvalidated",
    ),
    RoleSpec(
        "wet_oblate",
        "wet_oblate",
        "property_p3_ishmael_wet_oblate_sband_unvalidated",
    ),
    RoleSpec(
        "wet_prolate",
        "wet_prolate",
        "property_p3_ishmael_wet_prolate_sband_unvalidated",
    ),
    RoleSpec(
        "rain_standalone_and_residual",
        "rain_standalone_and_residual",
        "property_rain_sband_unvalidated",
    ),
)
ROLE_NAMES = tuple(spec.role for spec in ROLE_SPECS)


class PackGeneratorError(RuntimeError):
    """A pack/config/provenance/generation invariant failed."""


def canonical_json_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=True,
        allow_nan=False,
    ).encode("utf-8")


def sha256_json(value: Any) -> str:
    return hashlib.sha256(canonical_json_bytes(value)).hexdigest()


def exact_band_frequency_hz(band: str) -> float:
    try:
        return generate_lut.EXACT_PROPERTY_BAND_FREQUENCIES_HZ[band]
    except KeyError as error:
        raise PackGeneratorError(
            f"band must be one of s, c, or x; got {band!r}"
        ) from error


def default_pack_id(band: str) -> str:
    exact_band_frequency_hz(band)
    return f"property-tmatrix-{band}band-pytmatrix-0.3.3-unvalidated-v1"


def _validate_pack_id(pack_id: str) -> None:
    if not PACK_ID_PATTERN.fullmatch(pack_id):
        raise PackGeneratorError(
            "pack_id must be 1..128 ASCII letters, digits, '.', '_', or '-'"
        )


def _validate_science_revision(science_revision: str) -> None:
    if (
        not science_revision
        or science_revision != science_revision.strip()
        or len(science_revision.encode("utf-8")) > 128
    ):
        raise PackGeneratorError(
            "science_revision must be nonempty trimmed UTF-8 of at most 128 bytes"
        )


def _validate_relative_path(value: str) -> None:
    path = PurePosixPath(value)
    if (
        not value
        or "\\" in value
        or ":" in value
        or value.startswith("/")
        or value.endswith("/")
        or "//" in value
        or path.is_absolute()
        or any(part in ("", ".", "..") for part in value.split("/"))
    ):
        raise PackGeneratorError(f"unsafe pack-relative path {value!r}")


def retarget_role_config(template: Mapping[str, Any], band: str) -> dict[str, Any]:
    """Return one deep-copied config at exactly the requested band frequency."""
    frequency_hz = exact_band_frequency_hz(band)
    config = copy.deepcopy(dict(template))
    table_id = config.get("table_id")
    if not isinstance(table_id, str) or table_id.count("-sband-") != 1:
        raise PackGeneratorError(
            "role template table_id must contain exactly one '-sband-' token"
        )
    config["table_id"] = table_id.replace("-sband-", f"-{band}band-", 1)

    axes = config.get("axes")
    if not isinstance(axes, list):
        raise PackGeneratorError("role template axes must be an array")
    frequency_axes = [axis for axis in axes if axis.get("kind") == "frequency"]
    if len(frequency_axes) != 1:
        raise PackGeneratorError("role template must contain exactly one frequency axis")
    frequency_axes[0]["coordinates"] = [frequency_hz]

    dielectric = config.get("dielectric")
    if not isinstance(dielectric, dict):
        raise PackGeneratorError("role template dielectric must be an object")
    if dielectric.get("model") == generate_lut.TEMPERATURE_WATER_DIELECTRIC_MODEL:
        dielectric["frequency_range_hz"] = (
            [2.0e9, 4.0e9] if band == "s" else [2.0e9, 10.0e9]
        )

    # The locked single-table validator remains the authoritative physics,
    # material, ODF, solver, axis, and execution contract.
    generate_lut.validate_config(config)
    return config


def load_role_configs(template_root: Path, band: str) -> dict[str, dict[str, Any]]:
    configs: dict[str, dict[str, Any]] = {}
    for spec in ROLE_SPECS:
        path = template_root / spec.template_directory / "config.json"
        if not path.is_file():
            raise PackGeneratorError(f"missing role config template: {path}")
        _, _, template = generate_lut.parse_exact_json(path)
        configs[spec.role] = retarget_role_config(template, band)
    return configs


def provenance_records(
    configs: Mapping[str, Mapping[str, Any]], tool_root: Path
) -> dict[str, Any]:
    if set(configs) != set(ROLE_NAMES):
        raise PackGeneratorError("provenance requires exactly the five pack roles")
    generator_record = {
        "schema": 1,
        "kernel": "pytmatrix-0.3.3",
        "tool_file_sha256": generate_lut.hash_tool_files(tool_root),
    }
    solver_record = {
        "schema": 1,
        "kernel": "pytmatrix-0.3.3",
        "roles": {
            role: copy.deepcopy(configs[role]["radar"]["solver"])
            for role in ROLE_NAMES
        },
    }
    odf_record = {
        "schema": 1,
        "roles": {
            role: copy.deepcopy(configs[role]["orientation"])
            for role in ROLE_NAMES
        },
    }
    return {
        "generator": generator_record,
        "generator_sha256": sha256_json(generator_record),
        "solver": solver_record,
        "solver_sha256": sha256_json(solver_record),
        "odf": odf_record,
        "odf_sha256": sha256_json(odf_record),
    }


def build_pack_manifest(
    *,
    band: str,
    pack_id: str,
    science_revision: str,
    role_files: Sequence[Mapping[str, Any]],
    provenance: Mapping[str, Any],
) -> dict[str, Any]:
    _validate_pack_id(pack_id)
    _validate_science_revision(science_revision)
    manifest = {
        "pack_schema": PACK_SCHEMA,
        "pack_id": pack_id,
        "band": band,
        "frequency_hz": exact_band_frequency_hz(band),
        "science_revision": science_revision,
        # Generation is not validation. In particular, C/X cannot be promoted
        # by this tool even when all native solver calls complete.
        "validation_status": PACK_VALIDATION_STATUS,
        "generator_sha256": provenance["generator_sha256"],
        "solver_sha256": provenance["solver_sha256"],
        "odf_sha256": provenance["odf_sha256"],
        "role_files": [dict(entry) for entry in role_files],
    }
    validate_pack_manifest(manifest)
    return manifest


def validate_pack_manifest(manifest: Mapping[str, Any]) -> None:
    expected_keys = {
        "pack_schema",
        "pack_id",
        "band",
        "frequency_hz",
        "science_revision",
        "validation_status",
        "generator_sha256",
        "solver_sha256",
        "odf_sha256",
        "role_files",
    }
    if set(manifest) != expected_keys:
        raise PackGeneratorError(
            f"pack manifest keys must be exactly {sorted(expected_keys)!r}"
        )
    if manifest["pack_schema"] != PACK_SCHEMA:
        raise PackGeneratorError("pack_schema must equal 1")
    pack_id = manifest["pack_id"]
    if not isinstance(pack_id, str):
        raise PackGeneratorError("pack_id must be text")
    _validate_pack_id(pack_id)
    band = manifest["band"]
    if not isinstance(band, str):
        raise PackGeneratorError("band must be text")
    if manifest["frequency_hz"] != exact_band_frequency_hz(band):
        raise PackGeneratorError("pack frequency must exactly match its declared band")
    science_revision = manifest["science_revision"]
    if not isinstance(science_revision, str):
        raise PackGeneratorError("science_revision must be text")
    _validate_science_revision(science_revision)
    if manifest["validation_status"] != PACK_VALIDATION_STATUS:
        raise PackGeneratorError(
            "generator may emit only unvalidated_research; validation promotion "
            "requires separate convergence and independent-validation records"
        )
    for field in ("generator_sha256", "solver_sha256", "odf_sha256"):
        digest = manifest[field]
        if not isinstance(digest, str) or not DIGEST_PATTERN.fullmatch(digest):
            raise PackGeneratorError(f"{field} must be canonical lowercase SHA-256")
        if digest == "0" * 64:
            raise PackGeneratorError(f"{field} cannot be the zero digest")

    role_files = manifest["role_files"]
    if not isinstance(role_files, list) or len(role_files) != len(ROLE_SPECS):
        raise PackGeneratorError("role_files must contain exactly five entries")
    role_keys = {
        "role",
        "lut_path",
        "lut_sha256",
        "lut_bytes",
        "config_path",
        "config_sha256",
        "config_bytes",
    }
    seen_roles: set[str] = set()
    seen_paths: set[str] = set()
    for entry in role_files:
        if not isinstance(entry, dict) or set(entry) != role_keys:
            raise PackGeneratorError(
                f"each role file must have exactly {sorted(role_keys)!r}"
            )
        role = entry["role"]
        if role not in ROLE_NAMES or role in seen_roles:
            raise PackGeneratorError(f"invalid or duplicate role {role!r}")
        seen_roles.add(role)
        for prefix in ("lut", "config"):
            path = entry[f"{prefix}_path"]
            if not isinstance(path, str):
                raise PackGeneratorError(f"{prefix}_path must be text")
            _validate_relative_path(path)
            if path in seen_paths:
                raise PackGeneratorError(f"duplicate pack file path {path!r}")
            seen_paths.add(path)
            digest = entry[f"{prefix}_sha256"]
            if not isinstance(digest, str) or not DIGEST_PATTERN.fullmatch(digest):
                raise PackGeneratorError(
                    f"{role} {prefix}_sha256 must be canonical lowercase SHA-256"
                )
            size = entry[f"{prefix}_bytes"]
            if isinstance(size, bool) or not isinstance(size, int) or size <= 0:
                raise PackGeneratorError(f"{role} {prefix}_bytes must be positive")
    if seen_roles != set(ROLE_NAMES):
        raise PackGeneratorError("role_files does not cover the exact five-role contract")


def _role_file_entry(staging_root: Path, spec: RoleSpec) -> dict[str, Any]:
    lut_relative = f"{spec.directory}/table.lut"
    config_relative = f"{spec.directory}/config.json"
    lut_path = staging_root / PurePosixPath(lut_relative)
    config_path = staging_root / PurePosixPath(config_relative)
    return {
        "role": spec.role,
        "lut_path": lut_relative,
        "lut_sha256": generate_lut.sha256_file(lut_path),
        "lut_bytes": lut_path.stat().st_size,
        "config_path": config_relative,
        "config_sha256": generate_lut.sha256_file(config_path),
        "config_bytes": config_path.stat().st_size,
    }


def _validate_environment_report(path: Path, tool_root: Path) -> None:
    if not path.is_file():
        raise PackGeneratorError(f"environment report does not exist: {path}")
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise PackGeneratorError(f"invalid environment report {path}: {error}") from error
    expected = generate_lut.hash_tool_files(tool_root)
    if report.get("tool_file_sha256") != expected:
        raise PackGeneratorError(
            "environment report tool hashes are stale; recapture the locked "
            "environment before generating a band pack"
        )


def _publish_directory(staging: Path, output: Path, overwrite: bool) -> None:
    if output.exists() and not overwrite:
        raise PackGeneratorError(f"refusing to overwrite existing pack {output}")
    backup = output.with_name(f".{output.name}.replaced")
    if backup.exists():
        raise PackGeneratorError(f"stale replacement backup blocks publish: {backup}")
    moved_old = False
    try:
        if output.exists():
            os.replace(output, backup)
            moved_old = True
        os.replace(staging, output)
    except BaseException:
        if moved_old and not output.exists() and backup.exists():
            os.replace(backup, output)
        raise
    if moved_old:
        shutil.rmtree(backup)


def generate_pack(args: argparse.Namespace) -> None:
    band = args.band
    frequency_hz = exact_band_frequency_hz(band)
    pack_id = args.pack_id or default_pack_id(band)
    _validate_pack_id(pack_id)
    _validate_science_revision(args.science_revision)
    tool_root = Path(__file__).resolve().parent
    template_root = args.template_root.resolve()
    output = args.output.resolve()
    environment_report = args.environment_report.resolve()
    if output.exists() and not output.is_dir():
        raise PackGeneratorError(f"pack output exists and is not a directory: {output}")
    if output.exists() and not args.overwrite:
        raise PackGeneratorError(f"refusing to overwrite existing pack {output}")
    _validate_environment_report(environment_report, tool_root)
    configs = load_role_configs(template_root, band)

    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(
        tempfile.mkdtemp(prefix=f".{output.name}.building-", dir=output.parent)
    )
    published = False
    try:
        staged_environment = staging / "environment.json"
        shutil.copyfile(environment_report, staged_environment)
        for spec in ROLE_SPECS:
            role_root = staging / spec.directory
            role_root.mkdir(parents=True, exist_ok=True)
            config_path = role_root / "config.json"
            generate_lut.write_json(config_path, configs[spec.role])
            generate_lut.generate(
                argparse.Namespace(
                    config=config_path,
                    output=role_root / "table.lut",
                    manifest=role_root / "generation.json",
                    environment_report=staged_environment,
                    emitter=args.emitter,
                    overwrite=False,
                )
            )

        provenance = provenance_records(configs, tool_root)
        role_files = [_role_file_entry(staging, spec) for spec in ROLE_SPECS]
        manifest = build_pack_manifest(
            band=band,
            pack_id=pack_id,
            science_revision=args.science_revision,
            role_files=role_files,
            provenance=provenance,
        )
        generate_lut.write_json(
            staging / "provenance.json",
            {
                "schema": 1,
                "band": band,
                "frequency_hz": frequency_hz,
                **provenance,
            },
        )
        generate_lut.write_json(staging / "pack.json", manifest)
        _publish_directory(staging, output, args.overwrite)
        published = True
    finally:
        if not published:
            shutil.rmtree(staging, ignore_errors=True)
    print(
        f"generated unvalidated exact-{band.upper()} pack at {frequency_hz:.0f} Hz: "
        f"{output}"
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--band",
        choices=tuple(generate_lut.EXACT_PROPERTY_BAND_FREQUENCIES_HZ),
        required=True,
    )
    parser.add_argument("--template-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--environment-report", type=Path, required=True)
    parser.add_argument("--science-revision", required=True)
    parser.add_argument("--pack-id")
    parser.add_argument("--emitter", default="brslut-emitter")
    parser.add_argument("--overwrite", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        generate_pack(args)
        return 0
    except (generate_lut.GeneratorError, PackGeneratorError, OSError) as error:
        print(f"generate_band_pack.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
