"""Python port of the bowecho-bench dealias eval metrics.

This module is a LINE-FOR-LINE port of the metric definitions in
``crates/bench/src/dealias_eval.rs`` (dealias-v4 spec section 10.2), used to
score EXTERNAL dealiasers (Py-ART ``dealias_region_based``, Helmus & Collis
2016, JORS 4(1):e25, doi:10.5334/jors.119; UNRAVEL, Louf et al. 2020, JTECH
37, 741-758, doi:10.1175/JTECH-D-19-0020.1) on the same ruler as the Rust
engines.  The port is only trusted after ``parity_check.py`` reproduces the
Rust-reported numbers on a field dumped by ``bowecho-bench --dealias
--dump-fields`` (integer metrics exactly, float metrics to tight tolerance).

Faithfulness notes (each mirrors a specific Rust behavior):
- medians are the UPPER median: Rust ``select_nth_unstable_by(len/2)`` picks
  sorted index ``len/2`` (0-based); ``numpy.median`` would average the two
  middles, so we sort and index instead;
- pair thresholds use ``f32::min`` semantics (NaN ignored -> ``numpy.fmin``);
- comparisons happen in float32 exactly where the Rust code compares f32
  (values, thresholds), and accumulate in float64 exactly where Rust widens
  (``f64::from(a - b)`` widens AFTER the f32 subtraction);
- ``round`` is half-away-from-zero (Rust ``f32::round``), not banker's;
- connected components are 4-connected with the azimuth wrap seam joined
  when the sweep closes 360 degrees.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from scipy import ndimage

BOUNDARY_NYQUIST_FRAC = np.float32(1.2)
CORRECT_BRANCH_TOLERANCE_MPS = np.float32(6.0)

# fit_range_band_reference constants (crates/render2d/src/cascade.rs;
# Browning & Wexler 1968, J. Appl. Meteor. 7, 105-113).
REFERENCE_BAND_GATES = 16
FIT_MIN_SAMPLES = 48
FIT_MIN_SECTORS = 5
FIT_TRIM_MPS = np.float32(12.0)

# 4/3-effective-earth beam height (Doviak & Zrnic 1993, 2nd ed., eq. 2.28b;
# crates/radar_core EARTH_RADIUS_M = 6_371_000).
EFFECTIVE_EARTH_RADIUS_M = 6_371_000.0 * 4.0 / 3.0


@dataclass
class Field:
    """Mirror of the Rust ``Field``: one decoded velocity tilt."""

    rows: int
    gates: int
    wraps: bool
    values: np.ndarray  # (rows, gates) float32, NaN = missing
    nyq: np.ndarray  # (rows,) float32, NaN = unknown
    azimuth_deg: np.ndarray  # (rows,) float32 raw radial azimuth
    first_gate_m: float
    gate_spacing_m: float
    elevation_deg: float


def sweep_wraps(azimuths: np.ndarray) -> bool:
    """Same rule as the engine: closes 360 deg when first/last azimuths are
    within 3 typical spacings."""
    rows = len(azimuths)
    if rows < 8:
        return False
    first, last = float(azimuths[0]), float(azimuths[-1])
    if not (math.isfinite(first) and math.isfinite(last)):
        return False
    gap = min((first - last) % 360.0, (last - first) % 360.0)
    return gap <= 3.0 * (360.0 / rows)


def fill_nyquist_fallback(nyq: np.ndarray) -> np.ndarray:
    """Rust decode_field: rows with no finite Nyquist get the upper median
    of the finite per-row values."""
    nyq = nyq.astype(np.float32).copy()
    finite = nyq[np.isfinite(nyq)]
    if finite.size:
        fallback = np.sort(finite)[finite.size // 2]
        nyq[~np.isfinite(nyq)] = fallback
    return nyq


def _shift_rows(values: np.ndarray, delta: int, wraps: bool) -> np.ndarray:
    """values[(row + delta) % rows] with NaN beyond the edge when not wrapping."""
    if delta == 0:
        return values
    if wraps:
        return np.roll(values, -delta, axis=0)
    out = np.full_like(values, np.nan)
    if delta > 0:
        out[:-delta] = values[delta:]
    else:
        out[-delta:] = values[:delta]
    return out


def _shift_gates(values: np.ndarray, delta: int) -> np.ndarray:
    if delta == 0:
        return values
    out = np.full_like(values, np.nan)
    if delta > 0:
        out[:, :-delta] = values[:, delta:]
    else:
        out[:, -delta:] = values[:, :delta]
    return out


def boundary_pairs(field: Field) -> int:
    """Metric 1: adjacent finite 4-neighbor pairs (incl. azimuth wrap seam)
    with |dv| > 1.2 * min(N_rowA, N_rowB)."""
    v = field.values
    nyq = field.nyq
    count = 0
    # Horizontal (same row): threshold = 1.2 * min(n, n) = 1.2 * n.
    a, b = v[:, :-1], v[:, 1:]
    thr = (BOUNDARY_NYQUIST_FRAC * nyq)[:, None]
    mask = np.isfinite(a) & np.isfinite(b) & np.isfinite(thr)
    count += int(np.count_nonzero(mask & (np.abs(a - b) > thr)))
    # Vertical (adjacent rows): f32::min ignores NaN -> fmin.
    if field.rows > 1:
        a, b = v[:-1, :], v[1:, :]
        pair_nyq = np.fmin(nyq[:-1], nyq[1:])
        thr = (BOUNDARY_NYQUIST_FRAC * pair_nyq)[:, None]
        mask = np.isfinite(a) & np.isfinite(b) & np.isfinite(thr)
        count += int(np.count_nonzero(mask & (np.abs(a - b) > thr)))
        if field.wraps:
            a, b = v[-1, :], v[0, :]
            thr = BOUNDARY_NYQUIST_FRAC * np.fmin(nyq[-1], nyq[0])
            mask = np.isfinite(a) & np.isfinite(b) & np.isfinite(thr)
            count += int(np.count_nonzero(mask & (np.abs(a - b) > thr)))
    return count


def rms_against(field: Field, reference: np.ndarray) -> float | None:
    """Metric 2: RMS of (v - v_hat); subtraction in f32, accumulation in f64."""
    delta = field.values - reference.astype(np.float32)
    mask = np.isfinite(field.values) & np.isfinite(reference)
    if not mask.any():
        return None
    d64 = delta[mask].astype(np.float64)
    return float(np.sqrt(np.mean(d64 * d64)))


def percent_modified(output: Field, raw: Field) -> float:
    """Metric 3: % finite gates moved by more than one Nyquist vs raw."""
    out, src = output.values, raw.values
    both = np.isfinite(out) & np.isfinite(src)
    finite = int(np.count_nonzero(both))
    if finite == 0:
        return 0.0
    nyq = raw.nyq[:, None]
    moved = both & np.isfinite(nyq) & (np.abs(out - src) > nyq)
    return 100.0 * int(np.count_nonzero(moved)) / finite


def _components(flagged: np.ndarray, wraps: bool) -> list[int]:
    """Sizes of 4-connected components of ``flagged``, azimuth seam joined
    when the sweep wraps (union-find over scipy labels)."""
    structure = np.array([[0, 1, 0], [1, 1, 1], [0, 1, 0]], dtype=bool)
    labels, n = ndimage.label(flagged, structure=structure)
    if n == 0:
        return []
    parent = list(range(n + 1))

    def find(x: int) -> int:
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    if wraps and flagged.shape[0] > 1:
        top, bottom = labels[0, :], labels[-1, :]
        for g in np.nonzero((top > 0) & (bottom > 0))[0]:
            ra, rb = find(int(top[g])), find(int(bottom[g]))
            if ra != rb:
                parent[rb] = ra
    sizes = np.bincount(labels.ravel(), minlength=n + 1)
    merged: dict[int, int] = {}
    for label in range(1, n + 1):
        root = find(label)
        merged[root] = merged.get(root, 0) + int(sizes[label])
    return list(merged.values())


def speck_count(field: Field) -> int:
    """Metric 4: 4-connected components (<= 3 gates) of gates more than one
    Nyquist off their finite 8-neighborhood upper median (>= 4 neighbors)."""
    v = field.values
    neighborhoods = []
    for dr in (-1, 0, 1):
        rolled = _shift_rows(v, dr, field.wraps)
        for dg in (-1, 0, 1):
            if dr == 0 and dg == 0:
                continue
            neighborhoods.append(_shift_gates(rolled, dg))
    stack = np.stack(neighborhoods)  # (8, rows, gates)
    n_finite = np.isfinite(stack).sum(axis=0)
    # np.sort puts NaN last, so index n_finite//2 is the Rust upper median.
    ordered = np.sort(stack, axis=0)
    k = (n_finite // 2).astype(np.int64)[None, :, :]
    median = np.take_along_axis(ordered, k, axis=0)[0]
    nyq = field.nyq[:, None]
    flagged = (
        np.isfinite(v)
        & np.isfinite(nyq)
        & (n_finite >= 4)
        & (np.abs(v - median) > nyq)
    )
    return sum(1 for size in _components(flagged, field.wraps) if size <= 3)


def couplet_max_delta(field: Field) -> float | None:
    """Case B spot check: strongest azimuthal gate-to-gate |dV| in the
    15-40 km annulus."""
    first = float(field.first_gate_m)
    spacing = float(max(field.gate_spacing_m, 1))
    gate_lo = int(max(math.ceil((15_000.0 - first) / spacing), 0.0))
    gate_hi = int(math.floor((40_000.0 - first) / spacing))
    gate_hi = min(gate_hi, field.gates - 1)
    if gate_hi < gate_lo or field.rows < 2:
        return None
    sub = field.values[:, gate_lo : gate_hi + 1]
    if field.wraps:
        nxt = np.roll(sub, -1, axis=0)
        a, b = sub, nxt
    else:
        a, b = sub[:-1], sub[1:]
    mask = np.isfinite(a) & np.isfinite(b)
    if not mask.any():
        return None
    return float(np.abs(a - b)[mask].astype(np.float64).max())


def max_inbound(field: Field) -> float | None:
    """Case C spot check: most negative velocity on the tilt."""
    finite = field.values[np.isfinite(field.values)]
    return float(finite.min()) if finite.size else None


def multifold_structure(output: Field, raw: Field) -> tuple[int, int]:
    """Case C spot check: gates moved by |fold| >= 2 (|out - raw| > 3N) and
    how many of their 4-connected components are smaller than 32 gates."""
    nyq = raw.nyq[:, None]
    flagged = (
        np.isfinite(output.values)
        & np.isfinite(raw.values)
        & np.isfinite(nyq)
        & (np.abs(output.values - raw.values) > np.float32(3.0) * nyq)
    )
    sizes = _components(flagged, output.wraps)
    return int(np.count_nonzero(flagged)), sum(1 for size in sizes if size < 32)


def probe_mean(field: Field, azimuth_deg: float, range_km: float) -> float | None:
    """Metric 5: 5x5-gate mean around the nearest (azimuth, range) gate."""
    target = np.float32(azimuth_deg % 360.0)
    az = np.mod(field.azimuth_deg, np.float32(360.0))
    dist = np.abs(np.mod(az - target + np.float32(180.0), np.float32(360.0)) - np.float32(180.0))
    dist = np.where(np.isfinite(az), dist, np.inf)
    row = int(np.argmin(dist))  # first minimum, like Iterator::min_by
    range_m = np.float32(range_km * 1000.0)
    gate_f = (range_m - np.float32(field.first_gate_m)) / np.float32(max(field.gate_spacing_m, 1))
    gate_f = float(gate_f)
    gate = math.floor(gate_f + 0.5) if gate_f >= 0 else -math.floor(-gate_f + 0.5)
    if gate < 0 or gate >= field.gates:
        return None
    total, count = 0.0, 0
    for dr in range(-2, 3):
        r = row + dr
        if field.wraps:
            r %= field.rows
        elif r < 0 or r >= field.rows:
            continue
        for dg in range(-2, 3):
            g = gate + dg
            if g < 0 or g >= field.gates:
                continue
            value = field.values[r, g]
            if np.isfinite(value):
                total += float(value)
                count += 1
    return total / count if count else None


def correct_branch_percent(field: Field, truth: np.ndarray) -> float | None:
    """Case E: % gates with |v - truth| < 6 m/s (both finite)."""
    mask = np.isfinite(field.values) & np.isfinite(truth)
    counted = int(np.count_nonzero(mask))
    if counted == 0:
        return None
    good = np.abs(field.values - truth.astype(np.float32)) < CORRECT_BRANCH_TOLERANCE_MPS
    return 100.0 * int(np.count_nonzero(mask & good)) / counted


# ---- Browning & Wexler (1968) per-range-band harmonic reference ----


def fit_range_band_reference(field: Field) -> list[tuple[float, float] | None]:
    """Port of render2d's fit_range_band_reference: two-pass per-band
    least-squares of v(az) = a cos(az) + b sin(az), 16-gate bands, second
    pass trims samples > 12 m/s off the first fit."""
    rows, gates = field.rows, field.gates
    bands = max(-(-gates // REFERENCE_BAND_GATES), 1)
    az32 = field.azimuth_deg
    valid_row = np.isfinite(az32)
    az64 = az32.astype(np.float64)
    sin64, cos64 = np.sin(np.radians(az64)), np.cos(np.radians(az64))
    sin32, cos32 = sin64.astype(np.float32), cos64.astype(np.float32)
    sector = (np.mod(az32, np.float32(360.0)) / np.float32(30.0)).astype(np.uint32) % 12

    fits: list[tuple[float, float] | None] = [None] * bands
    for _pass in range(2):
        new_fits: list[tuple[float, float] | None] = [None] * bands
        for band in range(bands):
            sub = field.values[:, band * REFERENCE_BAND_GATES : (band + 1) * REFERENCE_BAND_GATES]
            fin = np.isfinite(sub) & valid_row[:, None]
            if _pass == 1 and fits[band] is not None:
                a32, b32 = np.float32(fits[band][0]), np.float32(fits[band][1])
                predicted = a32 * cos32 + b32 * sin32  # f32, per row
                keep = ~(np.abs(sub - predicted[:, None]) > FIT_TRIM_MPS)
                fin = fin & keep
            n_row = fin.sum(axis=1).astype(np.float64)
            n = float(n_row.sum())
            if int(n) < FIT_MIN_SAMPLES:
                continue
            contributing = n_row > 0
            if len(np.unique(sector[contributing])) < FIT_MIN_SECTORS:
                continue
            cc = float((cos64 * cos64 * n_row).sum())
            cs = float((cos64 * sin64 * n_row).sum())
            ss = float((sin64 * sin64 * n_row).sum())
            row_v = np.where(fin, sub.astype(np.float64), 0.0).sum(axis=1)
            cv = float((cos64 * row_v).sum())
            sv = float((sin64 * row_v).sum())
            det = cc * ss - cs * cs
            if abs(det) < 1e-6:
                continue
            a = (cv * ss - sv * cs) / det
            b = (sv * cc - cv * cs) / det
            new_fits[band] = (float(np.float32(a)), float(np.float32(b)))
        fits = new_fits
    return fits


def harmonic_rms(field: Field) -> float | None:
    """Metric 2b: RMS of the field against its OWN Browning & Wexler fit."""
    if not np.isfinite(field.azimuth_deg).all():
        return None  # Rust `?` aborts on any missing azimuth
    fits = fit_range_band_reference(field)
    az64 = field.azimuth_deg.astype(np.float64)
    sin32 = np.sin(np.radians(az64)).astype(np.float32)
    cos32 = np.cos(np.radians(az64)).astype(np.float32)
    total, count = 0.0, 0
    for band, fit in enumerate(fits):
        if fit is None:
            continue
        a32, b32 = np.float32(fit[0]), np.float32(fit[1])
        predicted = a32 * cos32 + b32 * sin32  # f32 per row
        sub = field.values[:, band * REFERENCE_BAND_GATES : (band + 1) * REFERENCE_BAND_GATES]
        delta = (sub - predicted[:, None])[np.isfinite(sub)].astype(np.float64)
        total += float((delta * delta).sum())
        count += delta.size
    if count == 0:
        return None
    return math.sqrt(total / count)


# ---- environmental wind projection (render2d dealias_v4/env_profile.rs) ----


def beam_height_above_radar_m(slant_range_m: np.ndarray, elevation_deg: float) -> np.ndarray:
    """Doviak & Zrnic (1993) eq. 2.28b, 4/3-effective-earth model."""
    ae = EFFECTIVE_EARTH_RADIUS_M
    r = slant_range_m
    theta = math.radians(elevation_deg)
    return np.sqrt(r * r + ae * ae + 2.0 * r * ae * math.sin(theta)) - ae


def project_environmental_winds(
    levels: list[tuple[float, float, float]],
    elevation_deg: float,
    azimuth_deg: np.ndarray,
    first_gate_m: float,
    gate_spacing_m: float,
    gates: int,
) -> np.ndarray:
    """v_hat_r = (u sin az + v cos az) cos(elev); wind linearly interpolated
    in 4/3-earth beam height, clamped at profile ends (f32 arithmetic like
    the Rust projection)."""
    rows = len(azimuth_deg)
    heights = np.array([lvl[0] for lvl in levels], dtype=np.float32)
    us = np.array([lvl[1] for lvl in levels], dtype=np.float32)
    vs = np.array([lvl[2] for lvl in levels], dtype=np.float32)
    slant = np.maximum(first_gate_m + np.arange(gates, dtype=np.float64) * gate_spacing_m, 0.0)
    h32 = beam_height_above_radar_m(slant, elevation_deg).astype(np.float32)
    # Linear interpolation with end clamping, in f32 like EnvironmentalWindProfile::wind_at.
    idx = np.searchsorted(heights, h32, side="right")
    idx = np.clip(idx, 1, len(heights) - 1)
    lo_h, hi_h = heights[idx - 1], heights[idx]
    span = hi_h - lo_h
    t = np.where(span > 0, (h32 - lo_h) / span, np.float32(0.0)).astype(np.float32)
    gu = (us[idx - 1] + t * (us[idx] - us[idx - 1])).astype(np.float32)
    gv = (vs[idx - 1] + t * (vs[idx] - vs[idx - 1])).astype(np.float32)
    below = h32 <= heights[0]
    above = h32 >= heights[-1]
    gu = np.where(below, us[0], np.where(above, us[-1], gu))
    gv = np.where(below, vs[0], np.where(above, vs[-1], gv))
    cos_elev = np.float32(math.cos(math.radians(float(elevation_deg))))
    az64 = azimuth_deg.astype(np.float64)
    sin32 = np.sin(np.radians(az64)).astype(np.float32)
    cos32 = np.cos(np.radians(az64)).astype(np.float32)
    out = (gu[None, :] * sin32[:, None] + gv[None, :] * cos32[:, None]) * cos_elev
    out = out.astype(np.float32)
    bad = ~np.isfinite(azimuth_deg)
    if bad.any():
        out[bad, :] = np.nan
    assert out.shape == (rows, gates)
    return out


# ---- Rust --dump-fields loaders ----


def load_dumped_field(dump_dir: Path, label: str, cut_index: int) -> Field:
    """Read one `<label>_cutNN` (.json meta + .bin f32-LE values) pair."""
    stem = dump_dir / f"{label}_cut{cut_index:02d}"
    meta = json.loads(stem.with_suffix(".json").read_text())
    rows, gates = meta["rows"], meta["gates"]
    values = np.fromfile(stem.with_suffix(".bin"), dtype="<f4").reshape(rows, gates)
    to_f32 = lambda seq: np.array(
        [np.nan if x is None else x for x in seq], dtype=np.float32
    )
    return Field(
        rows=rows,
        gates=gates,
        wraps=meta["wraps"],
        values=values,
        nyq=to_f32(meta["nyquist_mps"]),
        azimuth_deg=to_f32(meta["azimuth_deg"]),
        first_gate_m=meta["first_gate_m"],
        gate_spacing_m=meta["gate_spacing_m"],
        elevation_deg=meta["elevation_deg"],
    )


def load_dumped_bin(dump_dir: Path, name: str, rows: int, gates: int) -> np.ndarray:
    return np.fromfile(dump_dir / name, dtype="<f4").reshape(rows, gates)


def score_field(
    output: Field,
    raw: Field,
    env_projection: np.ndarray | None,
    lowest: bool,
    probes: list[tuple[str, float, float]] | None = None,
    truth: np.ndarray | None = None,
) -> dict:
    """Compute the full section-10.2 metric set for one tilt (mirrors
    evaluate_engine's per-cut block)."""
    report: dict = {"boundaries": boundary_pairs(output)}
    if lowest:
        report["specks_lowest"] = speck_count(output)
        report["percent_modified"] = percent_modified(output, raw)
        report["couplet_max_dv"] = couplet_max_delta(output)
        report["max_inbound"] = max_inbound(output)
        gates_moved, speckle = multifold_structure(output, raw)
        report["multifold_gates"] = gates_moved
        report["multifold_speckle"] = speckle
        report["rms_env"] = (
            rms_against(output, env_projection) if env_projection is not None else None
        )
        report["rms_harmonic"] = harmonic_rms(output)
        if probes:
            report["probes"] = [
                {"label": label, "mean": probe_mean(output, az, km)}
                for (label, az, km) in probes
            ]
        if truth is not None:
            report["correct_branch_percent"] = correct_branch_percent(output, truth)
    return report
