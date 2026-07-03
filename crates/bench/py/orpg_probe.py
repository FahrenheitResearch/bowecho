"""Spot-check what NOAA's OPERATIONAL dealiaser (the WSR-88D ORPG velocity
dealiasing algorithm) decided at the battery's probe points, from archived
Level-III products.

Two modes, honest about their very different evidentiary weight:

1. ``--nids <file>``: a raw Level-III digital base velocity product
   (product 99, N0U family; 0.25 km x 1 deg, 256 levels), read with Py-ART.
   QUANTITATIVE: reports the same 5x5-gate probe means, couplet max dV and
   max inbound the battery uses.  Resolution differs from super-res
   Level-II (1 deg vs 0.5 deg azimuth), so gate-count metrics (boundary
   pairs, specks) are deliberately NOT reported - spot checks only.
   Sources: unidata-nexrad-level3 S3 bucket (~2020-03 to ~2023) and the
   NCEI archive mirrored at gs://gcp-public-data-nexrad-l3 (1992 to ~2025-12,
   daily per-site .tar.Z).

2. ``--ridge-png <png>``: an IEM-archived RIDGE render of the N0S
   storm-relative mean velocity product (16-level), georeferenced by its
   ESRI world file.  QUALITATIVE ONLY: raw 2026 Level-III is not publicly
   archived anywhere we could find (NCEI order-only), so the only public
   record of the ORPG's 2026 branch decisions is this render.  The check
   classifies the probe pixel as TOWARD (green family) / AWAY (red family)
   / no-data, which is decisive for branch questions where the candidate
   branches differ by ~2N ~ 52 m/s >> any plausible storm-motion offset
   (N0S subtracts storm motion, which the PNG does not record).

Usage:
  python orpg_probe.py --nids LIX_N0U_2021_08_29_16_32_52 --probe 200,10,eyewall --annulus
  python orpg_probe.py --ridge-png MBX_N0S_202606092354.png --site-lat 48.3925 \
      --site-lon -100.8644 --probe 339,20,blob --probe 316,21,sector
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import warnings
from pathlib import Path

import numpy as np

warnings.filterwarnings("ignore")

EARTH_RADIUS_M = 6_371_000.0


def parse_probe(spec: str) -> tuple[str, float, float]:
    parts = spec.split(",")
    az, km = float(parts[0]), float(parts[1])
    return (parts[2] if len(parts) > 2 else f"az{az}/r{km}km", az, km)


# ---- mode 1: raw NIDS ----


def nids_report(path: Path, probes, annulus: bool, couplet: bool) -> dict:
    import pyart

    radar = pyart.io.read_nexrad_level3(str(path))
    field_name = next(iter(radar.fields))
    data = np.ma.filled(radar.fields[field_name]["data"], np.nan).astype(np.float32)
    az = radar.azimuth["data"].astype(np.float32)
    rng = radar.range["data"]
    first, spacing = float(rng[0]), float(rng[1] - rng[0])
    import dealias_metrics as dm

    field = dm.Field(
        rows=data.shape[0],
        gates=data.shape[1],
        wraps=dm.sweep_wraps(az),
        values=data,
        nyq=np.full(data.shape[0], np.nan, np.float32),  # L3 carries no Nyquist
        azimuth_deg=az,
        first_gate_m=first,
        gate_spacing_m=spacing,
        elevation_deg=float(radar.fixed_angle["data"][0]),
    )
    out = {
        "file": path.name,
        "product": field_name,
        "elevation_deg": field.elevation_deg,
        "rays": field.rows,
        "gates": field.gates,
        "gate_spacing_m": spacing,
        "probes": [
            {"label": label, "mean": dm.probe_mean(field, a, km)}
            for (label, a, km) in probes
        ],
    }
    if annulus:
        out["max_inbound"] = dm.max_inbound(field)
    if couplet:
        out["couplet_max_dv_15_40km"] = dm.couplet_max_delta(field)
    return out


# ---- mode 2: RIDGE render ----


def destination_point(lat0: float, lon0: float, bearing_deg: float, distance_m: float):
    """Great-circle destination on a sphere (adequate: RIDGE pixels are ~600 m)."""
    delta = distance_m / EARTH_RADIUS_M
    theta = math.radians(bearing_deg)
    phi1, lam1 = math.radians(lat0), math.radians(lon0)
    phi2 = math.asin(
        math.sin(phi1) * math.cos(delta) + math.cos(phi1) * math.sin(delta) * math.cos(theta)
    )
    lam2 = lam1 + math.atan2(
        math.sin(theta) * math.sin(delta) * math.cos(phi1),
        math.cos(delta) - math.sin(phi1) * math.sin(phi2),
    )
    return math.degrees(phi2), math.degrees(lam2)


def classify_rgb(r: float, g: float, b: float, alpha: float) -> str:
    if alpha < 0.5 or (r > 0.9 and g > 0.9 and b > 0.9):
        return "no-data"
    if r < 0.2 and g < 0.2 and b < 0.2:
        return "no-data"
    if b > 0.6 and r > 0.5 and g < 0.5:
        return "range-folded(purple)"
    if g > r:
        return "TOWARD"
    return "AWAY"


def ridge_report(png: Path, site_lat: float, site_lon: float, probes) -> dict:
    from matplotlib.image import imread

    image = imread(png)  # (h, w, 4) float
    world = [float(x) for x in png.with_suffix(".wld").read_text().split()]
    dx, _, _, dy, x0, y0 = world  # lon-per-px, 0, 0, lat-per-px(<0), lon of px0, lat of px0
    out = {"file": png.name, "world_file": world, "probes": []}
    for label, az, km in probes:
        lat, lon = destination_point(site_lat, site_lon, az, km * 1000.0)
        col = int(round((lon - x0) / dx))
        row = int(round((lat - y0) / dy))
        votes: dict[str, int] = {}
        sample_rgb = None
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                rr, cc = row + dr, col + dc
                if 0 <= rr < image.shape[0] and 0 <= cc < image.shape[1]:
                    px = image[rr, cc]
                    alpha = float(px[3]) if px.shape[0] > 3 else 1.0
                    verdict = classify_rgb(float(px[0]), float(px[1]), float(px[2]), alpha)
                    votes[verdict] = votes.get(verdict, 0) + 1
                    if dr == 0 and dc == 0:
                        sample_rgb = [round(float(v), 3) for v in px[:4]]
        data_votes = {k: v for k, v in votes.items() if k != "no-data"}
        verdict = (
            max(data_votes, key=data_votes.get) if data_votes else "no-data"
        )
        out["probes"].append(
            {
                "label": label,
                "lat": round(lat, 4),
                "lon": round(lon, 4),
                "pixel": [row, col],
                "center_rgba": sample_rgb,
                "votes_3x3": votes,
                "verdict": verdict,
            }
        )
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nids", type=Path)
    parser.add_argument("--ridge-png", type=Path)
    parser.add_argument("--site-lat", type=float)
    parser.add_argument("--site-lon", type=float)
    parser.add_argument("--probe", action="append", default=[])
    parser.add_argument("--annulus", action="store_true", help="report max inbound")
    parser.add_argument("--couplet", action="store_true", help="report 15-40 km couplet max dV")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    probes = [parse_probe(p) for p in args.probe]

    if args.nids:
        report = nids_report(args.nids, probes, args.annulus, args.couplet)
    elif args.ridge_png:
        if args.site_lat is None or args.site_lon is None:
            parser.error("--ridge-png needs --site-lat/--site-lon")
        report = ridge_report(args.ridge_png, args.site_lat, args.site_lon, probes)
    else:
        parser.error("one of --nids / --ridge-png required")
        return 2

    text = json.dumps(report, indent=1)
    if args.out:
        args.out.write_text(text, encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
