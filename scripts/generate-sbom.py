#!/usr/bin/env python3
"""Generate a CycloneDX SBOM for the helper and pinned native components."""

from __future__ import annotations

import argparse
import datetime
import json
import os
import subprocess
import urllib.parse
import uuid
from pathlib import Path

from toml_compat import tomllib


def timestamp() -> str:
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    value = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
    return value.isoformat().replace("+00:00", "Z")


def license_entry(expression: str | None) -> list[dict[str, str]]:
    return [{"expression": expression}] if expression else []


def cargo_purl(name: str, version: str) -> str:
    return f"pkg:cargo/{urllib.parse.quote(name)}@{urllib.parse.quote(version)}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--native-manifest", type=Path, default=Path("native/dependencies.toml"))
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()

    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                arguments.target,
            ],
            text=True,
            encoding="utf-8",
        )
    )
    package_by_id = {package["id"]: package for package in metadata["packages"]}
    node_by_id = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    helper = next(package for package in metadata["packages"] if package["name"] == "oxide-spice-helper")
    pending = [helper["id"]]
    included: set[str] = set()
    while pending:
        package_id = pending.pop()
        if package_id in included:
            continue
        included.add(package_id)
        pending.extend(node_by_id[package_id]["dependencies"])

    components = []
    for package_id in sorted(included, key=lambda value: package_by_id[value]["name"]):
        package = package_by_id[package_id]
        reference = cargo_purl(package["name"], package["version"])
        component = {
            "type": "library",
            "bom-ref": reference,
            "name": package["name"],
            "version": package["version"],
            "purl": reference,
            "licenses": license_entry(package.get("license")),
        }
        if package.get("repository"):
            component["externalReferences"] = [
                {"type": "vcs", "url": package["repository"]}
            ]
        components.append(component)

    native = tomllib.loads(arguments.native_manifest.read_text(encoding="utf-8"))
    platform = {
        "linux": "linux" in arguments.target,
        "macos": "apple-darwin" in arguments.target,
        "windows": "windows" in arguments.target,
    }
    selected_platform = next(name for name, matches in platform.items() if matches)
    for source in native["source"]:
        if selected_platform not in source["platforms"]:
            continue
        reference = f"pkg:generic/{source['name']}@{source['version']}"
        components.append(
            {
                "type": "library",
                "bom-ref": reference,
                "name": source["name"],
                "version": source["version"],
                "purl": reference,
                "hashes": [{"alg": "SHA-256", "content": source["sha256"]}],
                "licenses": license_entry(source["license"]),
                "externalReferences": [{"type": "distribution", "url": source["url"]}],
                "properties": [
                    {"name": "oxidespice:linkage", "value": source["linkage"]}
                ],
            }
        )

    serial = uuid.uuid5(uuid.NAMESPACE_URL, f"oxide-spice-helper:{helper['version']}:{arguments.target}")
    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": f"urn:uuid:{serial}",
        "version": 1,
        "metadata": {
            "timestamp": timestamp(),
            "component": {
                "type": "application",
                "name": "oxide-spice-helper",
                "version": helper["version"],
                "properties": [{"name": "oxidespice:target", "value": arguments.target}],
            },
        },
        "components": components,
        "dependencies": [
            {
                "ref": cargo_purl(package_by_id[package_id]["name"], package_by_id[package_id]["version"]),
                "dependsOn": [
                    cargo_purl(package_by_id[dependency]["name"], package_by_id[dependency]["version"])
                    for dependency in sorted(node_by_id[package_id]["dependencies"])
                    if dependency in included
                ],
            }
            for package_id in sorted(included)
        ],
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
