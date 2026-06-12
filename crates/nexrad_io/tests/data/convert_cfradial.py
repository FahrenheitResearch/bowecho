"""Convert a CfRadial-1 netCDF-4 file to NETCDF3_CLASSIC (CDF-1) bytes.

Real-world CfRadial 1.x is written by Radx as netCDF-4 these days; the
bowecho decoder reads the classic container (which early Radx wrote).
This produces the classic container with IDENTICAL variable data (raw
copy, no mask/scale applied), optionally dropping field variables to fit
fixture size budgets.

Usage: convert_cfradial.py <in.nc> <out.nc> [field,field,...]
"""
import sys
import netCDF4
import numpy as np


def main(src_path, dst_path, keep_fields=None):
    src = netCDF4.Dataset(src_path)
    dst = netCDF4.Dataset(dst_path, "w", format="NETCDF3_CLASSIC")
    field_vars = {k for k, v in src.variables.items() if v.dimensions == ("time", "range")}
    drop = set()
    if keep_fields is not None:
        drop = field_vars - set(keep_fields)
        print("dropping fields:", sorted(drop))

    dst.setncatts({k: src.getncattr(k) for k in src.ncattrs()})
    for name, dim in src.dimensions.items():
        dst.createDimension(name, None if dim.isunlimited() else len(dim))
    for name, var in src.variables.items():
        if name in drop:
            continue
        var.set_auto_maskandscale(False)
        fill = None
        if "_FillValue" in var.ncattrs():
            fill = var.getncattr("_FillValue")
        out = dst.createVariable(name, var.dtype, var.dimensions, fill_value=fill)
        out.set_auto_maskandscale(False)
        out.setncatts({k: var.getncattr(k) for k in var.ncattrs() if k != "_FillValue"})
        if var.shape:
            out[:] = var[:]
        else:
            out[()] = var[()]
    src.close()
    dst.close()
    with open(dst_path, "rb") as f:
        magic = f.read(4)
    print("wrote", dst_path, "magic:", magic)


if __name__ == "__main__":
    keep = sys.argv[3].split(",") if len(sys.argv) > 3 else None
    main(sys.argv[1], sys.argv[2], keep)
