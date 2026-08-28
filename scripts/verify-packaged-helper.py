#!/usr/bin/env python3
"""Launch a packaged helper and verify its metadata and Hello contract."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--capabilities", type=Path, default=Path("release/required-capabilities.json"))
    parser.add_argument("--forbid-loader-root", type=Path, action="append", default=[])
    arguments = parser.parse_args()

    environment = os.environ.copy()
    environment.pop("LD_LIBRARY_PATH", None)
    environment.pop("DYLD_LIBRARY_PATH", None)
    forbidden_roots = {root.resolve() for root in arguments.forbid_loader_root}
    environment["PATH"] = os.pathsep.join(
        entry
        for entry in environment.get("PATH", "").split(os.pathsep)
        if not any(root == Path(entry).resolve() or root in Path(entry).resolve().parents for root in forbidden_roots)
    )

    executable_name = "oxide-spice-helper.exe" if "windows" in arguments.target else "oxide-spice-helper"
    executable = arguments.directory / "bin" / executable_name
    expected_capabilities = sorted(json.loads(arguments.capabilities.read_text(encoding="utf-8")))
    artifact_metadata = json.loads((arguments.directory / "helper-metadata.json").read_text(encoding="utf-8"))
    embedded_metadata = json.loads(
        subprocess.check_output(
            [str(executable), "--print-build-metadata"], text=True, env=environment
        )
    )
    subprocess.run([str(executable), "--check-native-loads"], check=True, env=environment)
    for metadata in (artifact_metadata, embedded_metadata):
        if metadata["target"] != arguments.target:
            raise SystemExit("packaged helper target does not match its artifact name")
        if sorted(metadata["capabilities"]) != expected_capabilities:
            raise SystemExit("packaged helper does not report the complete capability contract")
    if not artifact_metadata["dynamicLibraries"]:
        raise SystemExit("artifact metadata has no audited dynamic-library list")
    if artifact_metadata["minimumSystemVersion"] == "unspecified":
        raise SystemExit("artifact metadata has no minimum system version")

    process = subprocess.Popen(
        [str(executable), "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    hello = {
        "type": "hello",
        "hello": {
            "protocolVersion": artifact_metadata["ipcProtocolVersion"],
            "requiredCapabilities": expected_capabilities,
        },
    }
    process.stdin.write(json.dumps(hello, separators=(",", ":")) + "\n")
    process.stdin.flush()
    acknowledgement = json.loads(process.stdout.readline())
    if acknowledgement.get("type") != "helloAck" or not acknowledgement["acknowledgement"]["compatible"]:
        raise SystemExit("packaged helper rejected the complete Hello contract")
    process.stdin.write('{"type":"close"}\n')
    process.stdin.flush()
    process.stdin.close()
    try:
        exit_code = process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        raise SystemExit("packaged helper did not exit after Close")
    if exit_code != 0:
        stderr = process.stderr.read() if process.stderr is not None else ""
        raise SystemExit(f"packaged helper exited with {exit_code}: {stderr}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
