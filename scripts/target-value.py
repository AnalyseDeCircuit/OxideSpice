#!/usr/bin/env python3
"""Print one release-target field for shell workflows."""

import argparse
from pathlib import Path

from toml_compat import tomllib


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path("release/targets.toml"))
    parser.add_argument("--target", required=True)
    parser.add_argument("--field", required=True)
    arguments = parser.parse_args()
    targets = tomllib.loads(arguments.manifest.read_text(encoding="utf-8"))["target"]
    selected = next(target for target in targets if target["triple"] == arguments.target)
    print(selected[arguments.field])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
