#!/usr/bin/env python3
"""Create a deterministic helper archive and its SHA-256 file."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import shutil
import tarfile
import zipfile
from pathlib import Path

ARCHIVE_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
HASH_CHUNK_SIZE = 1024 * 1024


def entries(root: Path) -> list[Path]:
    return [root, *sorted(root.rglob("*"), key=lambda path: path.as_posix())]


def write_tar(root: Path, output: Path) -> None:
    with output.open("wb") as destination, gzip.GzipFile(
        filename="", fileobj=destination, mode="wb", mtime=0
    ) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as archive:
            for path in entries(root):
                info = archive.gettarinfo(path, arcname=path.relative_to(root.parent))
                info.mtime = 0
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                if path.is_file():
                    with path.open("rb") as source:
                        archive.addfile(info, source)
                else:
                    archive.addfile(info)


def write_zip(root: Path, output: Path) -> None:
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in entries(root):
            name = path.relative_to(root.parent).as_posix()
            if path.is_dir():
                name += "/"
            info = zipfile.ZipInfo(name, ARCHIVE_TIMESTAMP)
            info.external_attr = (path.stat().st_mode & 0xFFFF) << 16
            if path.is_file():
                with path.open("rb") as source, archive.open(info, "w") as destination:
                    shutil.copyfileobj(source, destination, HASH_CHUNK_SIZE)
            else:
                archive.writestr(info, b"")


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(HASH_CHUNK_SIZE):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--format", choices=("tar.gz", "zip"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    if arguments.format == "tar.gz":
        write_tar(arguments.directory, arguments.output)
    else:
        write_zip(arguments.directory, arguments.output)
    digest = file_sha256(arguments.output)
    checksum = arguments.output.with_suffix(arguments.output.suffix + ".sha256")
    checksum.write_text(f"{digest}  {arguments.output.name}\n", encoding="utf-8")
    print(checksum)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
