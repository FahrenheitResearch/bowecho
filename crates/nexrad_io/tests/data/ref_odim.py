"""Independent ODIM_H5 reference dump (h5py) in dump_radar.rs format.

Replicates the decoder's documented conventions so `diff` against the Rust
output is mechanical:
- cuts sorted by elangle ascending,
- ray center azimuth = (i + 0.5) * 360 / nrays,
- physical = gain * raw + offset; nodata/undetect -> None,
- canonical quantity mapping with first-wins (duplicates keep raw name),
- moments per cut printed in BTreeMap order of the Rust MomentType
  (variant order, Unknown(name) sorted by name last).
"""
import sys
import h5py
import numpy as np

CANON = {
    "DBZH": "REF", "DBZV": "REF", "TH": "REF", "TV": "REF", "DBZ": "REF",
    "VRADH": "VEL", "VRADV": "VEL", "VRAD": "VEL", "VRADDH": "VEL",
    "WRADH": "SW", "WRADV": "SW", "WRAD": "SW",
    "ZDR": "ZDR", "ZDRU": "ZDR",
    "RHOHV": "RHO", "RHOHVU": "RHO",
    "PHIDP": "PHI", "PHIDPU": "PHI", "UPHIDP": "PHI",
    "KDP": "KDP", "KDPU": "KDP",
}
# BTreeMap order = MomentType variant declaration order, Unknowns last by name.
VARIANT_ORDER = {"REF": 0, "VEL": 1, "SW": 2, "ZDR": 3, "RHO": 4, "PHI": 5, "KDP": 6}


def s(v):
    return v.decode() if isinstance(v, bytes) else str(v)


def main(path):
    f = h5py.File(path, "r")
    what = f["what"].attrs
    where = f["where"].attrs
    source = s(what.get("source", ""))
    ids, name = {}, None
    for pair in source.split(","):
        if ":" not in pair:
            continue
        k, v = pair.split(":", 1)
        k, v = k.strip(), v.strip()
        if not v:
            continue
        if k in ("NOD", "RAD", "WMO"):
            ids[k] = v.upper() if k == "NOD" else v
        elif k == "PLC":
            name = v
    sid = ids.get("NOD") or ids.get("RAD") or ids.get("WMO") or "ODIM"
    print("kind: OdimH5")
    print(
        f"site: id={sid} name={name or '-'} lat={f32(where['lat']):.4f} "
        f"lon={f32(where['lon']):.4f} elev_m={f32(where['height']):.4f}"
    )
    date, time = s(what["date"]), s(what["time"])
    print(f"time: {date[:4]}-{date[4:6]}-{date[6:8]}T{time[:2]}:{time[2:4]}:{time[4:6]}Z")

    root_ni = None
    if "how" in f and "NI" in f["how"].attrs:
        root_ni = float(f["how"].attrs["NI"])

    datasets = sorted(
        [k for k in f.keys() if k.startswith("dataset") and k[7:].isdigit()],
        key=lambda n: int(n[7:]),
    )
    sweeps = []
    total_rays = 0
    for ds in datasets:
        g = f[ds]
        elangle = float(g["where"].attrs["elangle"])
        sweeps.append((elangle, ds))
        first = sorted(
            [k for k in g.keys() if k.startswith("data") and k[4:].isdigit()],
            key=lambda n: int(n[4:]),
        )[0]
        total_rays += g[first]["data"].shape[0]
    sweeps.sort(key=lambda pair: pair[0])
    print(f"scan_mode: Some(Ppi) cuts={len(sweeps)} radials={total_rays}")

    for index, (elangle, ds) in enumerate(sweeps):
        g = f[ds]
        dwhere = g["where"].attrs
        rstart_km = float(dwhere.get("rstart", 0.0))
        rscale = float(dwhere.get("rscale", 0.0))
        ni = root_ni
        if "how" in g and "NI" in g["how"].attrs:
            ni = float(g["how"].attrs["NI"])
        data_names = sorted(
            [k for k in g.keys() if k.startswith("data") and k[4:].isdigit()],
            key=lambda n: int(n[4:]),
        )
        nrays, nbins = g[data_names[0]]["data"].shape
        az0 = (0 + 0.5) * 360.0 / nrays
        gate0 = round(rstart_km * 1000.0)
        spacing = max(round(rscale), 1)
        nyq = f"{ni:.4f}" if ni and ni > 0 else "None"
        print(
            f"cut {index} elev={elangle:.3f} radials={nrays} az0={az0:.4f} "
            f"el0={elangle:.4f} nyq0={nyq} gate0={gate0} spacing={spacing} gates={nbins}"
        )

        moments = []  # (sort_key, label, plane info)
        seen = set()
        for dn in data_names:
            dg = g[dn]
            w = dg["what"].attrs
            quantity = s(w.get("quantity", dn.upper()))
            canon = CANON.get(quantity)
            if canon is not None and canon not in seen:
                label = canon
                seen.add(canon)
            else:
                label = quantity
            key = (VARIANT_ORDER.get(label, 7), label if label not in VARIANT_ORDER else "")
            moments.append((key, label, dn, w))
        moments.sort(key=lambda m: m[0])

        for _, label, dn, w in moments:
            arr = np.asarray(g[dn]["data"])
            if arr.shape != (nrays, nbins):
                continue  # decoder skips mismatched planes
            gain = float(w.get("gain", 1.0)) or 1.0
            offset = float(w.get("offset", 0.0))
            nodata = w.get("nodata")
            undetect = w.get("undetect")
            storage = {1: "u8", 2: "u16", 4: "f32", 8: "f64"}.get(arr.dtype.itemsize, "?")
            if arr.dtype.kind == "f":
                storage = "f32"
            print(f"  moment {label} storage={storage} rows={nrays} bins={nbins}")
            for ray, bin_ in [
                (0, 0),
                (0, nbins // 2),
                (nrays // 4, nbins // 3),
                (nrays // 2, min(10, nbins - 1)),
                (nrays - 1, nbins - 1),
            ]:
                raw = float(arr[ray, bin_])
                if (nodata is not None and raw == float(nodata)) or (
                    undetect is not None and raw == float(undetect)
                ):
                    print(f"    v[{ray},{bin_}]=None")
                else:
                    print(f"    v[{ray},{bin_}]={gain * raw + offset:.4f}")


def f32(v):
    """Match the decoder's f64->f32 narrowing of site coordinates."""
    return float(np.float32(v))


if __name__ == "__main__":
    main(sys.argv[1])
