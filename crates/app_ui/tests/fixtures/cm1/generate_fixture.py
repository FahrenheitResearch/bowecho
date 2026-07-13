"""Generate a tiny CM1-schema NetCDF fixture (metadata tests, not science)."""

from pathlib import Path

import netCDF4
import numpy as np


OUT = Path(__file__).with_name("cm1out_schema.nc")


with netCDF4.Dataset(OUT, "w", format="NETCDF3_64BIT_OFFSET") as nc:
    for name, length in (
        ("xh", 3),
        ("yh", 2),
        ("zh", 2),
        ("xf", 4),
        ("yf", 3),
        ("zf", 3),
        ("one", 1),
    ):
        nc.createDimension(name, length)
    nc.createDimension("time", None)

    axes = {
        "xh": ([-1.0, 0.0, 1.0], "west-east location of scalar grid points"),
        "xf": ([-1.5, -0.5, 0.5, 1.5], "west-east location of staggered u grid points"),
        "yh": ([-0.5, 0.5], "south-north location of scalar grid points"),
        "yf": ([-1.0, 0.0, 1.0], "south-north location of staggered v grid points"),
        "zh": ([0.5, 1.5], "nominal height of scalar grid points"),
        "zf": ([0.0, 1.0, 2.0], "nominal height of staggered w grid points"),
    }
    for name, (values, long_name) in axes.items():
        var = nc.createVariable(name, "f4", (name,))
        var.long_name = long_name
        var.units = "km"
        var.axis = name[0].upper()
        var[:] = values

    time = nc.createVariable("time", "f4", ("time",))
    time.long_name = "time since beginning of simulation"
    time.units = "seconds"
    time.axis = "T"
    time[:] = [0.0, 60.0]

    for name, values in (("umove", [12.5, 12.5]), ("vmove", [3.0, 3.0])):
        var = nc.createVariable(name, "f4", ("time",))
        var.long_name = name
        var.units = "m/s"
        var[:] = values

    cref = nc.createVariable("cref", "f4", ("time", "yh", "xh"))
    cref.long_name = "composite reflectivity"
    cref.units = "dBZ"
    cref[:] = np.arange(12, dtype=np.float32).reshape(2, 2, 3)

    scalar = nc.createVariable(
        "custom_scalar", "f4", ("time", "zh", "yh", "xh")
    )
    scalar.long_name = "fixture arbitrary scalar"
    scalar.units = "K"
    scalar[:] = np.array(
        [
            [[[0, 1, 2], [3, 4, 5]], [[10, 11, 12], [13, 14, 15]]],
            [[[100, 101, 102], [103, 104, 105]], [[110, 111, 112], [113, 114, 115]]],
        ],
        dtype=np.float32,
    )

    dbz = nc.createVariable("dbz", "f4", ("time", "zh", "yh", "xh"))
    dbz.long_name = "reflectivity"
    dbz.units = "dBZ"
    dbz[:] = scalar[:]

    zhval = nc.createVariable("zhval", "f4", ("time", "zh", "yh", "xh"))
    zhval.long_name = "height on model levels"
    zhval.units = "m"
    zhval[:] = np.array([500.0, 1500.0], dtype=np.float32)[None, :, None, None]

    u = nc.createVariable("u", "f4", ("time", "zh", "yh", "xf"))
    u.long_name = "u velocity"
    u.units = "m/s"
    u[:] = 0.0
    v = nc.createVariable("v", "f4", ("time", "zh", "yf", "xh"))
    v.long_name = "v velocity"
    v.units = "m/s"
    v[:] = 0.0
    w = nc.createVariable("w", "f4", ("time", "zf", "yh", "xh"))
    w.long_name = "w velocity"
    w.units = "m/s"
    w[:] = 0.0

    nc.setncattr("CM1 version", "cm1r21.1")
    nc.setncattr("Conventions", "CF-1.7")
    nc.setncattr("missing_value", np.float32(-1.0e30))
    nc.setncattr("x_units", "km")
    nc.setncattr("y_units", "km")
    nc.setncattr("z_units", "km")
    nc.setncattr("nx", np.int32(3))
    nc.setncattr("ny", np.int32(2))
    nc.setncattr("nz", np.int32(2))
    nc.setncattr("imoist", np.int32(1))
    nc.setncattr("ptype", np.int32(5))
    nc.setncattr("iorigin", np.int32(1))
    nc.setncattr("ctrlat", np.float32(36.68))
    nc.setncattr("ctrlon", np.float32(-98.35))
    for name, value in (
        ("year", 2026),
        ("month", 5),
        ("day", 20),
        ("hour", 18),
        ("minute", 30),
        ("second", 0),
    ):
        nc.setncattr(name, np.int32(value))

print(OUT)
