#!/usr/bin/env python3
"""Fetch and verify pinned native source archives."""

from __future__ import annotations

import argparse
import hashlib
import shutil
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from toml_compat import tomllib

READ_CHUNK_SIZE = 1024 * 1024
DOWNLOAD_ATTEMPTS = 3
DOWNLOAD_TIMEOUT_SECONDS = 60
RETRY_DELAY_SECONDS = 2


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as archive:
        while chunk := archive.read(READ_CHUNK_SIZE):
            digest.update(chunk)
    return digest.hexdigest()


def archive_suffix(url: str) -> str:
    for suffix in (".tar.gz", ".tar.xz", ".tar.bz2", ".zip"):
        if url.endswith(suffix):
            return suffix
    raise ValueError(f"unsupported archive format: {url}")


def fetch(url: str, destination: Path) -> None:
    partial = destination.with_suffix(destination.suffix + ".partial")
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        partial.unlink(missing_ok=True)
        try:
            with urllib.request.urlopen(
                url, timeout=DOWNLOAD_TIMEOUT_SECONDS
            ) as response, partial.open("wb") as output:
                shutil.copyfileobj(response, output, READ_CHUNK_SIZE)
            partial.replace(destination)
            return
        except (TimeoutError, urllib.error.URLError):
            partial.unlink(missing_ok=True)
            if attempt == DOWNLOAD_ATTEMPTS:
                raise
            delay = RETRY_DELAY_SECONDS * attempt
            print(
                f"download attempt {attempt} failed for {url}; retrying in {delay} seconds",
                file=sys.stderr,
            )
            time.sleep(delay)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path("native/dependencies.toml"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--platform", choices=("linux", "macos", "windows"))
    arguments = parser.parse_args()

    manifest = tomllib.loads(arguments.manifest.read_text(encoding="utf-8"))
    arguments.output.mkdir(parents=True, exist_ok=True)
    for source in manifest["source"]:
        if arguments.platform and arguments.platform not in source["platforms"]:
            continue
        suffix = archive_suffix(source["url"])
        destination = arguments.output / f"{source['name']}-{source['version']}{suffix}"
        if not destination.exists():
            fetch(source["url"], destination)
        actual = sha256(destination)
        if actual != source["sha256"]:
            destination.unlink(missing_ok=True)
            print(
                f"SHA-256 mismatch for {source['name']}: expected {source['sha256']}, got {actual}",
                file=sys.stderr,
            )
            return 1
        print(f"verified {destination} {actual}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
