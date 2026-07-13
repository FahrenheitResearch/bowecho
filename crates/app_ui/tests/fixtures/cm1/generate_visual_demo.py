"""Generate a visual CM1-schema BowEcho demo (synthetic data, never science)."""

from __future__ import annotations

import argparse
from pathlib import Path

import netCDF4
import numpy as np


def build_demo(output: Path) -> None:
    nx = 81
    ny = 81
    nz = 24
    times_s = np.asarray([0.0, 300.0], dtype=np.float32)
    x_km = np.linspace(-40.0, 40.0, nx, dtype=np.float32)
    y_km = np.linspace(-40.0, 40.0, ny, dtype=np.float32)
    xf_km = np.linspace(-40.5, 40.5, nx + 1, dtype=np.float32)
    yf_km = np.linspace(-40.5, 40.5, ny + 1, dtype=np.float32)
    z_km = (0.25 + 0.5 * np.arange(nz)).astype(np.float32)
    zf_km = (0.5 * np.arange(nz + 1)).astype(np.float32)
    xx, yy = np.meshgrid(x_km, y_km, indexing="xy")

    output.parent.mkdir(parents=True, exist_ok=True)
    with netCDF4.Dataset(output, "w", format="NETCDF4_CLASSIC") as nc:
        for name, length in (
            ("xh", nx),
            ("xf", nx + 1),
            ("yh", ny),
            ("yf", ny + 1),
            ("zh", nz),
            ("zf", nz + 1),
            ("time", 2),
        ):
            nc.createDimension(name, length)

        for name, values, long_name in (
            ("xh", x_km, "west-east location of scalar grid points"),
            ("xf", xf_km, "west-east location of staggered u grid points"),
            ("yh", y_km, "south-north location of scalar grid points"),
            ("yf", yf_km, "south-north location of staggered v grid points"),
            ("zh", z_km, "nominal height of scalar grid points"),
            ("zf", zf_km, "nominal height of staggered w grid points"),
        ):
            variable = nc.createVariable(name, "f4", (name,))
            variable.units = "km"
            variable.long_name = long_name
            variable[:] = values

        time = nc.createVariable("time", "f4", ("time",))
        time.units = "seconds"
        time.long_name = "time since beginning of simulation"
        time[:] = times_s

        shape = (len(times_s), nz, ny, nx)
        fields = {}
        for name, units, long_name in (
            ("dbz", "dBZ", "reflectivity"),
            ("th", "K", "potential temperature"),
            ("prs", "Pa", "pressure"),
            ("qv", "kg/kg", "water vapor mixing ratio"),
            ("uinterp", "m/s", "u interpolated to scalar points (grid-relative)"),
            ("vinterp", "m/s", "v interpolated to scalar points (grid-relative)"),
            ("winterp", "m/s", "w interpolated to scalar points"),
            ("zhval", "m", "height on model levels"),
        ):
            variable = nc.createVariable(
                name,
                "f4",
                ("time", "zh", "yh", "xh"),
                zlib=True,
                complevel=4,
                shuffle=True,
            )
            variable.units = units
            variable.long_name = long_name
            fields[name] = variable

        cref = nc.createVariable(
            "cref", "f4", ("time", "yh", "xh"), zlib=True, complevel=4
        )
        cref.units = "dBZ"
        cref.long_name = "composite reflectivity"
        zs = nc.createVariable(
            "zs", "f4", ("time", "yh", "xh"), zlib=True, complevel=4
        )
        zs.units = "m"
        zs.long_name = "terrain height"
        zs[:] = np.float32(0.0)

        for time_index in range(len(times_s)):
            center_x = -7.0 + 6.0 * time_index
            center_y = -2.0 + 3.0 * time_index
            dx = xx - center_x
            dy = yy - center_y
            radius = np.hypot(dx, dy) + 1.0e-3
            angle = np.arctan2(dy, dx)

            dbz_volume = np.empty((nz, ny, nx), dtype=np.float32)
            th_volume = np.empty_like(dbz_volume)
            prs_volume = np.empty_like(dbz_volume)
            qv_volume = np.empty_like(dbz_volume)
            u_volume = np.empty_like(dbz_volume)
            v_volume = np.empty_like(dbz_volume)
            w_volume = np.empty_like(dbz_volume)
            height_volume = np.empty_like(dbz_volume)

            for level, height_km in enumerate(z_km):
                tilted_x = center_x + 0.9 * height_km
                tilted_y = center_y + 0.25 * height_km
                storm_r = np.hypot(xx - tilted_x, yy - tilted_y)
                core = np.exp(-((storm_r / (8.5 + 0.2 * height_km)) ** 2))
                anvil = np.exp(-((storm_r / 18.0) ** 2)) * np.exp(
                    -(((height_km - 9.0) / 2.3) ** 2)
                )
                hook_radius = 10.0 + 1.2 * np.sin(angle + 0.6)
                hook = np.exp(-(((radius - hook_radius) / 2.0) ** 2)) * (
                    (angle < 0.5) & (angle > -2.5)
                )
                vertical_decay = np.exp(-((height_km / 10.5) ** 2))
                dbz_volume[level] = np.clip(
                    -12.0
                    + 68.0 * core * vertical_decay
                    + 28.0 * anvil
                    + 18.0 * hook * np.exp(-height_km / 2.5),
                    -12.0,
                    70.0,
                )

                tangential = 28.0 * (1.0 - np.exp(-radius / 3.0)) * np.exp(
                    -radius / 18.0
                ) * np.exp(-height_km / 10.0)
                radial_inflow = -8.0 * np.exp(-radius / 14.0) * np.exp(
                    -height_km / 2.0
                )
                u_volume[level] = (
                    11.0
                    - tangential * dy / radius
                    + radial_inflow * dx / radius
                    + 0.7 * height_km
                )
                v_volume[level] = (
                    4.0
                    + tangential * dx / radius
                    + radial_inflow * dy / radius
                    + 0.2 * height_km
                )
                w_volume[level] = (
                    28.0
                    * core
                    * np.exp(-(((height_km - 5.0) / 3.0) ** 2))
                    - 4.0
                    * np.exp(-(((storm_r - 15.0) / 5.0) ** 2))
                    * np.exp(-height_km / 3.0)
                )
                th_volume[level] = (
                    299.0 + 3.2 * height_km + 2.0 * core * np.exp(-height_km / 5.0)
                )
                prs_volume[level] = 100000.0 * np.exp(-height_km / 8.2)
                qv_volume[level] = (
                    0.015 * np.exp(-height_km / 2.2) + 0.0015 * core
                )
                height_volume[level] = height_km * 1000.0

            fields["dbz"][time_index] = dbz_volume
            fields["th"][time_index] = th_volume
            fields["prs"][time_index] = prs_volume
            fields["qv"][time_index] = qv_volume
            fields["uinterp"][time_index] = u_volume
            fields["vinterp"][time_index] = v_volume
            fields["winterp"][time_index] = w_volume
            fields["zhval"][time_index] = height_volume
            cref[time_index] = np.max(dbz_volume, axis=0)

        nc.setncattr("CM1 version", "cm1r21.1-schema-demo")
        nc.setncattr("Conventions", "CF-1.7")
        nc.setncattr("title", "BowEcho synthetic CM1 radar UI demonstration")
        nc.setncattr(
            "warning",
            "Synthetic schema-compatible test data; not a CM1 simulation and not scientific output",
        )
        nc.setncattr("missing_value", np.float32(-1.0e30))
        nc.setncattr("x_units", "km")
        nc.setncattr("y_units", "km")
        nc.setncattr("z_units", "km")
        nc.setncattr("nx", np.int32(nx))
        nc.setncattr("ny", np.int32(ny))
        nc.setncattr("nz", np.int32(nz))
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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    build_demo(args.output)
    print(args.output)


if __name__ == "__main__":
    main()
