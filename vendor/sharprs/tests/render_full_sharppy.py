#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
Full SHARPpy-style sounding display renderer.
Replicates the Pivotal Weather / SHARPpy multi-panel layout.

Usage:
    PYTHONPATH=/tmp/SHARPpy python render_full_sharppy.py <sounding.csv> [output.png]

CSV format: PRES,HGHT,TMPC,DWPC,WDIR,WSPD  (missing winds leave field blank)
"""
import sys, os, csv, warnings
import numpy as np
import numpy.ma as ma

# Monkey-patch numpy for SHARPpy compatibility
np.float = np.float64
np.int = np.int_
np.bool = np.bool_

warnings.filterwarnings('ignore')

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.gridspec as gridspec
from matplotlib.patches import FancyArrowPatch, Rectangle
from matplotlib.collections import LineCollection
from datetime import datetime

# --- Patch SHARPpy's pwv module BEFORE importing profile ---
import sharppy.databases.pwv as _pwv
_orig_pwv_climo = _pwv.pwv_climo
def _patched_pwv_climo(prof_obj, station, month=None):
    try:
        return _orig_pwv_climo(prof_obj, station, month=month)
    except Exception:
        return 0
_pwv.pwv_climo = _patched_pwv_climo
from sharppy.sharptab import profile as _profile_mod
_profile_mod.pwv_climo = _patched_pwv_climo

from sharppy.sharptab import profile, params, thermo, interp, utils, winds
from sharppy.sharptab.constants import MISSING

# ============================================================
# Color palette
# ============================================================
BG        = '#000000'
TEMP_CLR  = '#FF0000'
DEWP_CLR  = '#00FF00'
PARCEL_CLR= '#FFD700'
WETBLB_CLR= '#00BFFF'
BARB_CLR  = '#00FFFF'
HGT_CLR   = '#00FFFF'
GRID_CLR  = '#333333'
ISO_CLR   = '#444444'
ZERO_CLR  = '#00BFFF'
DRY_AD    = '#8B6914'
MOIST_AD  = '#006400'
MIX_CLR   = '#005500'
EFF_CLR   = '#00FFFF'
OMEGA_CLR = '#00FF00'
CYAN      = '#00FFFF'
YELLOW    = '#FFFF00'
RED       = '#FF0000'
GREEN     = '#00FF00'
MAGENTA   = '#FF00FF'
WHITE     = '#FFFFFF'
ORANGE    = '#FF8C00'
GRAY      = '#888888'
DKGRAY    = '#444444'

HODO_COLORS = {
    (0, 1000):    '#FF0000',
    (1000, 3000): '#FF8C00',
    (3000, 6000): '#FFFF00',
    (6000, 9000): '#00FF00',
    (9000, 12000):'#0088FF',
}


def S(val, fmt='.0f', default='M'):
    """Safely format a value."""
    try:
        if val is ma.masked or val is None:
            return default
        v = float(val)
        if np.isnan(v) or v < -9990:
            return default
        return f'{v:{fmt}}'
    except Exception:
        return default

def SF(val, default=0.0):
    try:
        if val is ma.masked or val is None: return default
        v = float(val)
        return default if (np.isnan(v) or v < -9990) else v
    except Exception:
        return default


def load_csv(path):
    pres, hght, tmpc, dwpc, wdir, wspd = [], [], [], [], [], []
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            pres.append(float(row['PRES']))
            hght.append(float(row['HGHT']))
            tmpc.append(float(row['TMPC']))
            dwpc.append(float(row['DWPC']))
            w_d = row.get('WDIR','').strip()
            w_s = row.get('WSPD','').strip()
            wdir.append(float(w_d) if w_d else MISSING)
            wspd.append(float(w_s) if w_s else MISSING)
    return (np.array(pres), np.array(hght), np.array(tmpc),
            np.array(dwpc), np.array(wdir), np.array(wspd))


def make_profile(pres, hght, tmpc, dwpc, wdir, wspd, loc='OBS'):
    return profile.create_profile(
        profile='convective', pres=pres, hght=hght, tmpc=tmpc,
        dwpc=dwpc, wdir=wdir, wspd=wspd,
        strictQC=False, missing=MISSING,
        date=datetime(2025, 4, 2, 0, 0), location=loc)


# ============================================================
# Skew-T coordinate helpers
# ============================================================
SKEW = 37

def t2x(t, p):
    return t + SKEW * np.log10(1000.0 / p)

def p2y(p):
    return -np.log10(p)


# ============================================================
# Skew-T background
# ============================================================
def draw_bg(ax, pmin=100, pmax=1050):
    prs = np.arange(pmax, pmin - 1, -5).astype(float)
    lp  = np.log10(prs)
    # Isotherms
    for t in range(-120, 60, 10):
        x = t + SKEW * np.log10(1000.0 / prs)
        y = -lp
        c = ZERO_CLR if t == 0 else ISO_CLR
        w = 0.8 if t == 0 else 0.3
        ax.plot(x, y, color=c, lw=w, ls='-' if t == 0 else '--', alpha=0.7 if t == 0 else 0.5)
    # Dry adiabats
    for th in range(-30, 250, 10):
        tk = th + 273.15
        tv = tk * (prs / 1000.0) ** 0.28571426 - 273.15
        ax.plot(tv + SKEW * np.log10(1000.0 / prs), -lp,
                color=DRY_AD, lw=0.3, ls='--', alpha=0.4)
    # Moist adiabats
    for tw in range(-10, 38, 5):
        ta = np.zeros_like(prs)
        ta[0] = tw
        for i in range(1, len(prs)):
            try: ta[i] = thermo.wetlift(prs[i-1], ta[i-1], prs[i])
            except: ta[i] = ta[i-1]
        ax.plot(ta + SKEW * np.log10(1000.0 / prs), -lp,
                color=MOIST_AD, lw=0.3, ls='--', alpha=0.4)
    # Mixing ratio lines
    for w in [2, 4, 7, 10, 16, 24]:
        tmr = thermo.temp_at_mixrat(w, prs)
        ax.plot(tmr + SKEW * np.log10(1000.0 / prs), -lp,
                color=MIX_CLR, lw=0.3, ls=':', alpha=0.4)


def draw_skewt(ax, prof):
    pmin, pmax = 100, 1050
    ax.set_facecolor(BG)
    draw_bg(ax, pmin, pmax)

    # Pressure axis labels + isobars
    for p in [1000, 850, 700, 500, 400, 300, 250, 200, 150, 100]:
        yp = p2y(p)
        ax.axhline(y=yp, color=GRID_CLR, lw=0.4)

    # Temperature trace
    vm = ~prof.tmpc.mask & ~prof.pres.mask
    ax.plot(t2x(prof.tmpc[vm], prof.pres[vm]), p2y(prof.pres[vm]),
            color=TEMP_CLR, lw=1.6, zorder=5)

    # Dewpoint trace
    vd = ~prof.dwpc.mask & ~prof.pres.mask
    ax.plot(t2x(prof.dwpc[vd], prof.pres[vd]), p2y(prof.pres[vd]),
            color=DEWP_CLR, lw=1.6, zorder=5)

    # Wetbulb trace
    if hasattr(prof, 'wetbulb') and prof.wetbulb is not None:
        vw = ~prof.wetbulb.mask & ~prof.pres.mask
        ax.plot(t2x(prof.wetbulb[vw], prof.pres[vw]), p2y(prof.pres[vw]),
                color=WETBLB_CLR, lw=0.4, alpha=0.4, zorder=4)

    # Parcel traces
    for pcl, col, lw, ls in [(prof.mlpcl, PARCEL_CLR, 1.2, '--'),
                              (prof.mupcl, ORANGE, 0.7, ':')]:
        if pcl and pcl.ptrace is not None and pcl.ttrace is not None:
            try:
                pt = np.asarray(pcl.ptrace, dtype=float)
                tt = np.asarray(pcl.ttrace, dtype=float)
                ok = (pt > 0) & (pt < 1100) & np.isfinite(pt) & np.isfinite(tt)
                if hasattr(pt, 'mask'): ok &= ~pt.mask
                if hasattr(tt, 'mask'): ok &= ~tt.mask
                if ok.sum() > 1:
                    ax.plot(t2x(tt[ok], pt[ok]), p2y(pt[ok]),
                            color=col, lw=lw, ls=ls, zorder=6, alpha=0.9)
            except: pass

    # DCAPE trace
    if hasattr(prof, 'dpcl_ptrace') and prof.dpcl_ptrace is not None:
        try:
            dp = np.asarray(prof.dpcl_ptrace, dtype=float)
            dt = np.asarray(prof.dpcl_ttrace, dtype=float)
            ok = (dp > 0) & np.isfinite(dp) & np.isfinite(dt)
            if hasattr(dp, 'mask'): ok &= ~dp.mask
            if hasattr(dt, 'mask'): ok &= ~dt.mask
            if ok.sum() > 1:
                ax.plot(t2x(dt[ok], dp[ok]), p2y(dp[ok]),
                        color='#BB00BB', lw=0.7, ls='--', zorder=4)
        except: pass

    # LCL / LFC / EL labels
    pcl = prof.mlpcl
    for attr, lbl, col in [('lclpres','LCL','#00FF00'),
                            ('lfcpres','LFC','#FFFF00'),
                            ('elpres','EL','#FF00FF')]:
        try:
            pv = getattr(pcl, attr)
            if pv is ma.masked or float(pv) < pmin: continue
            hagl = interp.to_agl(prof, interp.hght(prof, float(pv)))
            yv = p2y(float(pv))
            tp = interp.temp(prof, float(pv))
            xv = t2x(float(tp), float(pv))
            ax.text(xv + 4, yv, f'{lbl} {float(hagl):.0f}m',
                    color=col, fontsize=5.5, ha='left', va='center',
                    fontweight='bold', zorder=10)
        except: pass

    # Effective inflow layer bracket
    if hasattr(prof, 'ebottom') and prof.ebottom is not ma.masked and prof.etop is not ma.masked:
        try:
            yb = p2y(float(prof.ebottom))
            yt = p2y(float(prof.etop))
            xe = t2x(-44, pmax)
            ax.plot([xe, xe], [yb, yt], color=EFF_CLR, lw=2.5, zorder=8)
            ax.plot([xe-1, xe+1], [yb, yb], color=EFF_CLR, lw=2.5, zorder=8)
            ax.plot([xe-1, xe+1], [yt, yt], color=EFF_CLR, lw=2.5, zorder=8)
        except: pass

    # Surface temp labels
    try:
        st = prof.tmpc[prof.sfc]; sd = prof.dwpc[prof.sfc]; sp = prof.pres[prof.sfc]
        ax.text(t2x(float(st), float(sp)), p2y(float(sp)) - 0.008,
                f'{thermo.ctof(st):.0f}F', color=TEMP_CLR, fontsize=5.5,
                ha='center', va='top', fontweight='bold')
        ax.text(t2x(float(sd), float(sp)), p2y(float(sp)) - 0.008,
                f'{thermo.ctof(sd):.0f}F', color=DEWP_CLR, fontsize=5.5,
                ha='center', va='top', fontweight='bold')
    except: pass

    # Wind barbs (right side, every 50mb plus mandatory levels)
    xr = t2x(50, 1000)
    barb_p, barb_u, barb_v = [], [], []
    for p in sorted(set(list(range(100, 200, 50)) + list(range(200, 400, 50)) +
                        list(range(400, 1050, 50)) + [925, 850, 700, 500, 300, 200, 150])):
        if p > prof.pres[prof.sfc] or p < prof.pres[prof.top]: continue
        try:
            u, v = interp.components(prof, p)
            if u is ma.masked: continue
            barb_p.append(p); barb_u.append(float(u)); barb_v.append(float(v))
        except: continue
    if barb_p:
        ax.barbs([xr]*len(barb_p), [p2y(p) for p in barb_p],
                 barb_u, barb_v, length=5.5, lw=0.6,
                 barbcolor=BARB_CLR, flagcolor=BARB_CLR,
                 barb_increments={'half':5,'full':10,'flag':50}, clip_on=True)

    # Height labels (left side)
    for hkm in [0, 1, 3, 6, 9, 12, 15]:
        try:
            ph = interp.pres(prof, interp.to_msl(prof, hkm*1000.))
            if ph is ma.masked or float(ph) < pmin: continue
            ax.text(t2x(-44, pmax) - 4, p2y(float(ph)), f'{hkm}km',
                    color=HGT_CLR, fontsize=5, ha='right', va='center', fontweight='bold')
        except: continue

    # Pressure labels
    for p in [1000, 850, 700, 500, 400, 300, 200, 150, 100]:
        ax.text(t2x(-48, pmax), p2y(p), str(p), color=GRAY,
                fontsize=4.5, ha='right', va='center')

    ax.set_xlim(t2x(-48, pmax), t2x(52, 1000))
    ax.set_ylim(p2y(pmax), p2y(pmin))
    ax.set_xticks([]); ax.set_yticks([])
    for s in ax.spines.values(): s.set_visible(False)


# ============================================================
# Hodograph
# ============================================================
def draw_hodograph(ax, prof):
    ax.set_facecolor(BG)
    ax.set_aspect('equal')
    mx = 80
    for r in range(10, mx+1, 10):
        th = np.linspace(0, 2*np.pi, 100)
        ax.plot(r*np.cos(th), r*np.sin(th), color=DKGRAY, lw=0.4)
        ax.text(r, -2, str(r), color=GRAY, fontsize=4, ha='center', va='top')
    ax.axhline(0, color=DKGRAY, lw=0.3)
    ax.axvline(0, color=DKGRAY, lw=0.3)

    sfch = prof.hght[prof.sfc]
    vw = ~prof.u.mask & ~prof.v.mask
    ua, va, ha = prof.u[vw], prof.v[vw], prof.hght[vw] - sfch

    for (hb, ht), c in HODO_COLORS.items():
        m = (ha >= hb) & (ha <= ht)
        idx = np.where(m)[0]
        if len(idx) < 2: continue
        i0 = max(idx[0]-1, 0)
        i1 = min(idx[-1]+2, len(ua))
        ax.plot(ua[i0:i1], va[i0:i1], color=c, lw=2, zorder=5)

    # Height dots
    for hk in range(0, 13):
        try:
            ph = interp.pres(prof, interp.to_msl(prof, hk*1000.))
            uh, vh = interp.components(prof, ph)
            if uh is ma.masked: continue
            ax.plot(float(uh), float(vh), 'o', color=WHITE, ms=2.5, zorder=6)
            ax.text(float(uh)+1.5, float(vh)+1, str(hk),
                    color=WHITE, fontsize=4, ha='left', va='bottom', zorder=7)
        except: continue

    # Bunkers RM/LM
    bk = prof.bunkers
    rmu, rmv, lmu, lmv = [float(x) for x in bk[:4]]
    rmd, rms = utils.comp2vec(rmu, rmv)
    lmd, lms = utils.comp2vec(lmu, lmv)
    ax.plot(rmu, rmv, 'o', color=RED, ms=5, zorder=8)
    ax.text(rmu+2, rmv-3, f'{S(rmd)}/{S(rms)} RM',
            color=RED, fontsize=5, fontweight='bold', zorder=9)
    ax.plot(lmu, lmv, 'o', color='#4488FF', ms=5, zorder=8)
    ax.text(lmu+2, lmv+2, f'{S(lmd)}/{S(lms)} LM',
            color='#4488FF', fontsize=5, fontweight='bold', zorder=9)

    # Corfidi
    try:
        upu, upv, dnu, dnv = winds.corfidi_mcs_motion(prof)
        upd, ups = utils.comp2vec(upu, upv)
        dnd, dns = utils.comp2vec(dnu, dnv)
        ax.plot(float(upu), float(upv), 's', color=CYAN, ms=4, zorder=8)
        ax.plot(float(dnu), float(dnv), 's', color=MAGENTA, ms=4, zorder=8)
    except:
        upd = ups = dnd = dns = ma.masked

    # Critical angle
    try:
        ca = winds.critical_angle(prof, stu=rmu, stv=rmv)
    except:
        ca = ma.masked

    ax.set_xlim(-mx, mx); ax.set_ylim(-mx, mx)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)

    return ca, upd, ups, dnd, dns


# ============================================================
# Omega profile strip
# ============================================================
def draw_omega(ax, prof):
    ax.set_facecolor(BG)
    pmin, pmax = 100, 1050
    has_o = hasattr(prof, 'omeg') and not np.all(prof.omeg.mask)
    if has_o:
        v = ~prof.omeg.mask & ~prof.pres.mask
        if v.sum() > 1:
            om = prof.omeg[v] * 10
            yv = p2y(prof.pres[v])
            ax.fill_betweenx(yv, 0, -om, where=(-om > 0), color=GREEN, alpha=0.3)
            ax.fill_betweenx(yv, 0, -om, where=(-om < 0), color=RED, alpha=0.3)
            ax.plot(-om, yv, color=OMEGA_CLR, lw=0.8)
    ax.axvline(0, color=DKGRAY, lw=0.3)
    ax.set_ylim(p2y(pmax), p2y(pmin))
    ax.set_xlim(-5, 5)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)
    ax.text(0.5, 1.005, '\u03C9', color=WHITE, fontsize=5, ha='center', va='bottom',
            transform=ax.transAxes)


# ============================================================
# Parameter table (bottom) - the big one
# ============================================================
def draw_table(ax, prof):
    ax.set_facecolor(BG)
    ax.set_xlim(0, 100)
    ax.set_ylim(0, 50)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)

    fs = 6.5
    fsh = 7.0

    def T(x, y, s, c=WHITE, sz=fs, ha='center', w='normal'):
        ax.text(x, y, s, color=c, fontsize=sz, ha=ha, va='center',
                fontfamily='monospace', fontweight=w)

    # ---- Horizontal divider lines ----
    for yy in [48, 37, 24, 16, 8]:
        ax.axhline(yy, color=DKGRAY, lw=0.5)
    # Vertical dividers
    for xx in [8, 20, 32, 44, 56, 68, 80]:
        ax.axvline(xx, color=DKGRAY, lw=0.3, ymin=0, ymax=1)

    # ============ ROW 1: PARCEL TABLE (y=38-48) ============
    y0 = 47
    hdrs = ['PCL', 'CAPE', 'CINH', 'LCL(m)', 'LI', 'LFC(m)', 'EL(m)']
    xs   = [4, 14, 23, 33, 43, 54, 66]
    for i, h in enumerate(hdrs):
        T(xs[i], y0, h, c=CYAN, sz=fsh, w='bold')

    pcls = [('SFC', prof.sfcpcl), ('ML', prof.mlpcl),
            ('FCST', prof.fcstpcl), ('MU', prof.mupcl)]
    for j, (lbl, p) in enumerate(pcls):
        y = y0 - 2.5 - j * 2.2
        T(xs[0], y, lbl, c=YELLOW)
        T(xs[1], y, S(p.bplus))
        T(xs[2], y, S(p.bminus))
        T(xs[3], y, S(p.lclhght))
        T(xs[4], y, S(p.li5, '.1f'))
        T(xs[5], y, S(p.lfchght))
        T(xs[6], y, S(p.elhght))

    # ============ ROW 2: KINEMATICS (y=24-37) ============
    y0k = 36
    khdrs = ['', 'EHI', 'SRH', 'Shear(kt)', 'MnWind', 'SRW']
    xk    = [4, 14, 25, 38, 52, 66]
    for i, h in enumerate(khdrs):
        T(xk[i], y0k, h, c=CYAN, sz=fsh, w='bold')

    def ehi(cape, srh):
        try:
            c, s = float(cape), float(srh)
            return (c * s) / 160000.0 if c > 0 else 0
        except: return 0

    def wdir_spd(vec):
        try:
            if isinstance(vec, tuple) and len(vec) >= 2:
                d, s = utils.comp2vec(vec[0], vec[1]) if not isinstance(vec[0], (float, np.floating)) or abs(vec[0]) > 360 else vec[:2]
                # vec is already dir/spd from comp2vec
                return f'{S(vec[0])}/{S(vec[1])}'
            return 'M'
        except: return 'M'

    sfc_p = prof.pres[prof.sfc]
    try:
        p1, p3, p6, p8 = interp.pres(prof, interp.to_msl(prof, np.array([1000.,3000.,6000.,8000.])))
    except:
        p1 = p3 = p6 = p8 = ma.masked

    kin = []
    # SFC-1km
    srh1 = prof.right_srh1km[0]
    sh1 = utils.mag(prof.sfc_1km_shear[0], prof.sfc_1km_shear[1])
    kin.append(('Sfc-1km', ehi(prof.mlpcl.bplus, srh1), srh1, sh1,
                prof.mean_1km, prof.right_srw_1km))
    # SFC-3km
    srh3 = prof.right_srh3km[0]
    sh3 = utils.mag(prof.sfc_3km_shear[0], prof.sfc_3km_shear[1])
    kin.append(('Sfc-3km', ehi(prof.mlpcl.bplus, srh3), srh3, sh3,
                prof.mean_3km, prof.right_srw_3km))
    # Eff Inflow
    if prof.etop is not ma.masked:
        esrh = prof.right_esrh[0]
        esh = utils.mag(prof.eff_shear[0], prof.eff_shear[1])
        me = utils.comp2vec(*prof.mean_eff)
        se = utils.comp2vec(*prof.right_srw_eff)
        kin.append(('Eff Inflow', ehi(prof.mlpcl.bplus, esrh), esrh, esh, me, se))
    else:
        kin.append(('Eff Inflow', 0, ma.masked, ma.masked,
                    (ma.masked, ma.masked), (ma.masked, ma.masked)))
    # SFC-6km
    sh6 = utils.mag(prof.sfc_6km_shear[0], prof.sfc_6km_shear[1])
    kin.append(('Sfc-6km', '', '', sh6, prof.mean_6km, prof.right_srw_6km))
    # SFC-8km
    sh8 = utils.mag(prof.sfc_8km_shear[0], prof.sfc_8km_shear[1])
    kin.append(('Sfc-8km', '', '', sh8, prof.mean_8km, prof.right_srw_8km))
    # LCL-EL
    shlce = utils.mag(prof.lcl_el_shear[0], prof.lcl_el_shear[1])
    kin.append(('LCL-EL', '', '', shlce, prof.mean_lcl_el, prof.right_srw_lcl_el))
    # Eff Shear
    ebw = prof.ebwspd if prof.etop is not ma.masked else ma.masked
    kin.append(('Eff Shear', '', '', ebw, '', ''))

    for j, (lbl, eh, sr, sh, mn, sw) in enumerate(kin):
        y = y0k - 1.8 - j * 1.7
        T(xk[0], y, lbl, c=YELLOW, ha='left', sz=5.5)
        if eh != '': T(xk[1], y, S(eh, '.1f'))
        if sr != '': T(xk[2], y, S(sr))
        T(xk[3], y, S(sh))
        if isinstance(mn, tuple) and len(mn) == 2:
            T(xk[4], y, f'{S(mn[0])}/{S(mn[1])}')
        if isinstance(sw, tuple) and len(sw) == 2:
            T(xk[5], y, f'{S(sw[0])}/{S(sw[1])}')

    # ============ ROW 3: THERMO INDICES (y=16-24) ============
    row3 = [
        ('PW', S(prof.pwat,'.2f')+'"'),
        ('K', S(prof.k_idx)),
        ('TT', S(prof.totals_totals)),
        ('TEI', S(getattr(prof,'tei',ma.masked))),
        ('MeanW', S(prof.mean_mixr,'.1f')),
        ('WNDG', S(getattr(prof,'wndg',ma.masked),'.1f')),
        ('ConvT', S(getattr(prof,'convT',ma.masked))+'F'),
        ('3CAPE', S(getattr(prof.mupcl,'b3km',ma.masked))),
    ]
    row3b = [
        ('LowRH', S(prof.low_rh)+'%'),
        ('MidRH', S(prof.mid_rh)+'%'),
        ('DCAPE', S(getattr(prof,'dcape',ma.masked))),
        ('maxT', S(getattr(prof,'maxT',ma.masked))+'F'),
        ('DwnT', S(getattr(prof,'drush',ma.masked))+'F'),
        ('MMP', S(getattr(prof,'mmp',ma.masked),'.2f')),
        ('SigSvr', S(getattr(prof,'sig_severe',ma.masked))),
        ('ESP', S(getattr(prof,'esp',ma.masked),'.1f')),
    ]

    for i, (lbl, val) in enumerate(row3):
        x = 3 + i * 12.3
        T(x, 23, lbl, c=CYAN, sz=5.5, w='bold', ha='left')
        T(x, 21, val, sz=5.5, ha='left')

    for i, (lbl, val) in enumerate(row3b):
        x = 3 + i * 12.3
        T(x, 19, lbl, c=CYAN, sz=5.5, w='bold', ha='left')
        T(x, 17, val, sz=5.5, ha='left')

    # ============ ROW 4: LAPSE RATES (y=8-16) ============
    T(3, 15, 'LAPSE RATES (C/km)', c=CYAN, sz=fsh, w='bold', ha='left')
    lr = [('Sfc-3km', S(prof.lapserate_3km,'.1f')),
          ('3-6km', S(prof.lapserate_3_6km,'.1f')),
          ('850-500', S(prof.lapserate_850_500,'.1f')),
          ('700-500', S(prof.lapserate_700_500,'.1f'))]
    for i, (lbl, val) in enumerate(lr):
        T(3 + i * 22, 12.5, f'{lbl}: {val}', sz=6, ha='left')

    # ============ ROW 5: STORM MOTIONS & COMPOSITES (y=0-8) ============
    bk = prof.bunkers
    rmd, rms = utils.comp2vec(bk[0], bk[1])
    lmd, lms = utils.comp2vec(bk[2], bk[3])
    try:
        upu, upv, dnu, dnv = winds.corfidi_mcs_motion(prof)
        cdd, cds = utils.comp2vec(dnu, dnv)
        cud, cus = utils.comp2vec(upu, upv)
    except:
        cdd = cds = cud = cus = ma.masked

    motions = [('BnkR', f'{S(rmd)}/{S(rms)}'),
               ('BnkL', f'{S(lmd)}/{S(lms)}'),
               ('CorDn', f'{S(cdd)}/{S(cds)}'),
               ('CorUp', f'{S(cud)}/{S(cus)}')]
    for i, (lbl, val) in enumerate(motions):
        x = 3 + i * 13
        T(x, 7, lbl, c=CYAN, sz=5.5, w='bold', ha='left')
        T(x, 5, val, sz=5.5, ha='left')

    composites = [('STP(cin)', S(prof.stp_cin,'.1f')),
                  ('STP(fix)', S(prof.stp_fixed,'.1f')),
                  ('SHIP', S(prof.ship,'.1f')),
                  ('SCP', S(prof.scp,'.1f')),
                  ('BRN Shr', S(prof.mupcl.brnshear))]
    for i, (lbl, val) in enumerate(composites):
        x = 55 + i * 9
        T(x, 7, lbl, c=CYAN, sz=5.5, w='bold', ha='left')
        T(x, 5, val, sz=5.5, ha='left')


# ============================================================
# SARS panel
# ============================================================
def draw_sars(ax, prof):
    ax.set_facecolor(BG)
    ax.set_title('SARS - Sounding Analogs', color=WHITE, fontsize=5.5, pad=2)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)

    y, fs = 0.92, 4.5
    def T(x, yy, s, c=WHITE, sz=fs, w='normal'):
        ax.text(x, yy, s, color=c, fontsize=sz, fontweight=w,
                ha='left', va='center', transform=ax.transAxes)

    T(0.05, y, 'SUPERCELL', c=CYAN, w='bold'); y -= 0.09
    try:
        m = prof.supercell_matches
        if m and len(m) >= 5:
            T(0.05, y, f'Sig: {m[2]}  Non: {m[3]}  Total: {m[4]}'); y -= 0.07
            for s in m[0][:4]:
                txt = s.decode('utf-8') if isinstance(s, bytes) else str(s)
                T(0.08, y, txt, c=GREEN, sz=3.5); y -= 0.05
        else:
            T(0.05, y, 'No Quality Matches', c=GRAY); y -= 0.07
    except:
        T(0.05, y, 'No Quality Matches', c=GRAY); y -= 0.07

    y -= 0.04
    T(0.05, y, 'SIGNIFICANT HAIL', c=CYAN, w='bold'); y -= 0.09
    try:
        m = prof.matches
        if m and len(m) >= 5:
            T(0.05, y, f'Sig: {m[2]}  Non: {m[3]}  Total: {m[4]}'); y -= 0.07
            for s in m[0][:4]:
                txt = s.decode('utf-8') if isinstance(s, bytes) else str(s)
                T(0.08, y, txt, c=GREEN, sz=3.5); y -= 0.05
        else:
            T(0.05, y, 'No Quality Matches', c=GRAY)
    except:
        T(0.05, y, 'No Quality Matches', c=GRAY)


# ============================================================
# STP inset
# ============================================================
def draw_stp_inset(ax, prof):
    ax.set_facecolor(BG)
    ax.set_title('Effective Layer STP', color=WHITE, fontsize=5, pad=1)
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)

    ef = np.array([
        [1.2, 2.6, 5.3, 8.3, 11.0],
        [0.2, 1.0, 2.4, 4.5, 8.4],
        [0.0, 0.6, 1.7, 3.7, 5.6],
        [0.0, 0.3, 1.2, 2.6, 4.5],
        [0.0, 0.1, 0.8, 2.0, 3.7],
        [0.0, 0.0, 0.2, 0.7, 1.7],
    ])
    xlabels = ['EF4+', 'EF3', 'EF2', 'EF1', 'EF0', 'NONTOR']
    colors = ['#FF0000', '#FF4400', '#FF8800', '#FFCC00', '#FFFF00', GRAY]
    n = len(xlabels)
    pos = np.arange(n)

    for i in range(n):
        x = pos[i]; c = colors[i]
        wl, q1, med, q3, wh = ef[i]
        ax.plot([x, x], [wl, q1], color=c, lw=1)
        ax.plot([x, x], [q3, wh], color=c, lw=1)
        ax.add_patch(Rectangle((x-0.3, q1), 0.6, q3-q1, fc='none', ec=c, lw=1))
        ax.plot([x-0.3, x+0.3], [med, med], color=c, lw=1.5)
        ax.plot([x-0.15, x+0.15], [wl, wl], color=c, lw=1)
        ax.plot([x-0.15, x+0.15], [wh, wh], color=c, lw=1)

    stp_val = SF(prof.stp_cin)
    ax.axhline(stp_val, color=WHITE, lw=1, ls='--', alpha=0.8)
    ax.set_xlim(-0.5, n-0.5); ax.set_ylim(0, 11)
    ax.set_xticks(pos); ax.set_xticklabels(xlabels, fontsize=3.5, color=WHITE)
    ax.set_yticks(range(0, 12))
    ax.set_yticklabels([str(i) for i in range(0, 12)], fontsize=3.5, color=WHITE)
    ax.tick_params(axis='both', colors=DKGRAY, length=2)


# ============================================================
# Hazard type
# ============================================================
def draw_hazard(ax, prof):
    ax.set_facecolor(BG)
    ax.set_title('Possible Hazard Type', color=WHITE, fontsize=5, pad=1)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)
    wt = getattr(prof, 'watch_type', 'NONE')
    cmap = {'PDS TOR': RED, 'TOR': RED, 'MRGL TOR': ORANGE,
            'SVR': YELLOW, 'MRGL SVR': '#FFCC00', 'FLASH FLOOD': GREEN,
            'BLIZZARD': '#0088FF', 'NONE': GRAY}
    ax.text(0.5, 0.5, wt, color=cmap.get(wt, WHITE), fontsize=10,
            ha='center', va='center', fontweight='bold', transform=ax.transAxes)


# ============================================================
# Inferred Temp Advection
# ============================================================
def draw_inftemp(ax, prof):
    ax.set_facecolor(BG)
    ax.set_title('Inferred Temp Advection', color=WHITE, fontsize=5, pad=1)
    ax.set_xticks([]); ax.set_yticks([])
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)
    if not hasattr(prof, 'inf_temp_adv') or prof.inf_temp_adv is None:
        ax.text(0.5, 0.5, 'N/A', color=WHITE, fontsize=6, ha='center', va='center',
                transform=ax.transAxes)
        return
    try:
        inf = prof.inf_temp_adv
        if len(inf) < 2: return
        pa = np.array([x[0] for x in inf])
        ta = np.array([x[1] for x in inf])
        ok = (pa > 0) & np.isfinite(pa) & np.isfinite(ta)
        if ok.sum() < 2: return
        ax.set_ylim(p2y(1050), p2y(100))
        for pv, ti in zip(pa[ok], ta[ok]):
            try:
                to = interp.temp(prof, pv)
                yp = p2y(pv)
                c = RED if ti > float(to) else '#0088FF'
                ax.barh(yp, float(ti - float(to)) * 2, height=0.005, color=c, alpha=0.7)
            except: continue
    except:
        ax.text(0.5, 0.5, 'N/A', color=WHITE, fontsize=6, ha='center', va='center',
                transform=ax.transAxes)


# ============================================================
# Storm Slinky
# ============================================================
def draw_slinky(ax, prof):
    ax.set_facecolor(BG)
    ax.set_title('Storm Slinky', color=WHITE, fontsize=5, pad=1)
    ax.set_xticks([]); ax.set_yticks([])
    ax.set_aspect('equal')
    for sp in ax.spines.values(): sp.set_color(DKGRAY); sp.set_lw(0.5)
    if not hasattr(prof, 'slinky_traj') or prof.slinky_traj is ma.masked:
        ax.text(0.5, 0.5, 'N/A', color=WHITE, fontsize=6, ha='center', va='center',
                transform=ax.transAxes)
        return
    try:
        tr = prof.slinky_traj
        if tr is None or len(tr) < 2: return
        xs = [t[0] for t in tr]; ys = [t[1] for t in tr]
        n = len(xs)
        for i in range(n):
            a = 0.3 + 0.7 * i / max(n-1, 1)
            sz = 2 + 4 * i / max(n-1, 1)
            ax.plot(xs[i], ys[i], 'o', color=YELLOW, ms=sz, alpha=a)
        ax.plot(xs, ys, '-', color=GRAY, lw=0.5, alpha=0.5)
        tilt = prof.updraft_tilt
        if tilt is not ma.masked:
            ax.text(0.5, 0.05, f'Tilt = {float(tilt):.0f}\u00b0', color=WHITE,
                    fontsize=5, ha='center', va='bottom', transform=ax.transAxes)
    except:
        ax.text(0.5, 0.5, 'N/A', color=WHITE, fontsize=6, ha='center', va='center',
                transform=ax.transAxes)


# ============================================================
# MAIN RENDER
# ============================================================
def render(csv_path, output_path=None):
    pres, hght, tmpc, dwpc, wdir, wspd = load_csv(csv_path)
    bn = os.path.basename(csv_path)
    loc = bn.split('_')[0] if '_' in bn else 'OBS'
    prof = make_profile(pres, hght, tmpc, dwpc, wdir, wspd, loc=loc)

    fig = plt.figure(figsize=(16, 10), facecolor=BG, dpi=150)

    # Top/Bottom split: skew-T area vs table
    outer = gridspec.GridSpec(2, 1, height_ratios=[2.5, 1.3], hspace=0.04,
                              left=0.01, right=0.99, top=0.97, bottom=0.01)

    # Top row: omega | skew-T | right panels
    top = gridspec.GridSpecFromSubplotSpec(1, 3, subplot_spec=outer[0],
                                           width_ratios=[0.025, 0.50, 0.475], wspace=0.01)
    ax_omega = fig.add_subplot(top[0, 0])
    draw_omega(ax_omega, prof)

    ax_skewt = fig.add_subplot(top[0, 1])
    draw_skewt(ax_skewt, prof)

    # Right column: hodograph (top), then mid panels, then SARS/STP/hazard
    right = gridspec.GridSpecFromSubplotSpec(3, 1, subplot_spec=top[0, 2],
                                             height_ratios=[1.1, 0.5, 0.9], hspace=0.06)

    # Hodograph
    ax_hodo = fig.add_subplot(right[0])
    ca, upd, ups, dnd, dns = draw_hodograph(ax_hodo, prof)

    # Annotation text below hodograph
    hodo_bbox = ax_hodo.get_position()

    # Mid panels: InfTemp | Slinky
    mid = gridspec.GridSpecFromSubplotSpec(1, 2, subplot_spec=right[1], wspace=0.08)
    ax_inftemp = fig.add_subplot(mid[0])
    draw_inftemp(ax_inftemp, prof)
    ax_slinky = fig.add_subplot(mid[1])
    draw_slinky(ax_slinky, prof)

    # Bottom right: SARS | STP + Hazard
    bot_right = gridspec.GridSpecFromSubplotSpec(1, 2, subplot_spec=right[2],
                                                 width_ratios=[1, 1], wspace=0.06)
    ax_sars = fig.add_subplot(bot_right[0])
    draw_sars(ax_sars, prof)

    br2 = gridspec.GridSpecFromSubplotSpec(2, 1, subplot_spec=bot_right[1],
                                            height_ratios=[1.5, 1], hspace=0.1)
    ax_stp = fig.add_subplot(br2[0])
    draw_stp_inset(ax_stp, prof)
    ax_haz = fig.add_subplot(br2[1])
    draw_hazard(ax_haz, prof)

    # Bottom: parameter table
    ax_table = fig.add_subplot(outer[1])
    draw_table(ax_table, prof)

    # Hodograph annotations - place just below hodograph
    # Get hodograph bottom y position
    hodo_pos = ax_hodo.get_position()
    ann_y = hodo_pos.y0 - 0.01
    mid_x = (hodo_pos.x0 + hodo_pos.x1) / 2
    fig.text(mid_x, ann_y, f'Critical Angle = {S(ca)}\u00b0', color=WHITE, fontsize=5.5, ha='center', va='top')
    fig.text(mid_x - 0.12, ann_y, f'UP={S(upd)}/{S(ups)}', color=CYAN, fontsize=5.5, ha='center', va='top')
    fig.text(mid_x + 0.12, ann_y, f'DN={S(dnd)}/{S(dns)}', color=MAGENTA, fontsize=5.5, ha='center', va='top')

    # Title
    fig.text(0.5, 0.99, f'SHARPpy Sounding Analysis - {loc}',
             color=WHITE, fontsize=9, ha='center', va='top', fontweight='bold')

    if output_path is None:
        output_path = csv_path.replace('.csv', '_sharppy.png')
    fig.savefig(output_path, facecolor=BG, dpi=150, bbox_inches='tight', pad_inches=0.05)
    plt.close(fig)
    print(f'Saved to {output_path}')
    return output_path


if __name__ == '__main__':
    if len(sys.argv) < 2:
        print(f'Usage: {sys.argv[0]} <sounding.csv> [output.png]')
        sys.exit(1)
    render(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
