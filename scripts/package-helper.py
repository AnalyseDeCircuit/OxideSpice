#!/usr/bin/env python3
"""Assemble and audit one complete helper artifact directory."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
from collections import deque
from pathlib import Path

from toml_compat import tomllib


def command_output(command: list[str], *, encoding: str | None = None) -> str:
    return subprocess.check_output(
        command, text=True, encoding=encoding, stderr=subprocess.STDOUT
    )


def binary_dependencies(path: Path, platform: str) -> list[str]:
    if platform == "linux":
        return [line.strip() for line in command_output(["patchelf", "--print-needed", str(path)]).splitlines() if line.strip()]
    if platform == "macos":
        lines = command_output(["otool", "-L", str(path)]).splitlines()[1:]
        return [line.strip().split(" ", 1)[0] for line in lines if line.strip()]
    output = command_output(["dumpbin", "/dependents", str(path)])
    dependencies = []
    for line in output.splitlines():
        name = line.strip()
        if name.lower().endswith(".dll") and not any(character.isspace() for character in name):
            dependencies.append(name)
    return dependencies


def find_library(name: str, roots: list[Path]) -> Path | None:
    dependency_path = Path(name)
    if dependency_path.is_absolute():
        resolved = dependency_path.resolve()
        if dependency_path.is_file() and any(
            root.resolve() == resolved or root.resolve() in resolved.parents for root in roots
        ):
            return dependency_path
        return None
    basename = Path(name).name
    candidates = [
        candidate for root in roots for candidate in root.rglob(basename) if candidate.is_file()
    ]
    if not candidates and ".so." in basename:
        candidates = [
            candidate
            for root in roots
            for candidate in root.rglob(basename.split(".so.", 1)[0] + ".so*")
            if candidate.is_file()
        ]
    return sorted(candidates, key=lambda path: len(path.parts))[0] if candidates else None


def copy_dependency_closure(
    helper: Path,
    roots: list[Path],
    destination: Path,
    platform: str,
    implicit_libraries: list[str],
) -> tuple[list[str], list[Path]]:
    pending = deque([helper])
    inspected: set[Path] = set()
    dependency_names: set[str] = set()
    bundled: dict[str, Path] = {}
    for name in implicit_libraries:
        source = find_library(name, roots)
        if source is None:
            raise SystemExit(f"implicit runtime library is missing: {name}")
        output = destination / Path(name).name
        shutil.copy2(source.resolve(), output)
        bundled[output.name] = output
        dependency_names.add(output.name)
        pending.append(output)
    while pending:
        binary = pending.popleft()
        resolved = binary.resolve()
        if resolved in inspected:
            continue
        inspected.add(resolved)
        for dependency in binary_dependencies(binary, platform):
            name = Path(dependency).name
            dependency_names.add(name)
            source = find_library(dependency, roots)
            if source is None or name in bundled:
                continue
            output = destination / name
            shutil.copy2(source.resolve(), output)
            bundled[name] = output
            pending.append(output)
    return sorted(dependency_names), sorted(bundled.values())


def patch_runtime_paths(helper: Path, libraries: list[Path], platform: str) -> None:
    if platform == "linux":
        subprocess.run(["patchelf", "--set-rpath", "$ORIGIN/../lib", str(helper)], check=True)
        for library in libraries:
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(library)], check=True)
    elif platform == "macos":
        subprocess.run(["install_name_tool", "-add_rpath", "@executable_path/../lib", str(helper)], check=True)
        bundled_names = {library.name for library in libraries}
        for library in libraries:
            subprocess.run(["install_name_tool", "-id", f"@rpath/{library.name}", str(library)], check=True)
        for binary in [helper, *libraries]:
            for dependency in binary_dependencies(binary, platform):
                name = Path(dependency).name
                if name in bundled_names and dependency != f"@rpath/{name}":
                    subprocess.run(["install_name_tool", "-change", dependency, f"@rpath/{name}", str(binary)], check=True)
        for binary in [*libraries, helper]:
            subprocess.run(["codesign", "--force", "--sign", "-", str(binary)], check=True)


def target_config(manifest: Path, triple: str) -> dict:
    targets = tomllib.loads(manifest.read_text(encoding="utf-8"))["target"]
    return next(target for target in targets if target["triple"] == triple)


def copy_cargo_licenses(destination: Path, target: str) -> None:
    metadata = json.loads(
        command_output(
            [
                "cargo",
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                target,
            ],
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
    for package_id in included:
        package = package_by_id[package_id]
        if package.get("source") is None:
            continue
        root = Path(package["manifest_path"]).parent
        license_files = [
            path
            for path in root.rglob("*")
            if path.is_file()
            and len(path.relative_to(root).parts) <= 4
            and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE"))
        ]
        package_destination = destination / "cargo" / f"{package['name']}-{package['version']}"
        for source in license_files:
            relative = source.relative_to(root)
            output = package_destination / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, output)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--library-root", type=Path, action="append", default=[])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sbom", type=Path, required=True)
    parser.add_argument("--targets", type=Path, default=Path("release/targets.toml"))
    parser.add_argument("--capabilities", type=Path, default=Path("release/required-capabilities.json"))
    arguments = parser.parse_args()

    config = target_config(arguments.targets, arguments.target)
    expected_capabilities = sorted(json.loads(arguments.capabilities.read_text(encoding="utf-8")))
    metadata = json.loads(
        command_output(
            [str(arguments.helper), "--print-build-metadata"], encoding="utf-8"
        )
    )
    if metadata["target"] != arguments.target:
        raise SystemExit(f"helper target mismatch: expected {arguments.target}, got {metadata['target']}")
    if sorted(metadata["capabilities"]) != expected_capabilities:
        raise SystemExit("helper does not contain the complete release capability contract")

    artifact = arguments.output / f"oxide-spice-helper-{arguments.target}"
    if artifact.exists():
        shutil.rmtree(artifact)
    binary_dir = artifact / "bin"
    library_dir = artifact / "lib"
    license_dir = artifact / "licenses"
    binary_dir.mkdir(parents=True)
    library_dir.mkdir()
    license_dir.mkdir()
    helper_name = "oxide-spice-helper.exe" if config["platform"] == "windows" else "oxide-spice-helper"
    packaged_helper = binary_dir / helper_name
    shutil.copy2(arguments.helper, packaged_helper)

    dependency_names, bundled = copy_dependency_closure(
        packaged_helper,
        arguments.library_root,
        binary_dir if config["platform"] == "windows" else library_dir,
        config["platform"],
        config["implicit_bundled_libraries"],
    )
    missing = [
        required
        for required in config["required_bundled_libraries"]
        if not any(required.lower() in library.name.lower() for library in bundled)
    ]
    if missing:
        raise SystemExit(f"artifact is missing required dynamic libraries: {', '.join(missing)}")
    forbidden = [
        dependency
        for dependency in dependency_names
        if any(
            pattern.lower() in dependency.lower()
            for pattern in config["forbidden_dynamic_libraries"]
        )
    ]
    if forbidden:
        raise SystemExit(f"artifact links forbidden dynamic libraries: {', '.join(forbidden)}")
    patch_runtime_paths(packaged_helper, bundled, config["platform"])

    metadata["minimumSystemVersion"] = config["minimum_system_version"]
    metadata["dynamicLibraries"] = dependency_names
    (artifact / "helper-metadata.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    shutil.copy2("LICENSE", artifact / "LICENSE")
    shutil.copy2("THIRD-PARTY-NOTICES.md", artifact / "THIRD-PARTY-NOTICES.md")
    shutil.copy2(arguments.sbom, artifact / "oxide-spice-helper.cdx.json")
    copy_cargo_licenses(license_dir, arguments.target)
    for root in arguments.library_root:
        source = root / "share" / "licenses"
        if source.is_dir():
            shutil.copytree(source, license_dir, dirs_exist_ok=True)
    if not any(license_dir.iterdir()):
        raise SystemExit("artifact has no bundled native license texts")
    print(artifact)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
