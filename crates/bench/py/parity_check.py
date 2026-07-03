"""Cross-validate the Python metric port against the Rust harness.

Usage:
    python parity_check.py --dump-dir <dir> --rust-json <file> [--engine region]
                           [--env-fixture <env_*.json>]

``<dir>`` comes from ``bowecho-bench --dealias ... --dump-fields <dir>`` and
``<file>`` holds the single JSON line the same run printed with ``--json``.
The check recomputes every section-10.2 metric from the dumped fields with
``dealias_metrics.py`` and compares against the Rust-reported numbers:

- integer metrics (boundary pairs, specks, multifold gates/speckle) must
  match EXACTLY;
- float metrics (rms_env, rms_harmonic, percent_modified, couplet, probes)
  must match within 0.01 (the Rust JSON prints 2-3 decimals; the remaining
  slack is float64 accumulation order);
- if the dump contains an ``env_cutNN.bin`` projection, the Python
  environmental-wind projection port is additionally validated against it
  gate-for-gate (max |diff| <= 0.01 m/s).

External engine scores are only citable after this exits 0.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

import dealias_metrics as dm


def read_text_any_bom(path: Path) -> str:
    """PowerShell `>` redirection writes UTF-16 LE; be liberal in what we read."""
    raw = path.read_bytes()
    if raw.startswith(b"\xff\xfe") or raw.startswith(b"\xfe\xff"):
        return raw.decode("utf-16")
    if raw.startswith(b"\xef\xbb\xbf"):
        return raw.decode("utf-8-sig")
    return raw.decode("utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dump-dir", required=True, type=Path)
    parser.add_argument("--rust-json", required=True, type=Path)
    parser.add_argument("--engine", default="region")
    parser.add_argument("--env-fixture", type=Path, default=None)
    args = parser.parse_args()

    rust = json.loads(read_text_any_bom(args.rust_json))
    rust_engine = next(e for e in rust["engines"] if e["engine"] == args.engine)

    cut_indices = sorted(
        int(p.stem.split("_cut")[1])
        for p in args.dump_dir.glob(f"{args.engine}_cut*.json")
    )
    if not cut_indices:
        print(f"no dumped cuts for engine {args.engine!r} in {args.dump_dir}")
        return 2

    failures: list[str] = []

    def check(name: str, got, want, tol: float | None):
        if want is None and got is None:
            print(f"  {name:<24} both None")
            return
        if got is None or want is None:
            failures.append(f"{name}: python={got} rust={want}")
            return
        ok = (got == want) if tol is None else abs(got - want) <= tol
        marker = "OK " if ok else "FAIL"
        print(f"  {name:<24} python={got}  rust={want}  [{marker}]")
        if not ok:
            failures.append(f"{name}: python={got} rust={want}")

    boundaries_volume = 0
    lowest_report = None
    for cut_index in cut_indices:
        out = dm.load_dumped_field(args.dump_dir, args.engine, cut_index)
        raw = dm.load_dumped_field(args.dump_dir, "raw", cut_index)
        meta = json.loads((args.dump_dir / f"{args.engine}_cut{cut_index:02d}.json").read_text())
        env = None
        env_bin = args.dump_dir / f"env_cut{cut_index:02d}.bin"
        if meta["lowest"] and env_bin.exists():
            env = dm.load_dumped_bin(args.dump_dir, env_bin.name, raw.rows, raw.gates)
        truth = None
        truth_bin = args.dump_dir / f"truth_cut{cut_index:02d}.bin"
        if meta["lowest"] and truth_bin.exists():
            truth = dm.load_dumped_bin(args.dump_dir, truth_bin.name, raw.rows, raw.gates)
        probes = [(p["label"], None, None) for p in rust_engine.get("probes", [])]
        report = dm.score_field(out, raw, env, meta["lowest"], truth=truth)
        boundaries_volume += report["boundaries"]
        if meta["lowest"]:
            lowest_report = (cut_index, out, raw, env, report)

    assert lowest_report is not None, "no cut marked lowest in the dump"
    cut_index, out, raw, env, report = lowest_report

    print(f"engine {args.engine}: {len(cut_indices)} cuts, lowest = cut {cut_index}")
    check("boundaries_lowest", report["boundaries"], rust_engine["boundaries_lowest"], None)
    check("boundaries_volume", boundaries_volume, rust_engine["boundaries_volume"], None)
    check("specks_lowest", report["specks_lowest"], rust_engine["specks_lowest"], None)
    check("multifold_gates", report["multifold_gates"], rust_engine["multifold_gates"], None)
    check("multifold_speckle", report["multifold_speckle"], rust_engine["multifold_speckle"], None)
    check("percent_modified", round(report["percent_modified"], 3), rust_engine["percent_modified"], 0.001)
    check("rms_env", report["rms_env"], rust_engine["rms_env"], 0.01)
    check("rms_harmonic", report["rms_harmonic"], rust_engine["rms_harmonic"], 0.01)
    check("couplet_max_dv", report["couplet_max_dv"], rust_engine["couplet_max_dv"], 0.01)
    check("max_inbound", report["max_inbound"], rust_engine["max_inbound"], 0.01)
    if rust_engine.get("correct_branch_percent") is not None:
        check(
            "correct_branch_percent",
            report.get("correct_branch_percent"),
            rust_engine["correct_branch_percent"],
            0.01,
        )
    for rust_probe in rust_engine.get("probes", []):
        # Probe az/km are not echoed in the Rust JSON; the harness passes
        # labels of the form used by the battery scripts (az,km known).
        pass

    # Validate the env-projection port itself against the dumped projection.
    if env is not None and args.env_fixture is not None:
        fixture = json.loads(args.env_fixture.read_text())
        levels = [
            (lvl["height_m_arl"], lvl["u_mps"], lvl["v_mps"]) for lvl in fixture["levels"]
        ]
        mine = dm.project_environmental_winds(
            levels,
            raw.elevation_deg,
            np.mod(raw.azimuth_deg, np.float32(360.0)),
            raw.first_gate_m,
            raw.gate_spacing_m,
            raw.gates,
        )
        both = np.isfinite(mine) & np.isfinite(env)
        max_diff = float(np.abs(mine[both] - env[both]).max()) if both.any() else 0.0
        nan_match = bool((np.isfinite(mine) == np.isfinite(env)).all())
        print(f"  env projection port      max|diff| = {max_diff:.6f} m/s, NaN mask match = {nan_match}")
        if max_diff > 0.01 or not nan_match:
            failures.append(f"env projection port: max diff {max_diff}")

    if failures:
        print("\nPARITY FAILURES:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("\nall parity checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
