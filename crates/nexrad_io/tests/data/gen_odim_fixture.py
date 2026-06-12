#!/usr/bin/env python
"""Generate the synthetic ODIM_H5 polar-volume golden fixture.

Provenance: this file is SYNTHETIC, written by h5py (3.15.1 at generation
time) following the ODIM_H5 v2.2 information model (D. B. Michelson et al.,
"EUMETNET OPERA weather radar information model for implementation with the
HDF5 file format", EUMETNET OPERA Working Document WD_2008_03, v2.2, 2014).
Layout mirrors what BALTRAD/rave and HL-HDF emit: superblock v0, v1 object
headers, old-style groups, fixed-length ASCII string attributes, gzip'd
chunked 8-bit data planes.

Volume: 2 elevations (0.5deg, 1.5deg), 36 rays x 25 bins, DBZH + VRADH.
Values are deterministic ramps so the Rust test can assert exact samples:
  DBZH raw = (ray + bin) % 254 + 1, gain 0.5, offset -32 -> dBZ
  VRADH raw = (ray * 2 + bin) % 254 + 1, gain 0.1875, offset -24 -> m/s
nodata = 255, undetect = 0; gate (0,0)/(0,1) of dataset1 DBZH forced to
nodata/undetect to pin sentinel handling.

Run: python gen_odim_fixture.py  (writes odim_pvol_synth.h5 next to itself)
"""

import os

import h5py
import numpy as np


def s(value: str) -> np.bytes_:
    """Fixed-length ASCII scalar string attribute (HL-HDF style)."""
    return np.bytes_(value.encode("ascii"))


def main() -> None:
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "odim_pvol_synth.h5")
    nrays, nbins = 36, 25
    with h5py.File(out, "w", libver="earliest") as f:
        f.attrs.create("Conventions", s("ODIM_H5/V2_2"))

        what = f.create_group("what")
        what.attrs.create("object", s("PVOL"))
        what.attrs.create("version", s("H5rad 2.2"))
        what.attrs.create("date", s("20260609"))
        what.attrs.create("time", s("055100"))
        what.attrs.create("source", s("NOD:test,PLC:Synthetic,RAD:XX99"))

        where = f.create_group("where")
        where.attrs.create("lat", np.float64(39.74))
        where.attrs.create("lon", np.float64(-103.2927))
        where.attrs.create("height", np.float64(1519.0))

        how = f.create_group("how")
        how.attrs.create("beamwidth", np.float64(0.93))
        how.attrs.create("NI", np.float64(48.0))

        for idx, elangle in enumerate((0.5, 1.5), start=1):
            ds = f.create_group(f"dataset{idx}")
            dwhat = ds.create_group("what")
            dwhat.attrs.create("product", s("SCAN"))
            dwhat.attrs.create("startdate", s("20260609"))
            dwhat.attrs.create("starttime", s("055100"))
            dwhere = ds.create_group("where")
            dwhere.attrs.create("elangle", np.float64(elangle))
            dwhere.attrs.create("nbins", np.int64(nbins))
            dwhere.attrs.create("nrays", np.int64(nrays))
            dwhere.attrs.create("rstart", np.float64(0.05))  # km
            dwhere.attrs.create("rscale", np.float64(150.0))  # m
            dwhere.attrs.create("a1gate", np.int64(0))

            ray, bin_ = np.meshgrid(
                np.arange(nrays, dtype=np.int64),
                np.arange(nbins, dtype=np.int64),
                indexing="ij",
            )
            dbzh = ((ray + bin_) % 254 + 1).astype(np.uint8)
            vradh = ((ray * 2 + bin_) % 254 + 1).astype(np.uint8)
            if idx == 1:
                dbzh[0, 0] = 255  # nodata
                dbzh[0, 1] = 0  # undetect

            for quantity, raw, gain, offset, compress in (
                ("DBZH", dbzh, 0.5, -32.0, True),
                ("VRADH", vradh, 0.1875, -24.0, False),
            ):
                data = ds.create_group(f"data{1 if quantity == 'DBZH' else 2}")
                kwargs = (
                    dict(compression="gzip", compression_opts=6, chunks=(18, nbins))
                    if compress
                    else dict()  # contiguous layout path
                )
                dset = data.create_dataset("data", data=raw, dtype="u1", **kwargs)
                dset.attrs.create("CLASS", s("IMAGE"))
                dset.attrs.create("IMAGE_VERSION", s("1.2"))
                qwhat = data.create_group("what")
                qwhat.attrs.create("quantity", s(quantity))
                qwhat.attrs.create("gain", np.float64(gain))
                qwhat.attrs.create("offset", np.float64(offset))
                qwhat.attrs.create("nodata", np.float64(255.0))
                qwhat.attrs.create("undetect", np.float64(0.0))
    print(out, os.path.getsize(out), "bytes")


if __name__ == "__main__":
    main()
