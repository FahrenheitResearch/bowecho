"""Pure contract checks for exact-frequency T-matrix pack assembly."""

from __future__ import annotations

import copy
import hashlib
import unittest
from pathlib import Path
from unittest import mock

import generate_band_pack as pack
import generate_lut


def digest(label: str) -> str:
    return hashlib.sha256(label.encode("utf-8")).hexdigest()


def role_configs() -> dict[str, dict]:
    return {
        role: {
            "radar": {
                "solver": {"shape": "spheroid", "ddelt": 0.001, "ndgs": 14}
            },
            "orientation": {
                "model": "gaussian_canting",
                "mean_deg": 0.0,
                "standard_deviation_deg": 20.0,
                "alpha_quadrature_points": 5,
                "beta_quadrature_points": 10,
            },
        }
        for role in pack.ROLE_NAMES
    }


def role_files() -> list[dict]:
    return [
        {
            "role": spec.role,
            "lut_path": f"{spec.directory}/table.lut",
            "lut_sha256": digest(f"{spec.role}-lut"),
            "lut_bytes": index + 100,
            "config_path": f"{spec.directory}/config.json",
            "config_sha256": digest(f"{spec.role}-config"),
            "config_bytes": index + 10,
        }
        for index, spec in enumerate(pack.ROLE_SPECS)
    ]


class BandPackContractTests(unittest.TestCase):
    def test_exact_band_frequencies_are_not_interpolated(self) -> None:
        self.assertEqual(pack.exact_band_frequency_hz("s"), 2.8e9)
        self.assertEqual(pack.exact_band_frequency_hz("c"), 5.6e9)
        self.assertEqual(pack.exact_band_frequency_hz("x"), 9.4e9)
        with self.assertRaises(pack.PackGeneratorError):
            pack.exact_band_frequency_hz("5.5ghz")

    def test_c_and_x_manifests_are_always_unvalidated(self) -> None:
        provenance = pack.provenance_records(role_configs(), Path(__file__).parent)
        for band in ("c", "x"):
            manifest = pack.build_pack_manifest(
                band=band,
                pack_id=pack.default_pack_id(band),
                science_revision="unit-contract-v1",
                role_files=role_files(),
                provenance=provenance,
            )
            self.assertEqual(manifest["validation_status"], "unvalidated_research")
            self.assertEqual(
                manifest["frequency_hz"], pack.exact_band_frequency_hz(band)
            )
            promoted = copy.deepcopy(manifest)
            promoted["validation_status"] = "validated_research"
            with self.assertRaisesRegex(
                pack.PackGeneratorError, "separate convergence"
            ):
                pack.validate_pack_manifest(promoted)

    def test_provenance_hashes_are_canonical_and_deterministic(self) -> None:
        first = pack.provenance_records(role_configs(), Path(__file__).parent)
        second = pack.provenance_records(role_configs(), Path(__file__).parent)
        self.assertEqual(first, second)
        for field in ("generator_sha256", "solver_sha256", "odf_sha256"):
            self.assertRegex(first[field], r"^[0-9a-f]{64}$")

    def test_retarget_changes_only_the_declared_frequency_identity(self) -> None:
        template = {
            "table_id": "property-rain-sband-pytmatrix-0.3.3-unvalidated-v1",
            "axes": [
                {
                    "kind": "frequency",
                    "unit": "hertz",
                    "coordinates": [2.8e9],
                }
            ],
            "dielectric": {
                "model": generate_lut.TEMPERATURE_WATER_DIELECTRIC_MODEL,
                "frequency_range_hz": [2.0e9, 4.0e9],
            },
        }
        with mock.patch.object(generate_lut, "validate_config") as validate:
            retargeted = pack.retarget_role_config(template, "x")
        validate.assert_called_once_with(retargeted)
        self.assertEqual(retargeted["axes"][0]["coordinates"], [9.4e9])
        self.assertIn("-xband-", retargeted["table_id"])
        self.assertEqual(
            retargeted["dielectric"]["frequency_range_hz"], [2.0e9, 10.0e9]
        )
        self.assertEqual(template["axes"][0]["coordinates"], [2.8e9])

    def test_manifest_rejects_unsafe_or_duplicate_role_paths(self) -> None:
        provenance = pack.provenance_records(role_configs(), Path(__file__).parent)
        manifest = pack.build_pack_manifest(
            band="s",
            pack_id=pack.default_pack_id("s"),
            science_revision="unit-contract-v1",
            role_files=role_files(),
            provenance=provenance,
        )
        unsafe = copy.deepcopy(manifest)
        unsafe["role_files"][0]["lut_path"] = "../table.lut"
        with self.assertRaisesRegex(pack.PackGeneratorError, "unsafe"):
            pack.validate_pack_manifest(unsafe)
        duplicate = copy.deepcopy(manifest)
        duplicate["role_files"][1]["lut_path"] = duplicate["role_files"][0][
            "lut_path"
        ]
        with self.assertRaisesRegex(pack.PackGeneratorError, "duplicate"):
            pack.validate_pack_manifest(duplicate)


if __name__ == "__main__":
    unittest.main()
