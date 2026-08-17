import numpy as np
import sys, os, csv
sys.path.insert(0, os.path.abspath('/tmp/SHARPpy'))

from sharppy.sharptab import profile, params, thermo, interp, winds, utils

soundings = [
    ('tests/soundings/FWD_20250402_00Z.csv', 'FWD (Fort Worth TX) 00Z Apr 2 2025'),
    ('tests/soundings/SHV_20250402_00Z.csv', 'SHV (Shreveport LA) 00Z Apr 2 2025'),
    ('tests/soundings/SGF_20250402_00Z.csv', 'SGF (Springfield MO) 00Z Apr 2 2025'),
    ('tests/soundings/JAX_20250402_00Z.csv', 'JAX (Jacksonville FL) 00Z Apr 2 2025'),
    ('tests/soundings/MPX_20250402_00Z.csv', 'MPX (Minneapolis MN) 00Z Apr 2 2025'),
]

for path, name in soundings:
    pres_l, hght_l, tmpc_l, dwpc_l, wdir_l, wspd_l = [], [], [], [], [], []
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                p = float(row['PRES'])
                h = float(row['HGHT'])
                t = float(row['TMPC'])
                td = float(row['DWPC']) if row['DWPC'] else float('nan')
                wd = float(row['WDIR']) if row['WDIR'] else float('nan')
                ws = float(row['WSPD']) if row['WSPD'] else float('nan')
                pres_l.append(p)
                hght_l.append(h)
                tmpc_l.append(t)
                dwpc_l.append(td)
                wdir_l.append(wd)
                wspd_l.append(ws)
            except:
                pass

    pres = np.ma.masked_invalid(np.array(pres_l))
    hght = np.ma.masked_invalid(np.array(hght_l))
    tmpc = np.ma.masked_invalid(np.array(tmpc_l))
    dwpc = np.ma.masked_invalid(np.array(dwpc_l))
    wdir = np.ma.masked_invalid(np.array(wdir_l))
    wspd = np.ma.masked_invalid(np.array(wspd_l))

    try:
        prof = profile.create_profile(profile='default', pres=pres, hght=hght,
                                       tmpc=tmpc, dwpc=dwpc, wdir=wdir, wspd=wspd)

        sfcpcl = params.parcelx(prof, flag=1)
        mlpcl = params.parcelx(prof, flag=4)
        mupcl = params.parcelx(prof, flag=3)

        sfc = prof.pres[prof.sfc]
        p6km = interp.pres(prof, interp.to_msl(prof, 6000.))
        p1km = interp.pres(prof, interp.to_msl(prof, 1000.))
        shr6 = winds.wind_shear(prof, pbot=sfc, ptop=p6km)
        shr1 = winds.wind_shear(prof, pbot=sfc, ptop=p1km)

        # Use Bunkers for storm motion (returns rstu, rstv, lstu, lstv)
        rstu, rstv, lstu, lstv = winds.non_parcel_bunkers_motion(prof)
        srh1 = winds.helicity(prof, 0, 1000., stu=rstu, stv=rstv)
        srh3 = winds.helicity(prof, 0, 3000., stu=rstu, stv=rstv)

        shr6_mag_val = utils.mag(shr6[0], shr6[1])
        stp = params.stp_cin(mlpcl.bplus, srh3[0], shr6_mag_val,
                              mlpcl.lclhght, mlpcl.bminus)
        scp_val = params.scp(mupcl.bplus, srh3[0], shr6_mag_val)
        ship_val = params.ship(prof)

        ki = params.k_index(prof)
        tt = params.t_totals(prof)
        pw = params.precip_water(prof)
        lr = params.lapse_rate(prof, 700., 500.)

        shr6_mag = utils.mag(shr6[0], shr6[1])
        shr1_mag = utils.mag(shr1[0], shr1[1])

        print("")
        print("=" * 58)
        print("  " + name)
        print("=" * 58)
        print("  Sfc: %.0f hPa  T: %.1fC  Td: %.1fC" % (pres_l[0], tmpc_l[0], dwpc_l[0]))
        print("")
        print("  SB CAPE: %7.0f J/kg   CIN: %6.0f J/kg" % (sfcpcl.bplus, sfcpcl.bminus))
        print("  ML CAPE: %7.0f J/kg   CIN: %6.0f J/kg" % (mlpcl.bplus, mlpcl.bminus))
        print("  MU CAPE: %7.0f J/kg" % mupcl.bplus)
        print("  LCL: %5.0f m   LFC: %5.0f m   EL: %5.0f m" % (sfcpcl.lclhght, sfcpcl.lfchght, sfcpcl.elhght))
        print("")
        print("  0-6km Shear: %5.1f kts" % shr6_mag)
        print("  0-1km Shear: %5.1f kts" % shr1_mag)
        print("  0-1km SRH: %6.0f m2/s2" % srh1[0])
        print("  0-3km SRH: %6.0f m2/s2" % srh3[0])
        print("")
        print("  STP(cin): %5.1f   SCP: %5.1f   SHIP: %5.1f" % (stp, scp_val, ship_val))
        print("  K-Index: %5.0f   TT: %5.0f   PW: %5.2f in" % (ki, tt, pw))
        print("  700-500 LR: %4.1f C/km" % lr)
        print("=" * 58)
    except Exception as e:
        print("")
        print("=== %s ===" % name)
        print("ERROR: %s" % str(e))
        import traceback
        traceback.print_exc()
