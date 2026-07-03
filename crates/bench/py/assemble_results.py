"""Merge external-engine / v4-HRRR / ORPG spot-check results into
``docs/dealias-v4-baselines.json`` (same per-engine schema as the Rust rows)
and print a markdown summary table.

Inputs (produced by the workflow in README.md), all in --results-dir:
  ext_<case>.json          run_external.py output  -> engines pyart-region, unravel
  rust_A_hrrr.json etc.    bowecho-bench --json (v4 + HRRR fixture) -> engine v4-hrrr
  orpg_*.json              orpg_probe.py output    -> engine orpg-l3-spot

Usage:
  python assemble_results.py --results-dir <dir> --baselines ../../docs/dealias-v4-baselines.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

EXT_CASES = {
    "A-derecho": "ext_A-derecho.json",
    "B-moore": "ext_B-moore.json",
    "C-ida": "ext_C-ida.json",
    "D-blob": "ext_D-blob.json",
    "D-control": "ext_D-control.json",
    "E-keax12": "ext_E-keax12.json",
    "E-ktlx12": "ext_E-ktlx12.json",
}

HRRR_CASES = {
    "A-derecho": "rust_A_hrrr.json",
    "C-ida": "rust_C_hrrr.json",
    "D-blob": "rust_Dblob_hrrr.json",
    "D-control": "rust_Dctrl_hrrr.json",
    "E-keax12": "rust_E12_hrrr.json",
    # B-moore / E-ktlx12: 2013 predates the HRRR archive (2014-09-30) — RAP only.
}


def read_json_any_bom(path: Path) -> dict:
    raw = path.read_bytes()
    if raw.startswith(b"\xff\xfe") or raw.startswith(b"\xfe\xff"):
        return json.loads(raw.decode("utf-16"))
    return json.loads(raw.decode("utf-8-sig"))


def rnd(value, digits):
    return None if value is None else round(value, digits)


def ext_row(engine: dict) -> dict:
    return {
        "boundaries_lowest": engine["boundaries_lowest"],
        "boundaries_volume": engine["boundaries_volume"],
        "rms_env": rnd(engine.get("rms_env"), 2),
        "rms_harmonic": rnd(engine.get("rms_harmonic"), 2),
        "percent_modified": rnd(engine.get("percent_modified"), 2),
        "specks_lowest": engine.get("specks_lowest"),
        "couplet_max_dv": rnd(engine.get("couplet_max_dv"), 1),
        "max_inbound": rnd(engine.get("max_inbound"), 1),
        "multifold_gates": engine.get("multifold_gates"),
        "multifold_speckle": engine.get("multifold_speckle"),
        "volume_ms_best": rnd(engine["volume_ms_best"], 1),
        "worst_tilt_ms": rnd(engine["worst_tilt_ms"], 1),
        "amortized": True,
        "deterministic": engine["deterministic"],
        "invented_gates_masked": engine.get("invented_gates_masked", 0),
        "runtime_note": "Python (numpy/scipy/numba); apples/oranges vs Rust rows",
        **(
            {"correct_branch_percent": rnd(engine["correct_branch_percent"], 2)}
            if engine.get("correct_branch_percent") is not None
            else {}
        ),
    }


def hrrr_row(rust: dict) -> dict:
    engine = rust["engines"][0]
    row = {
        "boundaries_lowest": engine["boundaries_lowest"],
        "boundaries_volume": engine["boundaries_volume"],
        "rms_env": engine["rms_env"],
        "rms_harmonic": engine["rms_harmonic"],
        "percent_modified": rnd(engine["percent_modified"], 2),
        "specks_lowest": engine["specks_lowest"],
        "couplet_max_dv": rnd(engine["couplet_max_dv"], 1),
        "max_inbound": rnd(engine["max_inbound"], 1),
        "multifold_gates": engine["multifold_gates"],
        "multifold_speckle": engine["multifold_speckle"],
        "volume_ms_best": engine["volume_ms"],
        "worst_tilt_ms": engine["worst_tilt_ms"],
        "amortized": engine["amortized"],
        "deterministic": engine["deterministic"],
        "rms_env_note": "rms_env is vs the HRRR fixture, not the RAP fixture",
    }
    if engine.get("correct_branch_percent") is not None:
        row["correct_branch_percent"] = engine["correct_branch_percent"]
    return row


def probe_string(mean) -> str:
    return "null" if mean is None else f"{mean:+.1f}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--results-dir", required=True, type=Path)
    parser.add_argument(
        "--baselines",
        type=Path,
        default=Path(__file__).resolve().parents[3] / "docs" / "dealias-v4-baselines.json",
    )
    args = parser.parse_args()

    baselines = json.loads(args.baselines.read_text(encoding="utf-8"))
    cases = baselines["cases"]

    for case, filename in EXT_CASES.items():
        path = args.results_dir / filename
        if not path.exists():
            print(f"skip {case}: {filename} missing")
            continue
        ext = read_json_any_bom(path)
        for engine_name, engine in ext["engines"].items():
            cases[case]["engines"][engine_name] = ext_row(engine)
            if engine.get("probes"):
                cases[case].setdefault("probes", {})[engine_name] = {
                    p["label"]: probe_string(p["mean"]) for p in engine["probes"]
                }

    for case, filename in HRRR_CASES.items():
        path = args.results_dir / filename
        if not path.exists():
            print(f"skip {case}: {filename} missing")
            continue
        rust = read_json_any_bom(path)
        cases[case]["engines"]["v4-hrrr"] = hrrr_row(rust)
        engine = rust["engines"][0]
        if engine.get("probes"):
            cases[case].setdefault("probes", {})["v4-hrrr"] = {
                p["label"]: probe_string(p["mean"]) for p in engine["probes"]
            }

    # ORPG spot rows: probes / spot values only, never gate-count metrics
    # (Level-III is 1 deg x 0.25 km vs super-res 0.5 deg — not comparable).
    orpg = {
        "B-moore": ("orpg_ktlx.json", "quantitative N0U (product 99), KTLX 2013-05-20 20:16Z, 0.5 deg tilt, 1 deg x 0.25 km"),
        "C-ida": ("orpg_klix.json", "quantitative N0U (product 99), KLIX 2021-08-29 16:32:52Z; -65.0 is at the product's encoding floor (7 gates saturated)"),
    }
    for case, (filename, note) in orpg.items():
        path = args.results_dir / filename
        if not path.exists():
            continue
        data = read_json_any_bom(path)
        row: dict = {key: None for key in (
            "boundaries_lowest", "boundaries_volume", "rms_env", "rms_harmonic",
            "percent_modified", "specks_lowest", "multifold_gates",
            "multifold_speckle", "volume_ms_best", "worst_tilt_ms",
        )}
        row["couplet_max_dv"] = rnd(data.get("couplet_max_dv_15_40km"), 1)
        row["max_inbound"] = rnd(data.get("max_inbound"), 1)
        row["amortized"] = None
        row["deterministic"] = None
        row["note"] = note
        cases[case]["engines"]["orpg-l3-spot"] = row

    for case, filename, probes in (
        ("D-blob", "orpg_kmbx_blob.json", None),
        ("D-control", "orpg_kmbx_ctrl.json", None),
    ):
        path = args.results_dir / filename
        if not path.exists():
            continue
        data = read_json_any_bom(path)
        row = {key: None for key in (
            "boundaries_lowest", "boundaries_volume", "rms_env", "rms_harmonic",
            "percent_modified", "specks_lowest", "couplet_max_dv", "max_inbound",
            "multifold_gates", "multifold_speckle", "volume_ms_best", "worst_tilt_ms",
        )}
        row["amortized"] = None
        row["deterministic"] = None
        row["note"] = (
            "qualitative branch read from the IEM-archived RIDGE N0S "
            "(storm-relative velocity) render " + data["file"] + "; raw 2026 "
            "Level-III is not publicly archived (NCEI order-only)"
        )
        cases[case]["engines"]["orpg-l3-spot"] = row
        cases[case].setdefault("probes", {})["orpg-l3-spot"] = {
            p["label"]: f"{p['verdict']} (SRM render)" for p in data["probes"]
        }

    meta = baselines["_meta"]
    meta["env_fixtures"] = (
        "crates/bench/fixtures/dealias/env_*.json (RAP 13-km 0-h analyses) and "
        "env_*_hrrr.json (HRRR 3-km 0-h analyses, crates/bench/py/hrrr_profile.py)"
    )
    meta["external_rows"] = (
        "pyart-region (Helmus & Collis 2016, doi:10.5334/jors.119) and unravel "
        "(Louf et al. 2020, doi:10.1175/JTECH-D-19-0020.1) scored with the "
        "parity-validated Python metric port (crates/bench/py, see "
        "docs/dealias-external-baselines.md); v4-hrrr = v4 with HRRR 3-km 0-h "
        "analysis fixtures (env_*_hrrr.json; B-moore/E-ktlx12 predate the HRRR "
        "archive) — a resolution experiment, indistinguishable from v4(RAP), "
        "which is why production anchoring is RAP-only per amended spec s16; "
        "orpg-l3-spot = NOAA operational ORPG "
        "output spot checks from archived Level-III (quantitative N0U where "
        "public; qualitative RIDGE N0S renders for 2026). External runtimes "
        "are Python — apples/oranges vs Rust."
    )

    args.baselines.write_text(
        json.dumps(baselines, indent=1) + "\n", encoding="utf-8", newline="\n"
    )
    print(f"updated {args.baselines}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
