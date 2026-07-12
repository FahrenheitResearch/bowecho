from __future__ import annotations

import copy
import importlib.util
import math
import struct
import tempfile
import unittest
from pathlib import Path


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
            counts[config["table_id"]] = len(list(GENERATOR.point_coordinates(config)))
        self.assertEqual(sorted(counts.values()), [48, 87, 575])

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


if __name__ == "__main__":
    unittest.main()
