from __future__ import annotations

import copy
import importlib.util
import itertools
import math
import struct
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


REPO = Path(__file__).resolve().parents[2]
TOOL = REPO / "crates/radar_scattering/tools/pytmatrix-0.3.3/generate_lut.py"
ASSETS = REPO / "research_only_assets/tmatrix/pytmatrix-0.3.3"
SPEC = importlib.util.spec_from_file_location("brslut_generator_under_test", TOOL)
assert SPEC is not None and SPEC.loader is not None
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)


class GeneratorContractTests(unittest.TestCase):
    def configs(self):
        return sorted(ASSETS.glob("*/config.json"))

    def test_all_research_configs_are_strict_and_complete(self):
        counts = {}
        for path in self.configs():
            _, _, config = GENERATOR.parse_exact_json(path)
            GENERATOR.validate_config(config)
            expected_ndgs = 14 if GENERATOR._uses_material_state_grouping(config) else 2
            self.assertEqual(config["radar"]["solver"]["ndgs"], expected_ndgs)
            self.assertEqual(config["radar"]["solver"]["ddelt"], 0.001)
            counts[config["table_id"]] = len(list(GENERATOR.point_coordinates(config)))
        self.assertEqual(
            sorted(counts.values()),
            [48, 87, 1400, 2960, 132160, 163520, 864000, 1017600],
        )

    def test_parallel_group_generation_restores_flat_grid_order(self):
        path = ASSETS / "property_p3_ishmael_dry_oblate_sband_unvalidated/config.json"
        _, _, config = GENERATOR.parse_exact_json(path)
        base = {
            axis["kind"]: float(axis["coordinates"][0]) for axis in config["axes"]
        }
        points = []
        for temperature in (190.0, 230.0, 260.0):
            point = dict(base)
            point["temperature"] = temperature
            points.append(point)

        def fake_group(_config, group_points, _timeout):
            temperature = group_points[0]["temperature"]
            time.sleep((300.0 - temperature) * 0.0001)
            return [[temperature] * 9 for _ in group_points]

        with mock.patch.object(
            GENERATOR, "run_isolated_material_state_group", side_effect=fake_group
        ):
            values, process_count, maximum_points = GENERATOR._compute_isolated_grid(
                config, points
            )
        self.assertEqual([value[0] for value in values], [190.0, 230.0, 260.0])
        self.assertEqual(process_count, 3)
        self.assertEqual(maximum_points, 1)

    def test_duplicate_and_nonfinite_json_are_rejected(self):
        samples = (
            b'{"schema":1,"schema":1}',
            b'{"value":NaN}',
            b'{"value":Infinity}',
        )
        with tempfile.TemporaryDirectory() as directory:
            for index, sample in enumerate(samples):
                path = Path(directory) / f"bad-{index}.json"
                path.write_bytes(sample)
                with self.assertRaises(GENERATOR.GeneratorError):
                    GENERATOR.parse_exact_json(path)

    def test_axis_ratio_mapping_distinguishes_oblate_and_prolate(self):
        self.assertAlmostEqual(
            GENERATOR._pytmatrix_axis_ratio("oblate_spheroid", 0.7), 1.0 / 0.7
        )
        self.assertAlmostEqual(
            GENERATOR._pytmatrix_axis_ratio("prolate_spheroid", 0.7), 0.7
        )
        self.assertEqual(GENERATOR._pytmatrix_axis_ratio("oblate_spheroid", 1.0), 1.0)

    def test_python_rust_handoff_uses_exact_ieee_bits(self):
        for value in (0.0, -0.0, 1.1685896132045696e-7, 1.0e300):
            encoded = GENERATOR.f64_bits_hex(value)
            self.assertEqual(len(encoded), 16)
            decoded = struct.unpack("<d", struct.pack("<Q", int(encoded, 16)))[0]
            self.assertEqual(struct.pack("<d", value), struct.pack("<d", decoded))

    def test_maxwell_garnett_endpoints_and_mixture_density(self):
        path = ASSETS / "conventional_wet_hail_sband_unvalidated/config.json"
        _, _, config = GENERATOR.parse_exact_json(path)
        dry_coordinates = {
            "equivolume_diameter": 0.02,
            "liquid_mass_fraction": 0.0,
            "minor_to_major_axis_ratio": 1.0,
            "frequency": 2700832954.954955,
        }
        wet_coordinates = copy.deepcopy(dry_coordinates)
        wet_coordinates["liquid_mass_fraction"] = 1.0
        dry_index, dry_density = GENERATOR._material(config, dry_coordinates)
        wet_index, wet_density = GENERATOR._material(config, wet_coordinates)
        expected_ice = GENERATOR._complex_index(
            config["dielectric"]["ice_refractive_index"], "ice"
        )
        expected_water = GENERATOR._complex_index(
            config["dielectric"]["liquid_water_refractive_index"], "water"
        )
        self.assertLess(abs(dry_index - expected_ice), 1.0e-12)
        self.assertLess(abs(wet_index - expected_water), 1.0e-12)
        self.assertEqual(dry_density, config["dielectric"]["ice_density_kg_m3"])
        self.assertAlmostEqual(
            wet_density, config["dielectric"]["liquid_water_density_kg_m3"]
        )

    def test_terminal_moments_are_nonzero_and_zero_variance(self):
        path = ASSETS / "conventional_dry_ice_spheroids_sband_unvalidated/config.json"
        _, _, config = GENERATOR.parse_exact_json(path)
        speed = GENERATOR._terminal_speed(config, 0.01, 916.7)
        self.assertTrue(math.isfinite(speed) and speed > 0.0)
        zh = 12.5
        components = [zh, zh, zh, 0.0, 0.0, 0.0, 0.0, zh * speed, zh * speed**2]
        GENERATOR.validate_components(components)

    def test_terminal_speed_bisection_covers_refined_wet_transition_states(self):
        path = ASSETS / "property_p3_ishmael_wet_oblate_sband_unvalidated/config.json"
        _, _, config = GENERATOR.parse_exact_json(path)
        cases = (
            (0.004677021438182558, 0.014089831333721728),
            (0.31608446017210634, 0.0031031676915590912),
        )
        for condensed_fraction, diameter in cases:
            _, density = GENERATOR._material(
                config,
                {
                    "temperature": 269.15,
                    "condensed_volume_fraction": condensed_fraction,
                    "liquid_mass_fraction": 0.2,
                    "frequency": 2.8e9,
                },
            )
            speed = GENERATOR._terminal_speed(config, diameter, density)
            self.assertTrue(math.isfinite(speed))
            self.assertGreater(speed, 0.0)
            reynolds = (
                config["terminal_velocity"]["air_density_kg_m3"]
                * speed
                * diameter
                / config["terminal_velocity"]["air_dynamic_viscosity_pa_s"]
            )
            self.assertAlmostEqual(
                reynolds,
                config["terminal_velocity"]["drag_transition_reynolds"],
                places=9,
            )
            self.assertEqual(
                config["terminal_velocity"]["drag_transition_boundary_policy"],
                "select_exact_transition_reynolds_boundary_when_piecewise_drag_"
                "residual_jump_straddles_zero",
            )
            terminal = config["terminal_velocity"]
            transition = terminal["drag_transition_reynolds"]
            low_drag = (24.0 / transition) * (
                1.0 + 0.15 * transition**0.687
            )
            force_scale = (
                4.0
                * terminal["gravity_m_s2"]
                * diameter
                * (density - terminal["air_density_kg_m3"])
                / (3.0 * terminal["air_density_kg_m3"])
            )
            low_residual = speed * speed * low_drag - force_scale
            high_residual = (
                speed * speed * terminal["high_reynolds_drag_coefficient"]
                - force_scale
            )
            self.assertLessEqual(low_residual, 0.0)
            self.assertGreaterEqual(high_residual, 0.0)

    def test_all_configured_terminal_states_complete(self):
        evaluated = 0
        for path in self.configs():
            _, _, config = GENERATOR.parse_exact_json(path)
            if config["terminal_velocity"]["law"] != "schiller_naumann_gravity_drag":
                continue
            axes = {axis["kind"]: axis["coordinates"] for axis in config["axes"]}
            base = {kind: float(values[0]) for kind, values in axes.items()}
            grouping = config["execution"].get("grouping")
            if grouping is None:
                material_kinds = tuple(
                    kind
                    for kind in axes
                    if kind
                    not in {
                        "equivolume_diameter",
                        "minor_to_major_axis_ratio",
                        "radar_elevation",
                    }
                )
            else:
                material_kinds = tuple(grouping["material_state_axis_kinds"])
            for material_values in itertools.product(
                *([float(value) for value in axes[kind]] for kind in material_kinds)
            ):
                coordinates = dict(base)
                coordinates.update(dict(zip(material_kinds, material_values)))
                _, density = GENERATOR._material(config, coordinates)
                for diameter in axes["equivolume_diameter"]:
                    speed = GENERATOR._terminal_speed(config, float(diameter), density)
                    self.assertTrue(math.isfinite(speed))
                    self.assertGreater(speed, 0.0)
                    evaluated += 1
        self.assertGreater(evaluated, 100_000)

    def test_temperature_dependent_permittivity_golden_values(self):
        frequency = 2700832954.954955
        ice = GENERATOR._ice_permittivity_matzler_2006(273.15, frequency)
        water = GENERATOR._water_permittivity_liebe_1991(273.15, frequency)
        self.assertAlmostEqual(ice.real, 3.1885365, places=10)
        self.assertAlmostEqual(ice.imag, 0.00048568435, places=10)
        self.assertAlmostEqual(water.real, 80.8523695, places=6)
        self.assertAlmostEqual(water.imag, 22.8676989, places=6)

    def test_component_volume_fractions_round_trip_and_reject_overfill(self):
        bulk_density = 400.0
        liquid_mass_fraction = 0.25
        air, ice, water = GENERATOR._component_volume_fractions(
            bulk_density, liquid_mass_fraction, 917.0, 999.84
        )
        self.assertAlmostEqual(air + ice + water, 1.0)
        self.assertAlmostEqual(ice * 917.0 + water * 999.84, bulk_density)
        self.assertAlmostEqual(water * 999.84 / bulk_density, liquid_mass_fraction)
        wet_fractions, reconstructed = GENERATOR._wet_component_volume_fractions(
            0.5, liquid_mass_fraction, 917.0, 999.84
        )
        self.assertAlmostEqual(sum(wet_fractions), 1.0)
        self.assertAlmostEqual(
            wet_fractions[1] * 917.0 + wet_fractions[2] * 999.84,
            reconstructed,
        )
        self.assertAlmostEqual(
            wet_fractions[2] * 999.84 / reconstructed,
            liquid_mass_fraction,
        )
        with self.assertRaises(GENERATOR.GeneratorError):
            GENERATOR._component_volume_fractions(1000.0, 0.0, 917.0, 999.84)

    def test_symmetric_bruggeman_endpoints_permutations_and_dry_quadratic(self):
        air = complex(1.0, 0.0)
        ice = GENERATOR._ice_permittivity_matzler_2006(250.0, 2.8e9)
        water = GENERATOR._water_permittivity_liebe_1991(273.15, 2.8e9)
        common = {
            "homotopy_steps": 64,
            "maximum_iterations": 100,
            "tolerance": 1.0e-12,
        }
        for component in (air, ice, water):
            endpoint = GENERATOR._symmetric_bruggeman_permittivity(
                (component,), (1.0,), **common
            )
            self.assertEqual(endpoint, component)

        fractions = (0.45, 0.40, 0.15)
        reference = GENERATOR._symmetric_bruggeman_permittivity(
            (air, ice, water), fractions, **common
        )
        for permutation in itertools.permutations(range(3)):
            actual = GENERATOR._symmetric_bruggeman_permittivity(
                tuple((air, ice, water)[index] for index in permutation),
                tuple(fractions[index] for index in permutation),
                **common,
            )
            self.assertLess(abs(actual - reference), 1.0e-10)

        ice_fraction = 0.4
        dry = GENERATOR._symmetric_bruggeman_permittivity(
            (air, ice), (1.0 - ice_fraction, ice_fraction), **common
        )
        coefficient = (
            (3.0 * ice_fraction - 1.0) * ice
            + (3.0 * (1.0 - ice_fraction) - 1.0) * air
        )
        discriminant = (coefficient * coefficient + 8.0 * ice * air) ** 0.5
        roots = ((coefficient + discriminant) / 4.0, (coefficient - discriminant) / 4.0)
        self.assertLess(min(abs(dry - root) for root in roots), 1.0e-10)
        self.assertLess(
            abs(GENERATOR._bruggeman_residual(dry, (air, ice), (0.6, 0.4))),
            1.0e-12,
        )

    def test_property_materials_are_passive_and_keep_bulk_density(self):
        cases = (
            (
                "property_p3_ishmael_dry_oblate_sband_unvalidated",
                {
                    "equivolume_diameter": 0.01,
                    "temperature": 230.0,
                    "bulk_density": 400.0,
                    "minor_to_major_axis_ratio": 0.8,
                    "frequency": 2.8e9,
                    "radar_elevation": 4.5,
                },
            ),
            (
                "property_p3_ishmael_wet_oblate_sband_unvalidated",
                {
                    "equivolume_diameter": 0.01,
                    "temperature": 273.15,
                    "condensed_volume_fraction": 0.5,
                    "liquid_mass_fraction": 0.2,
                    "minor_to_major_axis_ratio": 0.8,
                    "frequency": 2.8e9,
                    "radar_elevation": 4.5,
                },
            ),
        )
        for directory, coordinates in cases:
            _, _, config = GENERATOR.parse_exact_json(ASSETS / directory / "config.json")
            refractive_index, density = GENERATOR._material(config, coordinates)
            if "bulk_density" in coordinates:
                self.assertEqual(density, coordinates["bulk_density"])
            else:
                self.assertGreater(density, 1.225)
            self.assertGreater(refractive_index.real, 0.0)
            self.assertGreaterEqual(refractive_index.imag, 0.0)

    def test_residual_rain_mass_is_allocated_once(self):
        self.assertEqual(GENERATOR.residual_rain_mass_after_wet_pairing(1.0, []), 1.0)
        self.assertAlmostEqual(
            GENERATOR.residual_rain_mass_after_wet_pairing(1.0, [0.2, 0.3]), 0.5
        )
        self.assertEqual(
            GENERATOR.residual_rain_mass_after_wet_pairing(1.0, [0.4, 0.6]), 0.0
        )
        with self.assertRaises(GENERATOR.GeneratorError):
            GENERATOR.residual_rain_mass_after_wet_pairing(1.0, [0.8, 0.3])

    def test_elevation_geometry_and_signed_kdp_contract(self):
        path = ASSETS / "property_p3_ishmael_dry_oblate_sband_unvalidated/config.json"
        _, _, config = GENERATOR.parse_exact_json(path)
        back, forward = GENERATOR._radar_geometries(
            config, {"radar_elevation": 19.5}
        )
        self.assertEqual(back, (70.5, 109.5, 0.0, 180.0, 0.0, 0.0))
        self.assertEqual(forward, (70.5, 70.5, 0.0, 0.0, 0.0, 0.0))
        GENERATOR.validate_components([1.0, 1.0, 1.0, 0.0, -2.0, 0.0, 0.0, 1.0, 1.0])


if __name__ == "__main__":
    unittest.main()
