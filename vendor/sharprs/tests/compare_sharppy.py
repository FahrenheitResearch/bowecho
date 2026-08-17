#!/usr/bin/env python3
"""
Compare SHARPpy (Python) and sharprs (Rust) on identical sounding data.

This script:
1. Creates five canonical soundings in SHARPpy
2. Computes all thermodynamic/kinematic parameters via SHARPpy
3. Invokes sharprs (when available) on the same data
4. Prints a side-by-side comparison table
5. Flags any differences exceeding the specified tolerances

Requirements:
  - SHARPpy installed or available at /tmp/SHARPpy
  - sharprs built as a cdylib or callable binary (future)

Usage:
  cd /tmp/SHARPpy && PYTHONPATH=/tmp/SHARPpy python C:/work/sharprs/tests/compare_sharppy.py
"""

from __future__ import annotations

import sys
import os
from dataclasses import dataclass, field
from typing import Optional

# Ensure SHARPpy is importable
SHARPPY_PATH = os.environ.get("SHARPPY_PATH", "/tmp/SHARPpy")
if SHARPPY_PATH not in sys.path:
    sys.path.insert(0, SHARPPY_PATH)

import numpy as np
import numpy.ma as ma
from sharppy.sharptab import thermo, utils, params, winds, interp, profile


# =========================================================================
# Tolerances
# =========================================================================

@dataclass
class Tolerances:
    """Acceptable differences between SHARPpy and sharprs."""
    cape_pct: float = 5.0      # CAPE/CIN: 5% relative
    lcl_hpa: float = 5.0       # LCL/LFC/EL: 5 hPa absolute
    shear_ms: float = 1.0      # Bulk shear: 1 m/s
    srh_m2s2: float = 10.0     # SRH: 10 m2/s2
    composite: float = 0.5     # STP/SCP/SHIP: 0.5 absolute
    bunkers_ms: float = 1.0    # Bunkers: 1 m/s
    k_tt_c: float = 1.0        # K-index, TT: 1 C
    pwat_mm: float = 1.0       # PWAT: 1 mm


TOL = Tolerances()


# =========================================================================
# Sounding definitions
# =========================================================================

@dataclass
class Sounding:
    name: str
    pres: np.ndarray
    hght: np.ndarray
    tmpc: np.ndarray
    dwpc: np.ndarray
    wdir: np.ndarray
    wspd: np.ndarray


def make_soundings() -> list[Sounding]:
    """Return the five canonical test soundings."""

    s1 = Sounding(
        name="Classic Supercell",
        pres=np.array([963, 950, 925, 900, 850, 800, 750, 700, 650, 600, 550, 500,
                       450, 400, 350, 300, 250, 200, 150], dtype=float),
        hght=np.array([350, 470, 714, 950, 1457, 2010, 2618, 3293, 4050, 4907, 5887, 7020,
                       8350, 9940, 11870, 14230, 17190, 20960, 25850], dtype=float),
        tmpc=np.array([28.0, 26.8, 24.0, 21.2, 16.0, 11.0, 5.8, 0.0, -5.8, -12.0, -19.0,
                       -27.0, -36.2, -46.5, -57.8, -68.5, -68.0, -57.0, -58.0], dtype=float),
        dwpc=np.array([19.0, 18.0, 16.0, 14.0, 7.0, 0.0, -6.0, -14.0, -20.0, -28.0, -35.0,
                       -42.0, -50.0, -58.0, -66.0, -72.0, -73.0, -62.0, -63.0], dtype=float),
        wdir=np.array([160, 165, 175, 185, 200, 215, 225, 235, 240, 245, 250, 255,
                       265, 270, 275, 280, 285, 290, 295], dtype=float),
        wspd=np.array([15, 18, 22, 25, 30, 35, 40, 45, 48, 52, 55, 60,
                       65, 70, 75, 80, 85, 90, 95], dtype=float),
    )

    s2 = Sounding(
        name="Weak/Null",
        pres=np.array([1013, 1000, 950, 900, 850, 800, 750, 700, 650, 600, 550, 500,
                       450, 400, 350, 300, 250, 200, 150], dtype=float),
        hght=np.array([0, 112, 540, 988, 1457, 1949, 2466, 3012, 3590, 4206, 4865, 5574,
                       6344, 7185, 8117, 9164, 10363, 11784, 13608], dtype=float),
        tmpc=np.array([-2.0, -1.0, 2.0, -2.0, -6.0, -10.0, -14.0, -18.0, -22.0, -27.0, -32.0, -38.0,
                       -44.0, -52.0, -58.0, -65.0, -60.0, -55.0, -55.0], dtype=float),
        dwpc=np.array([-4.0, -3.0, -2.0, -8.0, -14.0, -20.0, -25.0, -30.0, -35.0, -40.0, -45.0, -50.0,
                       -55.0, -60.0, -65.0, -70.0, -65.0, -60.0, -60.0], dtype=float),
        wdir=np.array([320, 320, 315, 310, 305, 300, 295, 290, 290, 285, 280, 280,
                       275, 270, 270, 265, 265, 260, 260], dtype=float),
        wspd=np.array([5, 5, 8, 10, 12, 12, 14, 15, 16, 18, 20, 22,
                       24, 25, 26, 28, 30, 32, 35], dtype=float),
    )

    s3 = Sounding(
        name="Elevated Convection",
        pres=np.array([1000, 975, 950, 925, 900, 875, 850, 825, 800, 750, 700, 650,
                       600, 550, 500, 450, 400, 350, 300, 250, 200], dtype=float),
        hght=np.array([200, 420, 660, 910, 1170, 1440, 1720, 2010, 2310, 2950, 3640, 4380,
                       5190, 6070, 7040, 8110, 9310, 10660, 12190, 13980, 16180], dtype=float),
        tmpc=np.array([5.0, 8.0, 10.0, 11.0, 12.0, 10.0, 8.0, 5.0, 2.0, -4.0, -10.0, -16.0,
                       -23.0, -30.0, -38.0, -46.0, -54.0, -60.0, -66.0, -60.0, -55.0], dtype=float),
        dwpc=np.array([2.0, 4.0, 6.0, 8.0, 10.0, 8.0, 6.0, 3.0, 0.0, -6.0, -12.0, -18.0,
                       -28.0, -35.0, -44.0, -52.0, -60.0, -65.0, -70.0, -64.0, -59.0], dtype=float),
        wdir=np.array([180, 185, 190, 195, 200, 210, 220, 230, 235, 240, 245, 250,
                       255, 260, 265, 270, 270, 272, 275, 278, 280], dtype=float),
        wspd=np.array([10, 12, 14, 16, 18, 20, 25, 28, 30, 35, 40, 42,
                       45, 48, 50, 52, 55, 58, 60, 62, 65], dtype=float),
    )

    s4 = Sounding(
        name="Tropical",
        pres=np.array([1010, 1000, 975, 950, 925, 900, 850, 800, 750, 700, 650, 600,
                       550, 500, 450, 400, 350, 300, 250, 200, 150], dtype=float),
        hght=np.array([10, 100, 345, 600, 860, 1130, 1500, 1950, 2430, 2950, 3520, 4140,
                       4810, 5540, 6350, 7250, 8280, 9470, 10850, 12430, 14400], dtype=float),
        tmpc=np.array([28.0, 27.5, 26.0, 24.0, 22.0, 20.0, 16.5, 13.0, 9.0, 5.0, 1.0, -3.5,
                       -8.5, -14.0, -20.0, -27.0, -34.0, -42.0, -52.0, -60.0, -70.0], dtype=float),
        dwpc=np.array([26.0, 25.5, 24.5, 23.0, 21.0, 19.0, 14.5, 10.0, 5.0, 0.0, -5.0, -10.0,
                       -16.0, -22.0, -28.0, -35.0, -42.0, -50.0, -58.0, -65.0, -75.0], dtype=float),
        wdir=np.array([90, 95, 100, 105, 110, 115, 120, 125, 130, 135, 140, 145,
                       150, 160, 170, 180, 190, 200, 210, 220, 230], dtype=float),
        wspd=np.array([8, 8, 10, 10, 10, 10, 12, 12, 12, 14, 14, 15,
                       16, 18, 20, 22, 24, 26, 28, 30, 32], dtype=float),
    )

    s5 = Sounding(
        name="Fire Weather",
        pres=np.array([1013, 1000, 975, 950, 925, 900, 850, 800, 750, 700, 650, 600,
                       550, 500, 450, 400, 350, 300, 250, 200], dtype=float),
        hght=np.array([100, 200, 430, 670, 920, 1180, 1730, 2330, 2980, 3680, 4440, 5280,
                       6190, 7200, 8330, 9600, 11050, 12710, 14640, 16920], dtype=float),
        tmpc=np.array([38.0, 37.0, 35.0, 33.0, 30.0, 27.0, 21.0, 15.0, 9.0, 2.0, -5.0, -12.0,
                       -20.0, -28.0, -37.0, -46.0, -56.0, -62.0, -58.0, -55.0], dtype=float),
        dwpc=np.array([5.0, 4.0, 2.0, 0.0, -3.0, -8.0, -15.0, -22.0, -28.0, -35.0, -40.0, -45.0,
                       -50.0, -55.0, -60.0, -65.0, -70.0, -72.0, -63.0, -60.0], dtype=float),
        wdir=np.array([240, 242, 245, 248, 250, 255, 260, 265, 268, 270, 272, 275,
                       278, 280, 282, 285, 288, 290, 292, 295], dtype=float),
        wspd=np.array([20, 22, 25, 28, 30, 32, 35, 38, 40, 42, 44, 45,
                       46, 48, 50, 52, 54, 56, 58, 60], dtype=float),
    )

    return [s1, s2, s3, s4, s5]


# =========================================================================
# SHARPpy computation
# =========================================================================

@dataclass
class Results:
    """Container for all computed parameters from a sounding."""
    name: str = ""
    sb_cape: float = 0.0
    sb_cin: float = 0.0
    sb_lcl_p: Optional[float] = None
    sb_lfc_p: Optional[float] = None
    sb_el_p: Optional[float] = None
    sb_lcl_h: Optional[float] = None
    ml_cape: float = 0.0
    ml_cin: float = 0.0
    ml_lcl_p: Optional[float] = None
    mu_cape: float = 0.0
    mu_cin: float = 0.0
    k_index: Optional[float] = None
    tt: Optional[float] = None
    pwat: Optional[float] = None  # inches
    shr06: Optional[float] = None  # m/s
    shr01: Optional[float] = None
    bunkers_ru: Optional[float] = None
    bunkers_rv: Optional[float] = None
    bunkers_lu: Optional[float] = None
    bunkers_lv: Optional[float] = None
    srh1: Optional[float] = None
    srh3: Optional[float] = None
    stp: Optional[float] = None
    scp: Optional[float] = None
    ship: Optional[float] = None
    sfc_rh: Optional[float] = None


def safe_float(val) -> Optional[float]:
    """Convert a possibly-masked numpy value to Optional[float]."""
    if type(val) == type(ma.masked):
        return None
    try:
        v = float(val)
        if np.isnan(v):
            return None
        return v
    except (TypeError, ValueError):
        return None


def compute_sharppy(s: Sounding) -> Results:
    """Compute all parameters for a sounding using SHARPpy."""
    r = Results(name=s.name)

    prof = profile.create_profile(
        pres=s.pres, hght=s.hght, tmpc=s.tmpc, dwpc=s.dwpc,
        wdir=s.wdir, wspd=s.wspd, missing=-9999.0,
    )

    # Parcels
    sfcpcl = params.parcelx(prof, flag=1)
    mlpcl = params.parcelx(prof, flag=4)
    mupcl = params.parcelx(prof, flag=3)

    r.sb_cape = safe_float(sfcpcl.bplus) or 0.0
    r.sb_cin = safe_float(sfcpcl.bminus) or 0.0
    r.sb_lcl_p = safe_float(sfcpcl.lclpres)
    r.sb_lfc_p = safe_float(sfcpcl.lfcpres)
    r.sb_el_p = safe_float(sfcpcl.elpres)
    r.sb_lcl_h = safe_float(sfcpcl.lclhght)
    r.ml_cape = safe_float(mlpcl.bplus) or 0.0
    r.ml_cin = safe_float(mlpcl.bminus) or 0.0
    r.ml_lcl_p = safe_float(mlpcl.lclpres)
    r.mu_cape = safe_float(mupcl.bplus) or 0.0
    r.mu_cin = safe_float(mupcl.bminus) or 0.0

    # Indices
    r.k_index = safe_float(params.k_index(prof))
    r.tt = safe_float(params.t_totals(prof))
    r.pwat = safe_float(params.precip_water(prof))

    # Shear
    try:
        p6km = interp.pres(prof, interp.to_msl(prof, 6000.0))
        p1km = interp.pres(prof, interp.to_msl(prof, 1000.0))
        shr06 = winds.wind_shear(prof, prof.pres[prof.sfc], p6km)
        shr01 = winds.wind_shear(prof, prof.pres[prof.sfc], p1km)
        r.shr06 = safe_float(utils.KTS2MS(utils.mag(shr06[0], shr06[1])))
        r.shr01 = safe_float(utils.KTS2MS(utils.mag(shr01[0], shr01[1])))
    except Exception:
        pass

    # Bunkers
    try:
        rstu, rstv, lstu, lstv = winds.non_parcel_bunkers_motion(prof)
        r.bunkers_ru = safe_float(rstu)
        r.bunkers_rv = safe_float(rstv)
        r.bunkers_lu = safe_float(lstu)
        r.bunkers_lv = safe_float(lstv)
    except Exception:
        pass

    # SRH
    try:
        if r.bunkers_ru is not None and r.bunkers_rv is not None:
            srh1 = winds.helicity(prof, 0, 1000, stu=rstu, stv=rstv)
            srh3 = winds.helicity(prof, 0, 3000, stu=rstu, stv=rstv)
            r.srh1 = safe_float(srh1[0])
            r.srh3 = safe_float(srh3[0])
    except Exception:
        pass

    # Composites
    try:
        if r.shr06 is not None and r.srh1 is not None:
            r.stp = safe_float(params.stp_fixed(sfcpcl.bplus, sfcpcl.lclhght, srh1[0], r.shr06))
    except Exception:
        pass

    try:
        if r.shr06 is not None and r.srh3 is not None:
            r.scp = safe_float(params.scp(mupcl.bplus, srh3[0], r.shr06))
    except Exception:
        pass

    try:
        r.ship = safe_float(params.ship(prof, mupcl=mupcl))
    except Exception:
        pass

    # Surface RH
    try:
        r.sfc_rh = safe_float(thermo.relh(s.pres[0], s.tmpc[0], s.dwpc[0]))
    except Exception:
        pass

    return r


# =========================================================================
# Comparison
# =========================================================================

def check_close(label: str, sharppy_val, sharprs_val, tol: float,
                tol_type: str = "abs", failures: list[str] = []) -> str:
    """Compare two values and return a formatted row. Append to failures if mismatch."""
    if sharppy_val is None and sharprs_val is None:
        return f"  {label:25s}  {'N/A':>12s}  {'N/A':>12s}  {'OK':>8s}"

    if sharppy_val is None or sharprs_val is None:
        status = "SKIP"
        return f"  {label:25s}  {str(sharppy_val):>12s}  {str(sharprs_val):>12s}  {status:>8s}"

    sv = float(sharppy_val)
    rv = float(sharprs_val)
    diff = abs(sv - rv)

    if tol_type == "pct":
        if abs(sv) < 1.0:
            ok = diff <= 1.0
        else:
            ok = diff <= abs(sv) * tol / 100.0
    else:
        ok = diff <= tol

    status = "OK" if ok else f"FAIL ({diff:.2f})"
    row = f"  {label:25s}  {sv:12.2f}  {rv:12.2f}  {status:>16s}"
    if not ok:
        failures.append(f"{label}: SHARPpy={sv:.4f}, sharprs={rv:.4f}, diff={diff:.4f}")
    return row


def compare_results(sharppy: Results, sharprs: Results) -> list[str]:
    """Compare SHARPpy and sharprs results. Returns list of failures."""
    failures: list[str] = []
    lines = []

    lines.append(f"\n{'='*70}")
    lines.append(f"  Sounding: {sharppy.name}")
    lines.append(f"{'='*70}")
    lines.append(f"  {'Parameter':25s}  {'SHARPpy':>12s}  {'sharprs':>12s}  {'Status':>16s}")
    lines.append(f"  {'-'*25}  {'-'*12}  {'-'*12}  {'-'*16}")

    lines.append(check_close("SB CAPE (J/kg)", sharppy.sb_cape, sharprs.sb_cape,
                              TOL.cape_pct, "pct", failures))
    lines.append(check_close("SB CIN (J/kg)", sharppy.sb_cin, sharprs.sb_cin,
                              TOL.cape_pct, "pct", failures))
    lines.append(check_close("SB LCL (hPa)", sharppy.sb_lcl_p, sharprs.sb_lcl_p,
                              TOL.lcl_hpa, "abs", failures))
    lines.append(check_close("SB LFC (hPa)", sharppy.sb_lfc_p, sharprs.sb_lfc_p,
                              TOL.lcl_hpa, "abs", failures))
    lines.append(check_close("SB EL (hPa)", sharppy.sb_el_p, sharprs.sb_el_p,
                              TOL.lcl_hpa, "abs", failures))
    lines.append(check_close("ML CAPE (J/kg)", sharppy.ml_cape, sharprs.ml_cape,
                              TOL.cape_pct, "pct", failures))
    lines.append(check_close("ML CIN (J/kg)", sharppy.ml_cin, sharprs.ml_cin,
                              TOL.cape_pct, "pct", failures))
    lines.append(check_close("MU CAPE (J/kg)", sharppy.mu_cape, sharprs.mu_cape,
                              TOL.cape_pct, "pct", failures))
    lines.append(check_close("K-Index (C)", sharppy.k_index, sharprs.k_index,
                              TOL.k_tt_c, "abs", failures))
    lines.append(check_close("Total Totals (C)", sharppy.tt, sharprs.tt,
                              TOL.k_tt_c, "abs", failures))
    lines.append(check_close("PWAT (in)", sharppy.pwat, sharprs.pwat,
                              TOL.pwat_mm / 25.4, "abs", failures))  # convert mm tol to inches
    lines.append(check_close("0-6km Shear (m/s)", sharppy.shr06, sharprs.shr06,
                              TOL.shear_ms, "abs", failures))
    lines.append(check_close("0-1km Shear (m/s)", sharppy.shr01, sharprs.shr01,
                              TOL.shear_ms, "abs", failures))
    lines.append(check_close("Bunkers R U (kt)", sharppy.bunkers_ru, sharprs.bunkers_ru,
                              utils.MS2KTS(TOL.bunkers_ms), "abs", failures))
    lines.append(check_close("Bunkers R V (kt)", sharppy.bunkers_rv, sharprs.bunkers_rv,
                              utils.MS2KTS(TOL.bunkers_ms), "abs", failures))
    lines.append(check_close("0-1km SRH (m2/s2)", sharppy.srh1, sharprs.srh1,
                              TOL.srh_m2s2, "abs", failures))
    lines.append(check_close("0-3km SRH (m2/s2)", sharppy.srh3, sharprs.srh3,
                              TOL.srh_m2s2, "abs", failures))
    lines.append(check_close("STP fixed", sharppy.stp, sharprs.stp,
                              TOL.composite, "abs", failures))
    lines.append(check_close("SCP", sharppy.scp, sharprs.scp,
                              TOL.composite, "abs", failures))
    lines.append(check_close("SHIP", sharppy.ship, sharprs.ship,
                              TOL.composite, "abs", failures))

    for line in lines:
        print(line)

    return failures


# =========================================================================
# Thermo function comparison
# =========================================================================

def compare_thermo_functions():
    """Compare individual thermodynamic functions between SHARPpy and sharprs."""
    print("\n" + "=" * 70)
    print("  Thermodynamic Function Reference Values (SHARPpy)")
    print("  (sharprs must match these when thermo module is implemented)")
    print("=" * 70)
    print(f"  {'Function':40s}  {'Value':>14s}")
    print(f"  {'-'*40}  {'-'*14}")

    tests = [
        ("theta(1000, 20, 1000)", thermo.theta(1000, 20, 1000)),
        ("theta(850, 15, 1000)", thermo.theta(850, 15, 1000)),
        ("theta(500, -10, 1000)", thermo.theta(500, -10, 1000)),
        ("lcltemp(20, 15)", thermo.lcltemp(20, 15)),
        ("lcltemp(30, 20)", thermo.lcltemp(30, 20)),
        ("lcltemp(35, 10)", thermo.lcltemp(35, 10)),
        ("drylift(1000, 30, 20)[0]", thermo.drylift(1000, 30, 20)[0]),
        ("drylift(1000, 30, 20)[1]", thermo.drylift(1000, 30, 20)[1]),
        ("vappres(20)", thermo.vappres(20)),
        ("vappres(0)", thermo.vappres(0)),
        ("vappres(-20)", thermo.vappres(-20)),
        ("mixratio(1000, 20)", thermo.mixratio(1000, 20)),
        ("mixratio(850, 10)", thermo.mixratio(850, 10)),
        ("wobf(20)", thermo.wobf(20)),
        ("wobf(0)", thermo.wobf(0)),
        ("wobf(-20)", thermo.wobf(-20)),
        ("wobf(-40)", thermo.wobf(-40)),
        ("wetlift(800, 10, 500)", thermo.wetlift(800, 10, 500)),
        ("wetlift(900, 20, 700)", thermo.wetlift(900, 20, 700)),
        ("virtemp(1000, 30, 20)", thermo.virtemp(1000, 30, 20)),
        ("virtemp(850, 15, 10)", thermo.virtemp(850, 15, 10)),
        ("relh(1000, 30, 20)", thermo.relh(1000, 30, 20)),
        ("thetae(1000, 30, 20)", thermo.thetae(1000, 30, 20)),
        ("thetae(850, 15, 10)", thermo.thetae(850, 15, 10)),
        ("thetaw(1000, 30, 20)", thermo.thetaw(1000, 30, 20)),
        ("wetbulb(1000, 30, 20)", thermo.wetbulb(1000, 30, 20)),
        ("temp_at_mixrat(10, 1000)", thermo.temp_at_mixrat(10, 1000)),
        ("satlift(500, 20)", thermo.satlift(500, 20)),
        ("lifted(1000, 30, 20, 500)", thermo.lifted(1000, 30, 20, 500)),
    ]

    for name, val in tests:
        print(f"  {name:40s}  {val:14.6f}")


# =========================================================================
# Main
# =========================================================================

def main():
    import warnings
    warnings.filterwarnings("ignore")

    print("SHARPpy/sharprs Verification Comparison")
    print("=" * 70)

    # Phase 1: Print thermo reference values
    compare_thermo_functions()

    # Phase 2: Compute SHARPpy values for all soundings
    soundings = make_soundings()
    all_failures: list[str] = []

    print("\n\n" + "=" * 70)
    print("  Full Sounding Parameter Comparison")
    print("  (SHARPpy vs sharprs -- sharprs column shows SHARPpy values")
    print("   until Rust implementation is complete)")
    print("=" * 70)

    for s in soundings:
        sharppy_results = compute_sharppy(s)

        # When sharprs is available as a Python extension or subprocess,
        # uncomment the following and compute sharprs_results:
        # sharprs_results = compute_sharprs(s)

        # For now, we do a self-comparison (SHARPpy vs SHARPpy) to validate
        # the test harness and print the reference values.
        sharprs_results = sharppy_results  # placeholder

        failures = compare_results(sharppy_results, sharprs_results)
        all_failures.extend(failures)

    # Summary
    print(f"\n{'='*70}")
    if all_failures:
        print(f"  FAILURES: {len(all_failures)}")
        for f in all_failures:
            print(f"    - {f}")
    else:
        print("  All comparisons PASSED (self-test mode: SHARPpy vs SHARPpy)")
    print(f"{'='*70}")

    return len(all_failures)


if __name__ == "__main__":
    sys.exit(main())
