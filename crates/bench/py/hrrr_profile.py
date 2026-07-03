"""Extract an ``EnvironmentalWindProfile`` fixture from an archived HRRR 3-km
0-hour analysis (NOAA Big Data Program bucket ``noaa-hrrr-bdp-pds``).

Produces the same fixture schema as the existing RAP fixtures in
``crates/bench/fixtures/dealias/env_*.json`` so ``bowecho-bench --dealias
--env`` can consume it directly (spec section 4a).  These fixtures are the
RESOLUTION EXPERIMENT behind the amended section-16 owner decision: v4(HRRR)
proved metric-for-metric indistinguishable from v4(RAP) on the battery (see
docs/dealias-external-baselines.md section 3), so production anchoring is
RAP-only; the HRRR rows stay as the measured justification.

Method (mirrors the RAP fixtures' documented convention):
- fetch ONLY the needed GRIB records via ``.idx`` byte-range requests
  (UGRD/VGRD/HGT on all isobaric levels + 10-m UGRD/VGRD), not the ~700 MB
  file;
- nearest HRRR grid point to the radar site (site coordinates read from the
  case volume itself via Py-ART so lat/lon/antenna-ASL match the data);
- height above radar level = geopotential height (gpm) - antenna ASL;
  pressure levels below/at the antenna are dropped;
- the 10-m AGL wind is pinned at 0.5 m ARL as the near-surface anchor,
  exactly like the RAP fixtures (the render2d projection clamps to the
  lowest level below it);
- the HRRR archive begins 2014-09-30: pre-era cases (KTLX 2013) CANNOT have
  an HRRR fixture and stay RAP-only.

Usage:
  python hrrr_profile.py --volume KEAX20260609_055143_V06 \
      --cycle 2026-06-09T06 --out ../fixtures/dealias/env_keax_hrrr.json
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import tempfile
import warnings
from pathlib import Path

import numpy as np
import requests

warnings.filterwarnings("ignore")

BUCKET = "https://noaa-hrrr-bdp-pds.s3.amazonaws.com"


def fetch_idx(cycle: dt.datetime) -> tuple[str, list[str]]:
    key = f"hrrr.{cycle:%Y%m%d}/conus/hrrr.t{cycle:%H}z.wrfprsf00.grib2"
    url = f"{BUCKET}/{key}"
    response = requests.get(url + ".idx", timeout=60)
    response.raise_for_status()
    return url, response.text.splitlines()


def wanted(entry_var: str, entry_level: str) -> bool:
    if entry_var in ("UGRD", "VGRD") and entry_level == "10 m above ground":
        return True
    return entry_var in ("UGRD", "VGRD", "HGT") and entry_level.endswith(" mb")


def subset_ranges(idx_lines: list[str]) -> list[tuple[int, int | None, str, str]]:
    """(start, end-exclusive-or-None, var, level) for every wanted record."""
    parsed = []
    for line in idx_lines:
        parts = line.split(":")
        parsed.append((int(parts[1]), parts[3], parts[4]))
    out = []
    for i, (offset, var, level) in enumerate(parsed):
        if wanted(var, level):
            end = parsed[i + 1][0] if i + 1 < len(parsed) else None
            out.append((offset, end, var, level))
    return out


def download_subset(url: str, ranges: list[tuple[int, int | None, str, str]], dest: Path) -> None:
    with dest.open("wb") as handle:
        for start, end, var, level in ranges:
            header = {"Range": f"bytes={start}-{'' if end is None else end - 1}"}
            response = requests.get(url, headers=header, timeout=300)
            response.raise_for_status()
            handle.write(response.content)
    print(f"fetched {len(ranges)} records, {dest.stat().st_size/1e6:.1f} MB", flush=True)


def nearest_index(lats: np.ndarray, lons: np.ndarray, lat0: float, lon0: float) -> tuple[int, int]:
    lons = np.where(lons > 180.0, lons - 360.0, lons)
    scale = np.cos(np.radians(lat0))
    d2 = (lats - lat0) ** 2 + (scale * (lons - lon0)) ** 2
    j, i = np.unravel_index(np.argmin(d2), d2.shape)
    return int(j), int(i)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--volume", required=True, type=Path, help="case Level-II volume (site coords source)")
    parser.add_argument("--cycle", required=True, help="HRRR cycle, e.g. 2026-06-09T06")
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument(
        "--at",
        default=None,
        help="lat,lon override: extract the profile at this point instead of "
        "the radar site (diagnostic for the spec's 2-D environmental-fields "
        "question; heights stay ARL vs the radar antenna)",
    )
    args = parser.parse_args()

    cycle = dt.datetime.strptime(args.cycle, "%Y-%m-%dT%H")
    hrrr_era = dt.datetime(2014, 9, 30)
    if cycle < hrrr_era:
        print(f"cycle {cycle} predates the HRRR archive ({hrrr_era:%Y-%m-%d}); RAP-only case")
        return 3

    import pyart  # deferred: slow import

    radar = pyart.io.read_nexrad_archive(str(args.volume))
    site = radar.metadata.get("instrument_name") or "UNKNOWN"
    if isinstance(site, bytes):
        site = site.decode()
    lat0 = float(radar.latitude["data"][0])
    lon0 = float(radar.longitude["data"][0])
    antenna_asl = float(radar.altitude["data"][0])
    point_note = ""
    if args.at:
        lat0, lon0 = (float(x) for x in args.at.split(","))
        point_note = f" profile point OVERRIDDEN to ({lat0:.4f}, {lon0:.4f});"
    print(f"site {site} lat {lat0:.4f} lon {lon0:.4f} antenna {antenna_asl:.0f} m ASL", flush=True)

    url, idx_lines = fetch_idx(cycle)
    ranges = subset_ranges(idx_lines)
    with tempfile.TemporaryDirectory() as tmp:
        grib = Path(tmp) / "subset.grib2"
        download_subset(url, ranges, grib)

        import xarray as xr

        def open_filtered(**filter_by_keys):
            return xr.open_dataset(
                grib,
                engine="cfgrib",
                decode_timedelta=True,
                backend_kwargs={
                    "filter_by_keys": filter_by_keys,
                    "indexpath": "",
                },
            )

        iso_u = open_filtered(typeOfLevel="isobaricInhPa", shortName="u")
        iso_v = open_filtered(typeOfLevel="isobaricInhPa", shortName="v")
        iso_gh = open_filtered(typeOfLevel="isobaricInhPa", shortName="gh")
        sfc10 = open_filtered(typeOfLevel="heightAboveGround", level=10)

        j, i = nearest_index(
            iso_u.latitude.values, iso_u.longitude.values, lat0, lon0
        )
        grid_lat = float(iso_u.latitude.values[j, i])
        grid_lon = float(iso_u.longitude.values[j, i])
        grid_lon = grid_lon - 360.0 if grid_lon > 180.0 else grid_lon

        pressures = iso_u.isobaricInhPa.values
        u_col = iso_u.u.values[:, j, i]
        v_col = iso_v.v.values[:, j, i]
        gh_col = iso_gh.gh.values[:, j, i]
        u10 = float(sfc10.u10.values[j, i])
        v10 = float(sfc10.v10.values[j, i])

    levels = [{"height_m_arl": 0.5, "u_mps": round(u10, 2), "v_mps": round(v10, 2)}]
    order = np.argsort(-pressures)  # high pressure (low height) first
    for k in order:
        height_arl = float(gh_col[k]) - antenna_asl
        if height_arl <= levels[-1]["height_m_arl"]:
            continue
        levels.append(
            {
                "height_m_arl": round(height_arl, 1),
                "u_mps": round(float(u_col[k]), 2),
                "v_mps": round(float(v_col[k]), 2),
            }
        )

    fixture = {
        "source": (
            f"HRRR 3-km 0-h analysis {cycle:%Y-%m-%d %H}Z (noaa-hrrr-bdp-pds S3: "
            f"hrrr.{cycle:%Y%m%d}/conus/hrrr.t{cycle:%H}z.wrfprsf00.grib2, .idx byte-range "
            f"subset of UGRD/VGRD/HGT isobaric + 10 m winds),{point_note} nearest grid point "
            f"({grid_lat:.4f}, {grid_lon:.4f}) to {site}; heights ARL vs antenna "
            f"{antenna_asl:.0f} m ASL (from the volume header); 10 m AGL wind pinned at 0.5 m ARL"
        ),
        "site": site[:4],
        "valid_time": f"{cycle:%Y-%m-%dT%H:%M:%S}Z",
        "levels": levels,
    }
    args.out.write_text(json.dumps(fixture, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {args.out} with {len(levels)} levels "
          f"(top {levels[-1]['height_m_arl']:.0f} m ARL)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
