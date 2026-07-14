#!/usr/bin/env python3
"""Build the byte-exact legacy S-band property T-matrix research data pack.

The archive deliberately uses ZIP_STORED. Together with fixed entry order,
timestamps, permissions, and paths, this makes the release asset independent
of zlib versions while preserving every LUT/config byte exactly.
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import sys
import zipfile


ASSET_NAME = "bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip"
SOURCE_ROOT = Path("research_only_assets/tmatrix/pytmatrix-0.3.3")

# path, exact byte length, SHA-256. Keep this list in the same role/path order
# as crates/app_ui/src/wrf_tmatrix_legacy_pack.rs.
MEMBERS = (
    (
        "property_p3_ishmael_dry_oblate_sband_unvalidated/table.lut",
        44_365_435,
        "30c8da4093b845faa415339f2cb5b4831f3450dc18afea3aacb2e2fabdcc4ad8",
    ),
    (
        "property_p3_ishmael_dry_oblate_sband_unvalidated/config.json",
        8_274,
        "e08adbe6d6e8a1b9a80ba920a0f82539c4056d9758e1e44ba11bcf907ba5cd19",
    ),
    (
        "property_p3_ishmael_dry_prolate_sband_unvalidated/table.lut",
        9_527_106,
        "7a563e1103cb1a61ccb94ce72513d82b9fdd68a6faddb4aa8ae46112fb0109c0",
    ),
    (
        "property_p3_ishmael_dry_prolate_sband_unvalidated/config.json",
        7_319,
        "c2b973ab36fa26edb8d9d82f7dbb2ae5df52feec4bf93c880e304cad5aa2ff49",
    ),
    (
        "property_p3_ishmael_wet_oblate_sband_unvalidated/table.lut",
        73_279_689,
        "6c376422c512ebfc37dc5b2038defea799995d1821170da74b4af87276df1dd7",
    ),
    (
        "property_p3_ishmael_wet_oblate_sband_unvalidated/config.json",
        8_028,
        "61cd6f72beaf503485168a9b43e72db8ebc49ef993389e67ff8906e0c42e9bf8",
    ),
    (
        "property_p3_ishmael_wet_prolate_sband_unvalidated/table.lut",
        62_220_152,
        "9c55a51eb63a982005564eb1f35bbb24dfad5f22a65ed820ac7c1d5cf19f1040",
    ),
    (
        "property_p3_ishmael_wet_prolate_sband_unvalidated/config.json",
        7_857,
        "0fa0bd759b64c8e6f62bcf629fd2d5c2733aa433fb0e2eb2793c2cbaa58e1758",
    ),
    (
        "property_rain_sband_unvalidated/table.lut",
        1_968_373,
        "396ca95c58d70a9a413d90799bd790dc389179dc9a38f48152e464bf852d5e11",
    ),
    (
        "property_rain_sband_unvalidated/config.json",
        5_278,
        "387f93b5998d9f6010ffb60d081b31dc64a556cf33ae7b34256fc356d26140b5",
    ),
    # The PyTMatrix 0.3.3 MIT notice is distributed beside the derived tables.
    # Its fixed length/hash are filled from the repository copy below.
    (
        "PYTMATRIX-LICENSE.txt",
        1_071,
        "be9109e8cf7842d4e789a6d314c011b4a1773059020895bc1f032882a03bae1d",
    ),
)


def sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def repository_root() -> Path:
    return Path(__file__).resolve().parent.parent


def validate_sources(root: Path) -> None:
    for relative, expected_bytes, expected_sha in MEMBERS:
        path = root / relative
        if not path.is_file():
            raise RuntimeError(f"missing pack input: {path}")
        actual_bytes = path.stat().st_size
        if actual_bytes != expected_bytes:
            raise RuntimeError(
                f"{relative}: expected {expected_bytes} bytes, got {actual_bytes}"
            )
        actual_sha = sha256_path(path)
        if actual_sha != expected_sha:
            raise RuntimeError(
                f"{relative}: expected SHA-256 {expected_sha}, got {actual_sha}"
            )


def write_pack(root: Path, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.unlink(missing_ok=True)
    with zipfile.ZipFile(
        temporary,
        mode="w",
        compression=zipfile.ZIP_STORED,
        allowZip64=True,
        strict_timestamps=True,
    ) as archive:
        for relative, _, _ in MEMBERS:
            data = (root / relative).read_bytes()
            entry = zipfile.ZipInfo(relative, date_time=(1980, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_STORED
            entry.create_system = 3
            entry.external_attr = 0o100644 << 16
            entry.flag_bits = 0
            archive.writestr(entry, data)
    temporary.replace(output)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help=f"archive path (default: target/research-packs/{ASSET_NAME})",
    )
    args = parser.parse_args()

    repo = repository_root()
    source = repo / SOURCE_ROOT
    output = args.output or repo / "target" / "research-packs" / ASSET_NAME
    try:
        validate_sources(source)
        write_pack(source, output)
    except (OSError, RuntimeError, zipfile.BadZipFile) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    print(output.resolve())
    print(f"bytes={output.stat().st_size}")
    print(f"sha256={sha256_path(output)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
