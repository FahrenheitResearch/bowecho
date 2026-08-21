#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import subprocess
import sys
import unittest


TOOLS_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(TOOLS_DIR))

from macos_plist_versions import derive_plist_versions  # noqa: E402


class PlistVersionTests(unittest.TestCase):
    def test_stable_version_stays_numeric(self) -> None:
        versions = derive_plist_versions("0.34.14")
        self.assertEqual(versions.short_version, "0.34.14")
        self.assertEqual(versions.bundle_version, "0.34.14")
        self.assertEqual(versions.human_version, "0.34.14")

    def test_prerelease_uses_numeric_core_and_preserves_human_version(self) -> None:
        versions = derive_plist_versions("v0.34.14-rc.1")
        self.assertEqual(versions.short_version, "0.34.14")
        self.assertEqual(versions.bundle_version, "0.34.14")
        self.assertEqual(versions.human_version, "0.34.14-rc.1")

    def test_build_metadata_is_preserved_only_in_human_version(self) -> None:
        versions = derive_plist_versions("1.2.3-beta.2+ci.47")
        self.assertEqual(versions.short_version, "1.2.3")
        self.assertEqual(versions.bundle_version, "1.2.3")
        self.assertEqual(versions.human_version, "1.2.3-beta.2+ci.47")

    def test_rejects_non_semver_or_noncanonical_numeric_identifiers(self) -> None:
        for invalid in (
            "",
            "0.34",
            "0.34.14.1",
            "00.34.14",
            "0.34.14-01",
            "0.34.14-rc_1",
            "10000.1.1",
            "1.100.1",
            "1.1.100",
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValueError):
                    derive_plist_versions(invalid)

    def test_cli_emits_one_tab_delimited_record(self) -> None:
        result = subprocess.run(
            [sys.executable, str(TOOLS_DIR / "macos_plist_versions.py"), "0.34.14-rc.1"],
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.stdout, "0.34.14\t0.34.14\t0.34.14-rc.1\n")


if __name__ == "__main__":
    unittest.main()
