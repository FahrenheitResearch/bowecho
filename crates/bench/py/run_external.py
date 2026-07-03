"""Run EXTERNAL dealiasers on the dealias-v4 battery cases, scored with the
parity-validated metric port (``dealias_metrics.py``).

Engines:
- ``pyart-region``: Py-ART ``dealias_region_based`` (Helmus & Collis 2016,
  JORS 4(1):e25, doi:10.5334/jors.119), shipped defaults (interval_splits=3,
  skip 100/100, default GateFilter) with per-sweep Nyquist from the file.
- ``unravel``: UNRAVEL (Louf et al. 2020, JTECH 37, 741-758,
  doi:10.1175/JTECH-D-19-0020.1; github.com/vlouf/dealias), default strategy,
  alpha=0.8, do_3d=True.

Fairness / faithfulness decisions (documented, deliberate):
- Both engines run on the SAME Level-II file as the Rust battery, restricted
  to the velocity-bearing sweeps (``Radar.extract_sweeps``).  NEXRAD split
  cuts interleave velocity-less surveillance sweeps; feeding those to
  UNRAVEL's bottom-up 3D pass would start its sweep ladder on an empty tilt,
  which is a harness artifact, not an algorithm property.
- UNRAVEL receives an explicit per-sweep Nyquist list read from the file.
  Its ``nyquist_velocity=None`` default silently applies the FIRST ray's
  Nyquist to the whole volume, which is wrong on split-cut VCPs (e.g. the
  KLIX case runs 23.2 / 32.1 m/s cuts in one volume).
- UNRAVEL receives a GateFilter that excludes exactly the invalid-velocity
  gates.  Its default filter (``filtering.do_gatefilter``) needs
  reflectivity and applies a despeckle+threshold pass, i.e. it would score a
  *filtered* field while our engines are scored on the full field.
- Engine outputs are re-masked to NaN wherever the INPUT velocity was
  missing (the Rust engines never invent data at empty gates; UNRAVEL
  initializes its output array with zeros there).  Invented-gate counts are
  reported for transparency before masking.
- Determinism: each engine runs twice; outputs must be byte-identical
  (bitwise, NaN-mask included) or the row is marked non-deterministic.
- Runtime: wall-clock of the dealias call only (decode excluded), best of
  the timed runs, after an untimed warmup run for numba JIT.  Python vs
  Rust runtimes are apples/oranges; the report caveats this.

Usage:
  python run_external.py --volume KEAX20260609_055143_V06 --case A-derecho
      [--env-fixture env_keax.json] [--probe 339,20,blob ...]
      [--engines pyart-region,unravel] [--out out.json]
  python run_external.py --rewrap-dump <dump-dir> --case E-keax12 ...
      (synthetic Case E rebuilt from `bowecho-bench --dealias --rewrap N
       --dump-fields <dump-dir>`: raw_cut00 = rewrapped input,
       truth_cut00.bin = exact truth)
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import warnings
from pathlib import Path

import numpy as np

import dealias_metrics as dm

warnings.filterwarnings("ignore")

import pyart  # noqa: E402
import unravel  # noqa: E402

VEL = "velocity"


def parse_probe(spec: str) -> tuple[str, float, float]:
    parts = spec.split(",")
    az, km = float(parts[0]), float(parts[1])
    label = parts[2] if len(parts) > 2 else f"az{az}/r{km}km"
    return (label, az, km)


def velocity_sweeps(radar) -> list[int]:
    """Sweeps that carry any valid velocity gate (split-cut CS sweeps do not)."""
    out = []
    for sweep in range(radar.nsweeps):
        data = radar.get_field(sweep, VEL)
        if np.isfinite(np.ma.filled(data, np.nan)).any():
            out.append(sweep)
    return out


def lowest_sweep(radar, sweeps: list[int]) -> int:
    angles = radar.fixed_angle["data"]
    return min(sweeps, key=lambda s: (float(angles[s]), s))


def field_from_radar(radar, sweep: int, data: np.ndarray | None = None) -> dm.Field:
    """Build a metric Field from one sweep (mirrors the Rust decode_field)."""
    sl = radar.get_slice(sweep)
    if data is None:
        data = radar.fields[VEL]["data"]
    values = np.ma.filled(data[sl], np.nan).astype(np.float32)
    nyq = dm.fill_nyquist_fallback(
        np.ma.filled(
            radar.instrument_parameters["nyquist_velocity"]["data"][sl], np.nan
        ).astype(np.float32)
    )
    az = np.ma.filled(radar.azimuth["data"][sl], np.nan).astype(np.float32)
    rng = radar.range["data"]
    first = float(rng[0])
    spacing = float(rng[1] - rng[0]) if len(rng) > 1 else 1.0
    return dm.Field(
        rows=values.shape[0],
        gates=values.shape[1],
        wraps=dm.sweep_wraps(az),
        values=values,
        nyq=nyq,
        azimuth_deg=az,
        first_gate_m=first,
        gate_spacing_m=spacing,
        elevation_deg=float(radar.fixed_angle["data"][sweep]),
    )


def run_pyart_region(radar) -> np.ndarray:
    corr = pyart.correct.dealias_region_based(radar, vel_field=VEL)
    return np.ma.filled(corr["data"], np.nan).astype(np.float32)


def run_unravel(radar) -> np.ndarray:
    nyquist_list = [
        float(radar.instrument_parameters["nyquist_velocity"]["data"][radar.get_slice(s)][0])
        for s in range(radar.nsweeps)
    ]
    gatefilter = pyart.filters.GateFilter(radar)
    gatefilter.exclude_masked(VEL)
    gatefilter.exclude_invalid(VEL)
    out = unravel.unravel_3D_pyart(
        radar,
        velname=VEL,
        dbzname="reflectivity",
        gatefilter=gatefilter,
        nyquist_velocity=nyquist_list,
        strategy="default",
    )
    return np.asarray(out, dtype=np.float32)


ENGINES = {"pyart-region": run_pyart_region, "unravel": run_unravel}


def build_rewrap_radar(dump_dir: Path):
    """Reconstruct the synthetic Case-E single-tilt volume from a Rust
    ``--rewrap --dump-fields`` dump as a Py-ART radar object."""
    raw = dm.load_dumped_field(dump_dir, "raw", 0)
    radar = pyart.testing.make_empty_ppi_radar(raw.gates, raw.rows, 1)
    radar.azimuth["data"] = raw.azimuth_deg.astype(np.float64)
    radar.elevation["data"] = np.full(raw.rows, raw.elevation_deg)
    radar.fixed_angle["data"] = np.array([raw.elevation_deg])
    radar.range["data"] = (
        raw.first_gate_m + np.arange(raw.gates) * raw.gate_spacing_m
    ).astype(np.float32)
    radar.instrument_parameters = {
        "nyquist_velocity": {"data": raw.nyq.astype(np.float64)}
    }
    radar.fields[VEL] = {
        "data": np.ma.masked_invalid(raw.values.astype(np.float64)),
        "units": "m/s",
        "standard_name": "radial_velocity_of_scatterers_away_from_instrument",
    }
    return radar


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--volume", type=Path)
    parser.add_argument("--rewrap-dump", type=Path)
    parser.add_argument("--case", required=True)
    parser.add_argument("--engines", default="pyart-region,unravel")
    parser.add_argument("--env-fixture", type=Path, default=None)
    parser.add_argument("--probe", action="append", default=[])
    parser.add_argument("--iters", type=int, default=2)
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()
    if (args.volume is None) == (args.rewrap_dump is None):
        parser.error("exactly one of --volume / --rewrap-dump")

    probes = [parse_probe(p) for p in args.probe]

    truth = None
    if args.volume is not None:
        radar = pyart.io.read_nexrad_archive(str(args.volume))
        sweeps = velocity_sweeps(radar)
        radar = radar.extract_sweeps(sweeps)
        sweeps = list(range(radar.nsweeps))
    else:
        radar = build_rewrap_radar(args.rewrap_dump)
        sweeps = [0]
        raw0 = dm.load_dumped_field(args.rewrap_dump, "raw", 0)
        truth = dm.load_dumped_bin(args.rewrap_dump, "truth_cut00.bin", raw0.rows, raw0.gates)

    low = lowest_sweep(radar, sweeps)
    raw_fields = {s: field_from_radar(radar, s) for s in sweeps}
    input_finite = np.isfinite(np.ma.filled(radar.fields[VEL]["data"], np.nan))

    env_projection = None
    env_source = None
    if args.env_fixture is not None:
        fixture = json.loads(args.env_fixture.read_text())
        env_source = fixture["source"]
        levels = [(l["height_m_arl"], l["u_mps"], l["v_mps"]) for l in fixture["levels"]]
        rl = raw_fields[low]
        env_projection = dm.project_environmental_winds(
            levels,
            rl.elevation_deg,
            np.mod(rl.azimuth_deg, np.float32(360.0)),
            rl.first_gate_m,
            rl.gate_spacing_m,
            rl.gates,
        )

    result = {
        "case": args.case,
        "source": str(args.volume or args.rewrap_dump),
        "env_source": env_source,
        "n_velocity_sweeps": len(sweeps),
        "lowest_sweep_elevation_deg": raw_fields[low].elevation_deg,
        "versions": {
            "pyart": pyart.__version__,
            "unravel": getattr(unravel, "__version__", "1.5.0 (git)"),
            "numpy": np.__version__,
        },
        "engines": {},
    }

    for name in args.engines.split(","):
        runner = ENGINES[name]
        print(f"[{args.case}] {name}: warmup...", flush=True)
        first = runner(radar)  # warmup (numba JIT for unravel)
        timings = []
        outputs = []
        for _ in range(max(args.iters, 1)):
            started = time.perf_counter()
            out = runner(radar)
            timings.append((time.perf_counter() - started) * 1000.0)
            outputs.append(out)
        deterministic = all(
            np.array_equal(outputs[0], other, equal_nan=True) for other in outputs[1:]
        ) and np.array_equal(first, outputs[0], equal_nan=True)
        out = outputs[0]
        invented = int(np.count_nonzero(np.isfinite(out) & ~input_finite))
        out[~input_finite] = np.nan  # engines must not invent gates; see module doc

        row: dict = {
            "boundaries_lowest": 0,
            "boundaries_volume": 0,
            "volume_ms_best": min(timings),
            "worst_tilt_ms": min(timings) / max(len(sweeps), 1),
            "amortized": True,
            "deterministic": bool(deterministic),
            "invented_gates_masked": invented,
        }
        for s in sweeps:
            sl = radar.get_slice(s)
            raw = raw_fields[s]
            outf = dm.Field(
                rows=raw.rows,
                gates=raw.gates,
                wraps=raw.wraps,
                values=out[sl],
                nyq=raw.nyq,
                azimuth_deg=raw.azimuth_deg,
                first_gate_m=raw.first_gate_m,
                gate_spacing_m=raw.gate_spacing_m,
                elevation_deg=raw.elevation_deg,
            )
            report = dm.score_field(
                outf,
                raw,
                env_projection if s == low else None,
                s == low,
                probes=probes if s == low else None,
                truth=truth if s == low else None,
            )
            row["boundaries_volume"] += report["boundaries"]
            if s == low:
                row["boundaries_lowest"] = report["boundaries"]
                for key in (
                    "rms_env",
                    "rms_harmonic",
                    "percent_modified",
                    "specks_lowest",
                    "couplet_max_dv",
                    "max_inbound",
                    "multifold_gates",
                    "multifold_speckle",
                    "probes",
                    "correct_branch_percent",
                ):
                    if key in report:
                        row[key] = report[key]
        result["engines"][name] = row
        print(f"[{args.case}] {name}: bnd_low {row['boundaries_lowest']} bnd_vol "
              f"{row['boundaries_volume']} vol {row['volume_ms_best']:.0f}ms det "
              f"{deterministic} invented {invented}", flush=True)

    text = json.dumps(result, indent=1)
    if args.out:
        args.out.write_text(text, encoding="utf-8")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
