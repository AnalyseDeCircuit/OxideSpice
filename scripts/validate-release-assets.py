#!/usr/bin/env python3
"""Validate the complete signed helper asset set before publication."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tarfile
import tomllib
import zipfile
from pathlib import Path

SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")
REQUIRED_ARCHIVE_FILES = (
    "LICENSE",
    "THIRD-PARTY-NOTICES.md",
    "helper-metadata.json",
    "oxide-spice-helper.cdx.json",
)


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def archive_members(path: Path) -> dict[str, bytes]:
    wanted: dict[str, bytes] = {}
    if path.name.endswith(".tar.gz"):
        with tarfile.open(path, "r:gz") as archive:
            for member in archive.getmembers():
                if not member.isfile():
                    continue
                member_path = Path(member.name)
                if len(member_path.parts) != 2:
                    continue
                basename = member_path.name
                if basename not in REQUIRED_ARCHIVE_FILES:
                    continue
                if basename in wanted:
                    raise ValueError(f"duplicate {basename} in {path.name}")
                source = archive.extractfile(member)
                if source is not None:
                    wanted[basename] = source.read()
    elif path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for name in archive.namelist():
                member_path = Path(name)
                if len(member_path.parts) != 2:
                    continue
                basename = member_path.name
                if basename in REQUIRED_ARCHIVE_FILES and not name.endswith("/"):
                    if basename in wanted:
                        raise ValueError(f"duplicate {basename} in {path.name}")
                    wanted[basename] = archive.read(name)
    else:
        raise ValueError(f"unsupported helper archive: {path.name}")
    missing = sorted(set(REQUIRED_ARCHIVE_FILES) - wanted.keys())
    if missing:
        raise ValueError(f"{path.name} is missing required files: {', '.join(missing)}")
    return wanted


def validate_checksum(archive: Path) -> None:
    checksum = archive.with_name(archive.name + ".sha256")
    line = checksum.read_text(encoding="utf-8")
    expected_suffix = f"  {archive.name}\n"
    if not line.endswith(expected_suffix):
        raise ValueError(f"invalid checksum filename in {checksum.name}")
    expected_digest = line[: -len(expected_suffix)]
    if not SHA256_PATTERN.fullmatch(expected_digest):
        raise ValueError(f"invalid SHA-256 syntax in {checksum.name}")
    if expected_digest != file_sha256(archive):
        raise ValueError(f"SHA-256 mismatch for {archive.name}")


def validate_archive(
    archive: Path,
    target: str,
    version: str,
    contract: dict,
) -> None:
    members = archive_members(archive)
    metadata = json.loads(members["helper-metadata.json"])
    if metadata.get("helperVersion") != version:
        raise ValueError(f"helper version mismatch in {archive.name}")
    if metadata.get("target") != target:
        raise ValueError(f"helper target mismatch in {archive.name}")
    if metadata.get("ipcProtocolVersion") != contract["ipcProtocolVersion"]:
        raise ValueError(f"IPC protocol version mismatch in {archive.name}")
    if sorted(metadata.get("capabilities", [])) != sorted(contract["capabilities"]):
        raise ValueError(f"helper capability mismatch in {archive.name}")
    if not metadata.get("minimumSystemVersion") or not metadata.get("dynamicLibraries"):
        raise ValueError(f"incomplete runtime metadata in {archive.name}")

    sbom = json.loads(members["oxide-spice-helper.cdx.json"])
    component = sbom.get("metadata", {}).get("component", {})
    properties = {
        property_["name"]: property_["value"]
        for property_ in component.get("properties", [])
    }
    if sbom.get("bomFormat") != "CycloneDX" or component.get("version") != version:
        raise ValueError(f"SBOM version mismatch in {archive.name}")
    if properties.get("oxidespice:target") != target:
        raise ValueError(f"SBOM target mismatch in {archive.name}")
    if not members["LICENSE"].strip() or not members["THIRD-PARTY-NOTICES.md"].strip():
        raise ValueError(f"empty legal notice in {archive.name}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--contract", type=Path, required=True)
    parser.add_argument("--targets", type=Path, default=Path("release/targets.toml"))
    arguments = parser.parse_args()

    contract = json.loads(arguments.contract.read_text(encoding="utf-8"))
    version = contract["helperVersion"]
    expected_tag = f"v{version}"
    if arguments.tag != expected_tag:
        raise SystemExit(f"release tag must be {expected_tag}, got {arguments.tag}")

    targets = tomllib.loads(arguments.targets.read_text(encoding="utf-8"))["target"]
    expected_assets: set[str] = set()
    archives: list[tuple[Path, str]] = []
    for target in targets:
        archive_name = f"oxide-spice-helper-{target['triple']}.{target['archive']}"
        archive = arguments.directory / archive_name
        archives.append((archive, target["triple"]))
        expected_assets.update(
            (archive_name, f"{archive_name}.sha256", f"{archive_name}.sha256.minisig")
        )
    actual_assets = {path.name for path in arguments.directory.iterdir() if path.is_file()}
    if actual_assets != expected_assets:
        missing = sorted(expected_assets - actual_assets)
        unexpected = sorted(actual_assets - expected_assets)
        raise SystemExit(f"release asset set mismatch; missing={missing}, unexpected={unexpected}")

    for archive, target in archives:
        validate_checksum(archive)
        validate_archive(archive, target, version, contract)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
