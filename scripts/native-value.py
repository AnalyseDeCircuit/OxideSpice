#!/usr/bin/env python3
"""Print one pinned native-source field for shell workflows."""

import argparse
from pathlib import Path

from toml_compat import tomllib


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path("native/dependencies.toml"))
    parser.add_argument("--name", required=True)
    parser.add_argument("--field", required=True)
    arguments = parser.parse_args()
    sources = tomllib.loads(arguments.manifest.read_text(encoding="utf-8"))["source"]
    selected = next(source for source in sources if source["name"] == arguments.name)
    print(selected[arguments.field])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
