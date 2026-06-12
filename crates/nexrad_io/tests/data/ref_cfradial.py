"""Independent CfRadial-1 reference dump (netCDF4) in dump_radar.rs format.

Replicates the decoder's documented conventions (cfradial.rs):
- cuts = sweeps via sweep_start/end_ray_index, sorted by fixed_angle,
- gate geometry derived from the range coordinate (center - spacing/2),
- CF packing raw*scale_factor + add_offset, _FillValue/missing -> None,
- canonical moment naming shared with the DORADE decoder (suffix
  stripping), first-wins, then BTreeMap print order.
"""
import sys
import netCDF4
import numpy as np

VARIANT_ORDER = {"REF": 0, "VEL": 1, "SW": 2, "ZDR": 3, "RHO": 4, "PHI": 5, "KDP": 6}
STEMS = {
    "REF": {"DBZ", "DZ", "DBZH", "DBZV", "REF", "CZ", "UZ"},
    "VEL": {"VR", "VE", "VEL", "VU", "VG", "VT"},
    "SW": {"SW", "WIDTH", "SPW", "SPECTRUM_WIDTH"},
    "ZDR": {"ZDR", "ZD", "UZDR"},
    "RHO": {"RHOHV", "RHO", "RH", "ROHV"},
    "PHI": {"PHIDP", "PHI", "PH", "UPHIDP"},
    "KDP": {"KDP", "KD"},
}


def canonical(name):
    stem = name.strip().upper()
    while True:
        for label, stems in STEMS.items():
            if stem in stems:
                return label
        for suffix in ["_F", "_HC", "_VC", "HC", "_V", "_H"]:
            if stem.endswith(suffix) and len(stem) > len(suffix):
                stem = stem[: -len(suffix)]
                break
        else:
            return None


def chars_to_str(arr):
    raw = arr.tobytes() if hasattr(arr, "tobytes") else bytes(arr)
    return raw.split(b"\x00")[0].decode("ascii", "replace").strip()


def main(path):
    d = netCDF4.Dataset(path)
    for v in d.variables.values():
        v.set_auto_maskandscale(False)
    n_rays = len(d.dimensions["time"])
    n_gates = len(d.dimensions["range"])
    azimuth = np.asarray(d.variables["azimuth"][:], dtype=np.float64)
    elevation = np.asarray(d.variables["elevation"][:], dtype=np.float64)
    nyq = None
    if "nyquist_velocity" in d.variables:
        nyq = np.asarray(d.variables["nyquist_velocity"][:], dtype=np.float64)
    rng = np.asarray(d.variables["range"][:], dtype=np.float64)
    spacing = max(round(rng[1] - rng[0]), 1.0)
    first_gate = round(rng[0] - spacing / 2.0)

    fixed = np.atleast_1d(np.asarray(d.variables["fixed_angle"][:], dtype=np.float64))
    starts = np.atleast_1d(np.asarray(d.variables["sweep_start_ray_index"][:], dtype=np.int64))
    ends = np.atleast_1d(np.asarray(d.variables["sweep_end_ray_index"][:], dtype=np.int64))
    n_sweeps = max(len(fixed), 1)

    modes = []
    if "sweep_mode" in d.variables:
        sm = d.variables["sweep_mode"][:]
        for row in np.atleast_2d(sm):
            modes.append(chars_to_str(np.asarray(row)))
    rust_modes = []
    for m in modes:
        rust_modes.append(
            {"azimuth_surveillance": "Ppi", "sector": "Ppi", "manual_ppi": "Ppi",
             "rhi": "Rhi", "manual_rhi": "Rhi", "vertical_pointing": "VerticalPointing"}.get(m, "Other")
        )
    combined = "None"
    if rust_modes:
        combined = f"Some({rust_modes[0]})" if all(m == rust_modes[0] for m in rust_modes) else "Some(Other)"

    sid = getattr(d, "instrument_name", None) or getattr(d, "site_name", None) or "CFRAD"
    site_name = getattr(d, "site_name", None)
    def scalar0(name):
        if name not in d.variables:
            return None
        arr = np.atleast_1d(np.asarray(d.variables[name][:], dtype=np.float64))
        return float(np.float32(arr.flat[0])) if arr.size else None

    lat = scalar0("latitude")
    lon = scalar0("longitude")
    alt = scalar0("altitude")
    tcs = d.variables.get("time_coverage_start")
    start_text = chars_to_str(np.asarray(tcs[:])) if tcs is not None else getattr(d, "time_coverage_start", "")
    iso = start_text.strip().rstrip("Z").replace(" ", "T")

    print("kind: CfRadial")
    fmt = lambda v: "None" if v is None else f"{v:.4f}"
    print(f"site: id={sid.strip()} name={site_name or '-'} lat={fmt(lat)} lon={fmt(lon)} elev_m={fmt(alt)}")
    print(f"time: {iso}Z")

    sweeps = []
    for s in range(n_sweeps):
        start = int(starts[s]) if s < len(starts) else 0
        end = min(int(ends[s]) if s < len(ends) else n_rays - 1, n_rays - 1)
        if start > end or end >= n_rays:
            continue
        fx = float(fixed[s]) if s < len(fixed) else float(np.mean(elevation[start : end + 1]))
        sweeps.append((np.float32(fx), start, end))
    sweeps.sort(key=lambda t: t[0])
    total = sum(end - start + 1 for _, start, end in sweeps)
    print(f"scan_mode: {combined} cuts={len(sweeps)} radials={total}")

    fields = [(k, v) for k, v in d.variables.items() if v.dimensions == ("time", "range")]
    for index, (fx, start, end) in enumerate(sweeps):
        rows = end - start + 1
        az0 = float(np.float32(azimuth[start])) % 360.0
        el0 = float(np.float32(elevation[start]))
        ny0 = "None"
        if nyq is not None and nyq[start] > 0:
            ny0 = f"{float(np.float32(nyq[start])):.4f}"
        print(
            f"cut {index} elev={fx:.3f} radials={rows} az0={az0:.4f} el0={el0:.4f} "
            f"nyq0={ny0} gate0={int(first_gate)} spacing={int(spacing)} gates={n_gates}"
        )
        moments = []
        seen = set()
        for name, var in fields:
            label = canonical(name)
            if label is None or label in seen:
                label_out = name
            else:
                label_out = label
                seen.add(label)
            key = (VARIANT_ORDER.get(label_out, 7), label_out if label_out not in VARIANT_ORDER else "")
            moments.append((key, label_out, name, var))
        moments.sort(key=lambda m: m[0])
        for _, label, name, var in moments:
            scale = float(getattr(var, "scale_factor", 1.0))
            offset = float(getattr(var, "add_offset", 0.0))
            fill = getattr(var, "_FillValue", getattr(var, "missing_value", None))
            print(f"  moment {label} storage=f32 rows={rows} bins={n_gates}")
            for ray, bin_ in [
                (0, 0),
                (0, n_gates // 2),
                (rows // 4, n_gates // 3),
                (rows // 2, min(10, n_gates - 1)),
                (rows - 1, n_gates - 1),
            ]:
                raw = float(np.asarray(var[start + ray, bin_]))
                if (fill is not None and raw == float(fill)) or not np.isfinite(raw):
                    print(f"    v[{ray},{bin_}]=None")
                else:
                    value = np.float32(raw * scale + offset)
                    print(f"    v[{ray},{bin_}]={value:.4f}")


if __name__ == "__main__":
    main(sys.argv[1])
