"""Validate a native Windows GUI executable and record unsigned CI provenance."""
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def check(path):
    data = path.read_bytes()
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("Invalid DOS header")
    offset = struct.unpack_from("<I", data, 0x3C)[0]
    if offset + 112 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError("Invalid PE header")
    machine = struct.unpack_from("<H", data, offset + 4)[0]
    magic = struct.unpack_from("<H", data, offset + 24)[0]
    subsystem, flags = struct.unpack_from("<HH", data, offset + 24 + 68)
    if (machine, magic, subsystem) != (0x8664, 0x20B, 2):
        raise ValueError("Expected a Windows x64 GUI executable")
    if flags & 0x160 != 0x160:
        raise ValueError("ASLR, high-entropy ASLR or DEP missing")
    digest = hashlib.sha256(data).hexdigest()
    package = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    metadata = {
        "schema": 1, "name": package["name"], "version": package["version"],
        "source_commit": commit, "target": "x86_64-pc-windows-msvc", "signed": False,
        "file": path.name, "size": len(data), "sha256": digest,
    }
    (path.parent / "BUILD.json").write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    (path.parent / "SHA256SUMS.txt").write_text(f"{digest}  {path.name}\n", encoding="ascii")
    print(f"Verified unsigned {package['name']} Windows x64 GUI / {commit}")


if __name__ == "__main__":
    check(Path(sys.argv[1]))
