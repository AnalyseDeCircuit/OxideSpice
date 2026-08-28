#!/usr/bin/env python3
"""Build the pinned native dependencies into one target-local prefix."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import tarfile
from pathlib import Path

from toml_compat import tomllib

# libusb 1.0.29 combines public enum flags that newer MSVC versions diagnose.
MSVC_LIBUSB_MIXED_ENUM_WARNING = "5287"


def run(command: list[str], *, cwd: Path | None = None, environment: dict[str, str] | None = None) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, cwd=cwd, env=environment, check=True)


def archive_name(source: dict) -> str:
    suffix = next(suffix for suffix in (".tar.gz", ".tar.xz", ".tar.bz2", ".zip") if source["url"].endswith(suffix))
    return f"{source['name']}-{source['version']}{suffix}"


def extract_archive(archive: Path, destination: Path) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as source:
        root = destination.resolve()
        for member in source.getmembers():
            target = (destination / member.name).resolve()
            if root != target and root not in target.parents:
                raise ValueError(f"archive member escapes destination: {member.name}")
            if member.isdev():
                raise ValueError(f"archive contains a device node: {member.name}")
            if member.issym():
                link_target = (target.parent / member.linkname).resolve()
                if root != link_target and root not in link_target.parents:
                    raise ValueError(f"archive link escapes destination: {member.name}")
            if member.islnk():
                link_target = (destination / member.linkname).resolve()
                if root != link_target and root not in link_target.parents:
                    raise ValueError(f"archive link escapes destination: {member.name}")
        source.extractall(destination)
    roots = [entry for entry in destination.iterdir() if entry.is_dir()]
    if len(roots) != 1:
        raise ValueError(f"expected one source root in {archive}")
    return roots[0]


def meson_build(source: Path, build: Path, prefix: Path, library_kind: str, options: list[str], environment: dict[str, str]) -> None:
    run(
        [
            "meson",
            "setup",
            str(build),
            str(source),
            "--buildtype=release",
            f"--prefix={prefix}",
            "--libdir=lib",
            f"--default-library={library_kind}",
            *options,
        ],
        environment=environment,
    )
    run(["meson", "compile", "-C", str(build)], environment=environment)
    run(["meson", "install", "-C", str(build)], environment=environment)


def copy_license(source: Path, prefix: Path, package: str, candidates: tuple[str, ...]) -> None:
    destination = prefix / "share" / "licenses" / package
    destination.mkdir(parents=True, exist_ok=True)
    for candidate in candidates:
        license_path = source / candidate
        if license_path.is_file():
            shutil.copy2(license_path, destination / license_path.name)
            return
    raise FileNotFoundError(f"license file not found for {package}")


def git_bash() -> str:
    """Resolve Git for Windows Bash without selecting the WSL launcher."""
    git_executable = shutil.which("git")
    if git_executable is None:
        raise FileNotFoundError("git.exe is required to locate Git Bash")
    candidate = Path(git_executable).parent.parent / "bin" / "bash.exe"
    if not candidate.is_file():
        raise FileNotFoundError(f"Git Bash was not found at {candidate}")
    return str(candidate)


def build_libusb(
    source: Path,
    version: str,
    build: Path,
    prefix: Path,
    platform: str,
    architecture: str,
    environment: dict[str, str],
) -> None:
    if platform == "windows":
        visual_studio_platform = "x64" if architecture == "x86_64" else "ARM64"
        libusb_environment = environment.copy()
        existing_compiler_options = libusb_environment.get("CL", "").strip()
        libusb_environment["CL"] = (
            f"{existing_compiler_options} /wd{MSVC_LIBUSB_MIXED_ENUM_WARNING}".strip()
        )
        run(
            [
                "msbuild",
                str(source / "msvc" / "libusb_dll.vcxproj"),
                "/m",
                "/p:Configuration=Release-MT",
                f"/p:Platform={visual_studio_platform}",
            ],
            environment=libusb_environment,
        )
        dll = next((source / "build").rglob("libusb-1.0.dll"))
        import_library = next((source / "build").rglob("libusb-1.0.lib"))
        (prefix / "bin").mkdir(parents=True, exist_ok=True)
        (prefix / "lib").mkdir(parents=True, exist_ok=True)
        (prefix / "include" / "libusb-1.0").mkdir(parents=True, exist_ok=True)
        shutil.copy2(dll, prefix / "bin" / dll.name)
        shutil.copy2(import_library, prefix / "lib" / import_library.name)
        shutil.copy2(source / "libusb" / "libusb.h", prefix / "include" / "libusb-1.0" / "libusb.h")
        pkgconfig = prefix / "lib" / "pkgconfig"
        pkgconfig.mkdir(parents=True, exist_ok=True)
        normalized_prefix = prefix.as_posix()
        (pkgconfig / "libusb-1.0.pc").write_text(
            f"prefix={normalized_prefix}\nlibdir=${{prefix}}/lib\nincludedir=${{prefix}}/include\n\n"
            f"Name: libusb-1.0\nDescription: USB access library\nVersion: {version}\n"
            "Libs: -L${libdir} -llibusb-1.0\nCflags: -I${includedir}/libusb-1.0\n",
            encoding="utf-8",
        )
        return
    build.mkdir(parents=True, exist_ok=True)
    configure_options = [
        str(source / "configure"),
        f"--prefix={prefix}",
        "--enable-shared",
        "--disable-static",
    ]
    if platform == "linux":
        configure_options.append("--enable-udev")
    run(
        configure_options,
        cwd=build,
        environment=environment,
    )
    run(["make", f"-j{os.cpu_count() or 2}"], cwd=build, environment=environment)
    run(["make", "install"], cwd=build, environment=environment)


def build_libvpx(source: Path, build: Path, prefix: Path, platform: str, architecture: str, environment: dict[str, str]) -> None:
    build.mkdir(parents=True, exist_ok=True)
    libvpx_environment = environment.copy()
    command = [
        str(source / "configure"),
        f"--prefix={prefix}",
        "--enable-static",
        "--disable-shared",
        "--disable-examples",
        "--disable-tools",
        "--disable-docs",
        "--disable-unit-tests",
    ]
    target_architecture = "x86_64" if architecture == "x86_64" else "arm64"
    if platform == "linux":
        command.append(f"--target={target_architecture}-linux-gcc")
    elif platform == "macos":
        command.append(f"--target={target_architecture}-darwin24-gcc")
    else:
        # Relative source paths remain valid in both Git Bash and native Windows make.
        git_bash_path = Path(git_bash())
        libvpx_environment["PATH"] = os.pathsep.join(
            (str(git_bash_path.parent), libvpx_environment["PATH"])
        )
        command[0] = Path(os.path.relpath(source / "configure", build)).as_posix()
        command[1] = f"--prefix={prefix.resolve().as_posix()}"
        command = [
            str(git_bash_path),
            *command,
            f"--target={target_architecture}-win64-vs17",
            "--enable-static-msvcrt",
        ]
    run(command, cwd=build, environment=libvpx_environment)
    make_jobs = os.cpu_count() or 2
    if platform == "windows":
        # Git Bash keeps upstream shell recipes away from the Windows WSL association.
        run(
            [str(git_bash_path), "-c", f"make SHELL=/bin/bash -j{make_jobs}"],
            cwd=build,
            environment=libvpx_environment,
        )
        run(
            [str(git_bash_path), "-c", "make SHELL=/bin/bash install"],
            cwd=build,
            environment=libvpx_environment,
        )
    else:
        run(["make", f"-j{make_jobs}"], cwd=build, environment=libvpx_environment)
        run(["make", "install"], cwd=build, environment=libvpx_environment)
    if platform == "windows":
        installed_library = prefix / "lib" / "vpx.lib"
        if installed_library.is_file():
            shutil.copy2(installed_library, prefix / "lib" / "libvpx.lib")


def build_kerberos(source: Path, build: Path, prefix: Path, environment: dict[str, str]) -> None:
    build.mkdir(parents=True, exist_ok=True)
    run(
        [
            str(source / "src" / "configure"),
            f"--prefix={prefix}",
            "--enable-shared",
            "--disable-static",
            "--without-system-verto",
        ],
        cwd=build,
        environment=environment,
    )
    run(["make", f"-j{os.cpu_count() or 2}"], cwd=build, environment=environment)
    run(["make", "install"], cwd=build, environment=environment)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path("native/dependencies.toml"))
    parser.add_argument("--archives", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--platform", choices=("linux", "macos", "windows"), required=True)
    parser.add_argument("--architecture", choices=("x86_64", "aarch64"), required=True)
    arguments = parser.parse_args()

    manifest = tomllib.loads(arguments.manifest.read_text(encoding="utf-8"))
    selected = {
        source["name"]: source
        for source in manifest["source"]
        if arguments.platform in source["platforms"]
    }
    source_roots = {}
    for name, source in selected.items():
        source_roots[name] = extract_archive(
            arguments.archives / archive_name(source), arguments.work / "source" / name
        )

    arguments.prefix.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    pkgconfig_paths = [arguments.prefix / "lib" / "pkgconfig", arguments.prefix / "share" / "pkgconfig"]
    environment["PKG_CONFIG_PATH"] = os.pathsep.join(str(path) for path in pkgconfig_paths)
    environment["PKG_CONFIG_LIBDIR"] = environment["PKG_CONFIG_PATH"]
    meson_native_options = ["-Db_vscrt=mt"] if arguments.platform == "windows" else []
    build_libusb(
        source_roots["libusb"],
        selected["libusb"]["version"],
        arguments.work / "build" / "libusb",
        arguments.prefix,
        arguments.platform,
        arguments.architecture,
        environment,
    )
    copy_license(source_roots["libusb"], arguments.prefix, "libusb", ("COPYING",))
    pixman_options = [
        "-Dtests=disabled",
        "-Ddemos=disabled",
        "-Dgtk=disabled",
        "-Dlibpng=disabled",
        *meson_native_options,
    ]
    # Meson's MSVC backend cannot compile Pixman's GNU-style AArch64 assembly sources
    # and its intrinsic probes can incorrectly enable x86 implementations for ARM64.
    if arguments.platform == "windows" and arguments.architecture == "aarch64":
        pixman_options.extend(
            (
                "-Da64-neon=disabled",
                "-Dmmx=disabled",
                "-Dsse2=disabled",
                "-Dssse3=disabled",
            )
        )
    meson_build(
        source_roots["pixman"],
        arguments.work / "build" / "pixman",
        arguments.prefix,
        "static",
        pixman_options,
        environment,
    )
    copy_license(source_roots["pixman"], arguments.prefix, "pixman", ("COPYING",))
    build_libvpx(
        source_roots["libvpx"],
        arguments.work / "build" / "libvpx",
        arguments.prefix,
        arguments.platform,
        arguments.architecture,
        environment,
    )
    copy_license(source_roots["libvpx"], arguments.prefix, "libvpx", ("LICENSE",))
    meson_build(
        source_roots["usbredir"],
        arguments.work / "build" / "usbredir",
        arguments.prefix,
        "shared",
        ["-Dtools=disabled", "-Dtests=disabled", *meson_native_options],
        environment,
    )
    copy_license(source_roots["usbredir"], arguments.prefix, "usbredir", ("COPYING.LIB", "COPYING"))
    if arguments.platform == "linux":
        meson_build(
            source_roots["pcsc-lite"],
            arguments.work / "build" / "pcsc-lite",
            arguments.prefix,
            "shared",
            [
                "-Dlibsystemd=false",
                "-Dlibudev=false",
                "-Dlibusb=false",
                "-Dpolkit=false",
                "-Dusb=false",
                "-Dserial=false",
            ],
            environment,
        )
        copy_license(
            source_roots["pcsc-lite"], arguments.prefix, "pcsc-lite", ("COPYING",)
        )
        build_kerberos(
            source_roots["mit-krb5"],
            arguments.work / "build" / "mit-krb5",
            arguments.prefix,
            environment,
        )
        copy_license(source_roots["mit-krb5"], arguments.prefix, "mit-krb5", ("NOTICE", "LICENSE"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
