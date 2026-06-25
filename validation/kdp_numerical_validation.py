#!/usr/bin/env python3
"""Independent numerical check of the Rust KDP algorithm's equations.

This mirrors the unwrap -> short-gap interpolation -> Hampel -> robust local
linear-fit path. It is not a substitute for `cargo test`; it exists so the
patch bundle contains reproducible numerical evidence even in environments
without a Rust toolchain.
"""
from __future__ import annotations

import json
import math
from pathlib import Path

import numpy as np


def unwrap(values: np.ndarray, period: float = 360.0) -> np.ndarray:
    out = values.copy()
    previous = None
    for i, value in enumerate(out):
        if not np.isfinite(value):
            continue
        if previous is not None:
            value -= round((value - previous) / period) * period
            out[i] = value
        previous = value
    return out


def fill_short_gaps(values: np.ndarray, max_gap: int = 2) -> np.ndarray:
    out = values.copy()
    i = 0
    while i < len(out):
        if np.isfinite(out[i]):
            i += 1
            continue
        start = i
        while i < len(out) and not np.isfinite(out[i]):
            i += 1
        n = i - start
        if start == 0 or i >= len(out) or n > max_gap:
            continue
        left, right = out[start - 1], out[i]
        if np.isfinite(left) and np.isfinite(right):
            for off in range(n):
                f = (off + 1) / (n + 1)
                out[start + off] = left + f * (right - left)
    return out


def hampel(values: np.ndarray, half_window: int = 3, threshold: float = 3.0) -> np.ndarray:
    out = values.copy()
    for i, value in enumerate(values):
        if not np.isfinite(value):
            continue
        neighborhood = values[max(0, i-half_window):min(len(values), i+half_window+1)]
        neighborhood = neighborhood[np.isfinite(neighborhood)]
        if len(neighborhood) == 0:
            continue
        median = float(np.median(neighborhood))
        mad = float(np.median(np.abs(neighborhood - median)))
        sigma = 1.4826 * mad
        if sigma > 1e-4 and abs(value - median) > threshold * sigma:
            out[i] = median
    return out


def weighted_fit(x: np.ndarray, y: np.ndarray, w: np.ndarray) -> tuple[float, float] | None:
    sw = np.sum(w)
    sx = np.sum(x*w)
    sy = np.sum(y*w)
    sxx = np.sum(x*x*w)
    sxy = np.sum(x*y*w)
    denominator = sw*sxx - sx*sx
    if sw <= 0 or abs(denominator) <= 1e-12:
        return None
    slope = (sw*sxy - sx*sy) / denominator
    intercept = (sy - slope*sx) / sw
    return float(slope), float(intercept)


def robust_fit(x: np.ndarray, y: np.ndarray, huber_k: float = 1.5) -> tuple[float, float] | None:
    w = np.ones(len(x))
    fit = None
    for _ in range(3):
        fit = weighted_fit(x, y, w)
        if fit is None:
            return None
        slope, intercept = fit
        residual = y - (intercept + slope*x)
        median_residual = np.median(residual)
        sigma = 1.4826*np.median(np.abs(residual-median_residual))
        if not np.isfinite(sigma) or sigma <= 1e-8:
            break
        cutoff = max(huber_k, .1)*sigma
        magnitude = np.abs(residual)
        w = np.where(magnitude <= cutoff, 1.0, cutoff/np.maximum(magnitude, 1e-12))
    return fit


def retrieve(phi_wrapped: np.ndarray, spacing_km: float, original_valid: np.ndarray | None = None) -> np.ndarray:
    if original_valid is None:
        original_valid = np.isfinite(phi_wrapped)
    phase = hampel(fill_short_gaps(unwrap(phi_wrapped)))
    window_gates = round(3.0/spacing_km)
    window_gates = min(max(window_gates, 7), 41)
    if window_gates % 2 == 0:
        window_gates += 1
    out = np.full(len(phase), np.nan)
    half = window_gates//2
    for gate in range(len(phase)):
        if not original_valid[gate]:
            continue
        start, end = max(0, gate-half), min(len(phase), gate+half+1)
        idx = np.arange(start, end)
        valid = np.isfinite(phase[start:end])
        if valid.sum() < 5:
            continue
        x = (idx[valid]-gate)*spacing_km
        y = phase[start:end][valid]
        fit = robust_fit(x, y)
        if fit is None:
            continue
        kdp = 0.5*fit[0]
        if -2 <= kdp <= 14:
            out[gate] = kdp
    return out


def metrics(expected: float, estimate: np.ndarray, margin: int = 10) -> dict[str, float | int]:
    core = estimate[margin:len(estimate)-margin]
    core = core[np.isfinite(core)]
    error = core - expected
    return {
        "valid_gate_count": int(len(core)),
        "median_deg_per_km": float(np.median(core)),
        "bias_deg_per_km": float(np.mean(error)),
        "rmse_deg_per_km": float(np.sqrt(np.mean(error**2))),
        "p95_abs_error_deg_per_km": float(np.percentile(np.abs(error), 95)),
    }


def main() -> None:
    rng = np.random.default_rng(260026)
    spacing_km = 0.25
    gates = 201
    r = np.arange(gates)*spacing_km

    exact_expected = 2.0
    exact_phi = np.mod(350.0 + 2*exact_expected*r, 360.0)
    exact = retrieve(exact_phi, spacing_km)

    noisy_expected = 1.5
    noisy_unwrapped = 330.0 + 2*noisy_expected*r + rng.normal(0.0, 0.8, gates)
    noisy_unwrapped[[35, 90, 150]] += [35.0, -45.0, 55.0]
    noisy = np.mod(noisy_unwrapped, 360.0)
    original_valid = np.ones(gates, dtype=bool)
    original_valid[[60, 61, 125]] = False
    noisy[[60, 61, 125]] = np.nan
    noisy_estimate = retrieve(noisy, spacing_km, original_valid)

    flat_expected = 0.0
    flat_phi = np.mod(120.0 + rng.normal(0.0, 0.3, gates), 360.0)
    flat = retrieve(flat_phi, spacing_km)

    report = {
        "algorithm": "unwrap + <=2-gate interpolation + Hampel + 3 km Huber local slope; KDP=0.5*dPHIDP/dr",
        "cases": {
            "wrapped_linear_2_deg_per_km": metrics(exact_expected, exact),
            "noisy_spiky_gappy_1_5_deg_per_km": metrics(noisy_expected, noisy_estimate),
            "flat_phase_0_deg_per_km": metrics(flat_expected, flat),
        },
    }
    out = Path(__file__).with_name("kdp_numerical_validation.json")
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
