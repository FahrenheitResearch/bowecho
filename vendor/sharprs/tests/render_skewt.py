import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
import csv, os, sys
sys.path.insert(0, '/tmp/SHARPpy')
from sharppy.sharptab import profile, params, thermo, interp, winds, utils
from matplotlib.ticker import MultipleLocator

soundings = [
    ('tests/soundings/FWD_20250402_00Z.csv', 'FWD Fort Worth TX', 'FWD'),
    ('tests/soundings/SHV_20250402_00Z.csv', 'SHV Shreveport LA', 'SHV'),
    ('tests/soundings/SGF_20250402_00Z.csv', 'SGF Springfield MO', 'SGF'),
    ('tests/soundings/JAX_20250402_00Z.csv', 'JAX Jacksonville FL', 'JAX'),
    ('tests/soundings/MPX_20250402_00Z.csv', 'MPX Minneapolis MN', 'MPX'),
]

SKEW = 37

def skew_t(t, p):
    return t + SKEW * np.log10(1000.0 / p)

for path, name, code in soundings:
    pres_l, hght_l, tmpc_l, dwpc_l, wdir_l, wspd_l = [], [], [], [], [], []
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                p = float(row['PRES']); h = float(row['HGHT']); t = float(row['TMPC'])
                td = float(row['DWPC']) if row['DWPC'] else float('nan')
                wd = float(row['WDIR']) if row['WDIR'] else float('nan')
                ws = float(row['WSPD']) if row['WSPD'] else float('nan')
                pres_l.append(p); hght_l.append(h); tmpc_l.append(t)
                dwpc_l.append(td); wdir_l.append(wd); wspd_l.append(ws)
            except:
                pass

    pres = np.ma.masked_invalid(np.array(pres_l))
    hght = np.ma.masked_invalid(np.array(hght_l))
    tmpc = np.ma.masked_invalid(np.array(tmpc_l))
    dwpc = np.ma.masked_invalid(np.array(dwpc_l))
    wdir = np.ma.masked_invalid(np.array(wdir_l))
    wspd = np.ma.masked_invalid(np.array(wspd_l))

    prof = profile.create_profile(profile='default', pres=pres, hght=hght,
                                   tmpc=tmpc, dwpc=dwpc, wdir=wdir, wspd=wspd)
    sfcpcl = params.parcelx(prof, flag=1)
    mlpcl = params.parcelx(prof, flag=4)
    mupcl = params.parcelx(prof, flag=3)
    rstu, rstv, lstu, lstv = winds.non_parcel_bunkers_motion(prof)
    srh1 = winds.helicity(prof, 0, 1000., stu=rstu, stv=rstv)
    srh3 = winds.helicity(prof, 0, 3000., stu=rstu, stv=rstv)
    sfc_p = prof.pres[prof.sfc]
    shr6 = winds.wind_shear(prof, pbot=sfc_p, ptop=interp.pres(prof, interp.to_msl(prof, 6000.)))
    shr6_mag = utils.mag(shr6[0], shr6[1])
    stp = params.stp_cin(mlpcl.bplus, srh3[0], shr6_mag, mlpcl.lclhght, mlpcl.bminus)
    scp_val = params.scp(mupcl.bplus, srh3[0], shr6_mag)
    ki = params.k_index(prof)
    tt = params.t_totals(prof)
    pw = params.precip_water(prof)

    fig = plt.figure(figsize=(14, 10), facecolor='#0a0a0f')

    # === Skew-T ===
    ax = fig.add_axes([0.06, 0.08, 0.48, 0.85])
    ax.set_facecolor('#0a0a0f')
    ax.set_ylim(1050, 100)
    ax.set_yscale('log')
    ax.set_xlim(-40, 50)
    ax.yaxis.set_major_locator(plt.FixedLocator([1000,850,700,500,400,300,250,200,150,100]))
    ax.yaxis.set_major_formatter(plt.FuncFormatter(lambda y, p: '%d' % y))
    ax.xaxis.set_major_locator(MultipleLocator(10))
    ax.tick_params(colors='#555', labelsize=8)
    ax.grid(True, alpha=0.15, color='#444')
    for spine in ax.spines.values():
        spine.set_color('#333')

    # Isotherms
    for t_val in range(-80, 60, 10):
        ps = np.array([1050, 100])
        ts = skew_t(np.array([t_val, t_val], dtype=float), ps)
        c = '#555' if t_val == 0 else '#333'
        lw = 1.0 if t_val == 0 else 0.5
        ax.plot(ts, ps, color=c, linewidth=lw, linestyle='--' if t_val != 0 else '-')

    # Dry adiabats
    for theta_val in range(250, 460, 20):
        ps = np.linspace(1050, 100, 100)
        ts = (theta_val * (ps / 1000.0) ** 0.286) - 273.15
        ax.plot(skew_t(ts, ps), ps, color='#3a2222', linewidth=0.4)

    # Temperature and dewpoint
    valid_t = ~tmpc.mask if hasattr(tmpc, 'mask') else ~np.isnan(tmpc)
    valid_td = ~dwpc.mask if hasattr(dwpc, 'mask') else ~np.isnan(dwpc)
    ax.plot(skew_t(tmpc[valid_t], pres[valid_t]), pres[valid_t], color='#ff3333', linewidth=2.5, zorder=5)
    ax.plot(skew_t(dwpc[valid_td], pres[valid_td]), pres[valid_td], color='#33cc33', linewidth=2.5, zorder=5)

    # Parcel trace
    if hasattr(sfcpcl, 'ptrace') and sfcpcl.ptrace is not None:
        pt = np.array(sfcpcl.ptrace)
        tt_trace = np.array(sfcpcl.ttrace)
        valid = ~np.isnan(pt) & ~np.isnan(tt_trace) & (pt > 0)
        if np.any(valid):
            ax.plot(skew_t(tt_trace[valid], pt[valid]), pt[valid],
                    color='#ff9900', linewidth=1.5, linestyle='--', zorder=4, alpha=0.8)

    # Wind barbs
    valid_w = (~wspd.mask & ~wdir.mask) if hasattr(wspd, 'mask') else (~np.isnan(wspd) & ~np.isnan(wdir))
    barb_p = pres[valid_w]
    barb_u_l, barb_v_l = [], []
    for idx in np.where(valid_w)[0]:
        u, v = utils.vec2comp(wdir[idx], wspd[idx])
        barb_u_l.append(float(u))
        barb_v_l.append(float(v))
    step = max(1, len(barb_p) // 30)
    if len(barb_p) > 0:
        bx = np.full(len(barb_p[::step]), 48.0)
        ax.barbs(bx, barb_p[::step],
                 np.array(barb_u_l[::step]), np.array(barb_v_l[::step]),
                 length=5, linewidth=0.6, color='#aaa', zorder=6)

    ax.set_ylabel('Pressure (hPa)', color='#888', fontsize=9)
    ax.set_xlabel('Temperature (C)', color='#888', fontsize=9)

    # === Hodograph ===
    ax2 = fig.add_axes([0.58, 0.50, 0.38, 0.42])
    ax2.set_facecolor('#0a0a0f')
    ax2.set_aspect('equal')

    u_arr, v_arr, h_arr = [], [], []
    for i in range(len(pres_l)):
        if not np.isnan(wdir_l[i]) and not np.isnan(wspd_l[i]):
            u, v = utils.vec2comp(wdir_l[i], wspd_l[i])
            u_arr.append(float(u))
            v_arr.append(float(v))
            h_arr.append(hght_l[i] - hght_l[0])

    u_arr = np.array(u_arr)
    v_arr = np.array(v_arr)
    h_arr = np.array(h_arr)

    max_spd = max(60, np.max(np.sqrt(u_arr**2 + v_arr**2)) * 1.2) if len(u_arr) > 0 else 60
    for r in range(20, int(max_spd) + 20, 20):
        circle = plt.Circle((0, 0), r, fill=False, color='#333', linewidth=0.5)
        ax2.add_patch(circle)
        ax2.text(r + 1, 1, str(r), fontsize=7, color='#555')

    colors_h = ['#ff3333', '#ff6600', '#ffcc00', '#33cc33', '#3399ff', '#9933ff']
    for i in range(len(u_arr) - 1):
        h_km = h_arr[i] / 1000.0
        ci = min(len(colors_h) - 1, int(h_km / 2))
        ax2.plot([u_arr[i], u_arr[i+1]], [v_arr[i], v_arr[i+1]], color=colors_h[ci], linewidth=2.5)

    ax2.plot(float(rstu), float(rstv), 'o', color='#ff3333', markersize=7, zorder=10, label='RM')
    ax2.plot(float(lstu), float(lstv), '^', color='#3399ff', markersize=7, zorder=10, label='LM')
    ax2.legend(fontsize=7, loc='upper right', facecolor='#14141f', edgecolor='#333', labelcolor='#aaa')

    ax2.set_xlim(-max_spd, max_spd)
    ax2.set_ylim(-max_spd, max_spd)
    ax2.axhline(0, color='#444', linewidth=0.5)
    ax2.axvline(0, color='#444', linewidth=0.5)
    ax2.tick_params(colors='#555', labelsize=7)
    for spine in ax2.spines.values():
        spine.set_color('#333')
    ax2.set_title('Hodograph (kts)', color='#888', fontsize=9, pad=8)

    # === Parameters panel ===
    ax3 = fig.add_axes([0.58, 0.08, 0.38, 0.38])
    ax3.set_facecolor('#0a0a0f')
    ax3.axis('off')

    def safe_fmt(val, fmt='%.0f'):
        try:
            f = float(val)
            if np.isnan(f):
                return 'N/A'
            return fmt % f
        except:
            return 'N/A'

    params_text = '\n'.join([
        'SB CAPE:  %s J/kg' % safe_fmt(sfcpcl.bplus),
        'ML CAPE:  %s J/kg' % safe_fmt(mlpcl.bplus),
        'MU CAPE:  %s J/kg' % safe_fmt(mupcl.bplus),
        'SB CIN:   %s J/kg' % safe_fmt(sfcpcl.bminus),
        'LCL:      %s m AGL' % safe_fmt(sfcpcl.lclhght),
        'LFC:      %s m AGL' % safe_fmt(sfcpcl.lfchght),
        'EL:       %s m AGL' % safe_fmt(sfcpcl.elhght),
        '',
        '0-6km Shr: %s kts' % safe_fmt(shr6_mag),
        '0-1km SRH: %s m2/s2' % safe_fmt(srh1[0]),
        '0-3km SRH: %s m2/s2' % safe_fmt(srh3[0]),
        '',
        'STP(cin):  %s' % safe_fmt(stp, '%.1f'),
        'SCP:       %s' % safe_fmt(scp_val, '%.1f'),
        'K-Index:   %s' % safe_fmt(ki),
        'TT:        %s' % safe_fmt(tt),
        'PW:        %s in' % safe_fmt(pw, '%.2f'),
    ])

    ax3.text(0.05, 0.95, params_text, transform=ax3.transAxes, fontsize=10,
             verticalalignment='top', fontfamily='monospace', color='#ddd',
             bbox=dict(boxstyle='round,pad=0.5', facecolor='#14141f', edgecolor='#333'))

    fig.suptitle(name + ' - 00Z Apr 2, 2025', color='#4af', fontsize=14, fontweight='bold', y=0.97)

    out = 'tests/soundings/' + code + '_skewt.png'
    fig.savefig(out, dpi=130, facecolor='#0a0a0f', bbox_inches='tight')
    plt.close(fig)
    print('Saved', out)
