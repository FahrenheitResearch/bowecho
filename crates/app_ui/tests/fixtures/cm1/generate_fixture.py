"""Generate a tiny CM1-schema NetCDF fixture (metadata tests, not science)."""

from pathlib import Path

import netCDF4
import numpy as np


OUT = Path(__file__).with_name("cm1out_schema.nc")
LEGACY_OUT = Path(__file__).with_name("cm1out_legacy_r19.nc")
DIAGNOSTIC_OUTPUTS = (
    (Path(__file__).with_name("cm1out_diag_000001.nc"), 0.0, 0.0, 0.0),
    (Path(__file__).with_name("cm1out_diag_000002.nc"), 60.0, 750.0, 180.0),
)


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
    u[:] = np.broadcast_to(
        np.array([0.0, 2.0, 4.0, 6.0], dtype=np.float32)[None, None, None, :],
        (2, 2, 2, 4),
    )
    v = nc.createVariable("v", "f4", ("time", "zh", "yf", "xh"))
    v.long_name = "v velocity"
    v.units = "m/s"
    v[:] = np.broadcast_to(
        np.array([0.0, 4.0, 8.0], dtype=np.float32)[None, None, :, None],
        (2, 2, 3, 3),
    )
    w = nc.createVariable("w", "f4", ("time", "zf", "yh", "xh"))
    w.long_name = "w velocity"
    w.units = "m/s"
    w[:] = np.broadcast_to(
        np.array([0.0, 10.0, 20.0], dtype=np.float32)[None, :, None, None],
        (2, 3, 2, 3),
    )

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

with netCDF4.Dataset(LEGACY_OUT, "w", format="NETCDF3_64BIT_OFFSET") as nc:
    for name, length in (
        ("ni", 3),
        ("nj", 2),
        ("nk", 2),
        ("nip1", 4),
        ("njp1", 3),
        ("nkp1", 3),
        ("one", 1),
    ):
        nc.createDimension(name, length)
    nc.createDimension("time", None)

    axes = {
        "xh": ("ni", [-1.0, 0.0, 1.0], "west-east location of scalar grid points"),
        "xf": (
            "nip1",
            [-1.5, -0.5, 0.5, 1.5],
            "west-east location of staggered u grid points",
        ),
        "yh": ("nj", [-0.5, 0.5], "south-north location of scalar grid points"),
        "yf": (
            "njp1",
            [-1.0, 0.0, 1.0],
            "south-north location of staggered v grid points",
        ),
        "z": ("nk", [0.5, 1.5], "height of scalar grid points"),
        "zf": ("nkp1", [0.0, 1.0, 2.0], "height of staggered w grid points"),
    }
    for name, (dimension, values, long_name) in axes.items():
        var = nc.createVariable(name, "f4", (dimension,))
        var.long_name = long_name
        var.units = "km"
        var[:] = values

    time = nc.createVariable("time", "f4", ("time",))
    time.long_name = "time since beginning of simulation"
    time.units = "minutes"
    time[:] = [0.0, 1.0]

    scalar = nc.createVariable(
        "custom_legacy", "f4", ("time", "nk", "nj", "ni")
    )
    scalar.long_name = "fixture arbitrary legacy scalar"
    scalar.units = "K"
    scalar[:] = np.array(
        [
            [[[0, 1, 2], [3, 4, 5]], [[10, 11, 12], [13, 14, 15]]],
            [[[100, 101, 102], [103, 104, 105]], [[110, 111, 112], [113, 114, 115]]],
        ],
        dtype=np.float32,
    )
    scalar[1, 1, 1, 2] = np.float32(-999999.9)

    u = nc.createVariable("u", "f4", ("time", "nk", "nj", "nip1"))
    u.units = "m/s"
    u[:] = 0.0
    v = nc.createVariable("v", "f4", ("time", "nk", "njp1", "ni"))
    v.units = "m/s"
    v[:] = 0.0
    w = nc.createVariable("w", "f4", ("time", "nkp1", "nj", "ni"))
    w.units = "m/s"
    w[:] = 0.0

    nc.setncattr("cm1 version", "cm1r19.9")
    nc.setncattr("missing_value", np.float32(-999999.9))
    nc.setncattr("nx", np.int32(3))
    nc.setncattr("ny", np.int32(2))
    nc.setncattr("nz", np.int32(2))
    nc.setncattr("imoist", np.int32(1))
    nc.setncattr("ptype", np.int32(5))
    nc.setncattr("iorigin", np.int32(1))

for diag_path, elapsed_seconds, east_m, north_m in DIAGNOSTIC_OUTPUTS:
    with netCDF4.Dataset(diag_path, "w", format="NETCDF3_64BIT_OFFSET") as nc:
        nc.createDimension("xh", 1)
        nc.createDimension("yh", 1)
        nc.createDimension("zh", 2)
        nc.createDimension("zf", 3)
        nc.createDimension("time", 1)
        for name, dimension, values, long_name, units in (
            ("xh", "xh", [0.0], "west-east location", "degree_east"),
            ("yh", "yh", [0.0], "south-north location", "degree_north"),
            ("zh", "zh", [500.0, 1500.0], "height of scalar levels", "m"),
            ("zf", "zf", [0.0, 1000.0, 2000.0], "height of w levels", "m"),
        ):
            var = nc.createVariable(name, "f4", (dimension,))
            var.long_name = long_name
            var.units = units
            var[:] = values
        time = nc.createVariable("time", "f4", ("time",))
        time.long_name = "time"
        time.units = "seconds"
        time[:] = [elapsed_seconds]
        for name, value, long_name, units in (
            ("umove", 12.5, "umove", "m/s"),
            ("vmove", 3.0, "vmove", "m/s"),
            ("domainlocx", east_m, "x location of (center of) domain", "m"),
            ("domainlocy", north_m, "y location of (center of) domain", "m"),
        ):
            var = nc.createVariable(name, "f4", ("time", "yh", "xh"))
            var.long_name = long_name
            var.units = units
            var[:] = value
        nc.setncattr("CM1 version", "cm1r21.1")
        nc.setncattr("Conventions", "CF-1.7")
        nc.setncattr("missing_value", np.float32(-999999.9))

print(OUT)
print(LEGACY_OUT)
for path, *_ in DIAGNOSTIC_OUTPUTS:
    print(path)
