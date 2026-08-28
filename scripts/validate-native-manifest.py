#!/usr/bin/env python3
"""Validate native pins against Cargo.lock and the six release targets."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path

from toml_compat import tomllib

SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")
EXPECTED_PLATFORMS = {"linux", "macos", "windows"}
EXPECTED_TARGET_COUNT = 6


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--native", type=Path, default=Path("native/dependencies.toml"))
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    parser.add_argument("--targets", type=Path, default=Path("release/targets.toml"))
    arguments = parser.parse_args()

    native = tomllib.loads(arguments.native.read_text(encoding="utf-8"))
    lockfile = tomllib.loads(arguments.lockfile.read_text(encoding="utf-8"))
    locked_versions = {(package["name"], package["version"]) for package in lockfile["package"]}
    cargo_metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1"], text=True
        )
    )
    package_metadata = {
        (package["name"], package["version"]): package for package in cargo_metadata["packages"]
    }
    for package in native["cargo_native"]:
        identity = (package["name"], package["version"])
        if identity not in locked_versions:
            raise SystemExit(f"native Cargo dependency is not locked: {identity[0]} {identity[1]}")
        actual_license = package_metadata[identity].get("license")
        if package["license"] != actual_license:
            raise SystemExit(
                f"license mismatch for {identity[0]}: expected {actual_license}, got {package['license']}"
            )
    for source in native["source"]:
        if not SHA256_PATTERN.fullmatch(source["sha256"]):
            raise SystemExit(f"invalid SHA-256 for {source['name']}")
        unknown_platforms = set(source["platforms"]) - EXPECTED_PLATFORMS
        if unknown_platforms:
            raise SystemExit(f"unknown platforms for {source['name']}: {sorted(unknown_platforms)}")

    targets = tomllib.loads(arguments.targets.read_text(encoding="utf-8"))["target"]
    triples = {target["triple"] for target in targets}
    platform_architectures = {(target["platform"], target["architecture"]) for target in targets}
    expected_platform_architectures = {
        (platform, architecture)
        for platform in EXPECTED_PLATFORMS
        for architecture in ("x86_64", "aarch64")
    }
    if len(triples) != EXPECTED_TARGET_COUNT or platform_architectures != expected_platform_architectures:
        raise SystemExit("release target manifest must contain x86-64 and ARM64 for all three platforms")
    for target in targets:
        if not target["required_bundled_libraries"] or not target["forbidden_dynamic_libraries"]:
            raise SystemExit(f"release target lacks dynamic-library policy: {target['triple']}")
        for library in target["implicit_bundled_libraries"]:
            if not any(
                required.lower() in library.lower()
                for required in target["required_bundled_libraries"]
            ):
                raise SystemExit(
                    f"implicit library is not required by target {target['triple']}: {library}"
                )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
