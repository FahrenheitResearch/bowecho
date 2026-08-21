#!/usr/bin/env python3
"""Derive plist-safe numeric versions from BowEcho's Cargo SemVer."""

from __future__ import annotations

from dataclasses import dataclass
import re
import sys


_NUMERIC = r"(?:0|[1-9][0-9]*)"
_NON_NUMERIC_PRERELEASE = r"(?:[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
_PRERELEASE_ID = rf"(?:{_NUMERIC}|{_NON_NUMERIC_PRERELEASE})"
_BUILD_ID = r"[0-9A-Za-z-]+"
_SEMVER = re.compile(
    rf"^(?P<major>{_NUMERIC})\."
    rf"(?P<minor>{_NUMERIC})\."
    rf"(?P<patch>{_NUMERIC})"
    rf"(?:-(?P<prerelease>{_PRERELEASE_ID}(?:\.{_PRERELEASE_ID})*))?"
    rf"(?:\+(?P<build>{_BUILD_ID}(?:\.{_BUILD_ID})*))?$"
)


@dataclass(frozen=True)
class PlistVersions:
    short_version: str
    bundle_version: str
    human_version: str


def derive_plist_versions(raw_version: str) -> PlistVersions:
    """Return numeric plist versions plus the complete human Cargo version."""

    human_version = raw_version.strip()
    if human_version.startswith("v"):
        human_version = human_version[1:]
    match = _SEMVER.fullmatch(human_version)
    if match is None:
        raise ValueError(f"not a valid Cargo SemVer: {raw_version!r}")

    components = (match.group("major"), match.group("minor"), match.group("patch"))
    # CFBundleVersion allows at most four digits in its first numeric component
    # and two in each following component. BowEcho's 0.x version line is valid;
    # reject a future out-of-range version instead of emitting an invalid plist.
    if len(components[0]) > 4 or any(
        len(component) > 2 for component in components[1:]
    ):
        raise ValueError(f"Cargo SemVer exceeds CFBundleVersion component widths: {raw_version!r}")

    # Apple's public bundle-version keys are numeric. The full prerelease and
    # build identity is carried separately in CFBundleGetInfoString and the
    # BowEchoFullVersion custom key.
    numeric_core = ".".join(components)
    return PlistVersions(
        short_version=numeric_core,
        bundle_version=numeric_core,
        human_version=human_version,
    )


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <cargo-semver>", file=sys.stderr)
        return 2
    try:
        versions = derive_plist_versions(argv[1])
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2
    print(
        "\t".join(
            (
                versions.short_version,
                versions.bundle_version,
                versions.human_version,
            )
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
