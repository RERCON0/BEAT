"""Validate a native Windows GUI executable and record unsigned CI provenance."""
import hashlib
import json
import re
from pathlib import Path
import struct
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
MAX_META = 1024 * 1024


def pe_info(data: bytes, subsystem: int) -> dict:
    """Reject wrong architecture, missing exploit mitigations and dynamic CRT."""
    def unpack(fmt: str, offset: int) -> tuple:
        size = struct.calcsize(fmt)
        if offset < 0 or offset + size > len(data):
            raise ValueError("Truncated PE image")
        return struct.unpack_from(fmt, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("Missing DOS header")
    pe, = unpack("<I", 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Missing PE header")
    machine, sections = unpack("<HH", pe + 4)
    opt_size, = unpack("<H", pe + 20)
    opt = pe + 24
    magic, = unpack("<H", opt)
    actual_subsystem, flags = unpack("<HH", opt + 68)
    if machine != 0x8664 or magic != 0x20B or actual_subsystem != subsystem:
        raise ValueError("Expected Windows x64 PE with the correct CLI/GUI subsystem")
    if flags & 0x160 != 0x160:
        raise ValueError("PE requires ASLR, high-entropy ASLR and DEP")
    if opt_size < 128 or sections == 0 or sections > 96:
        raise ValueError("Invalid PE sections/optional header")
    mappings = []
    for number in range(sections):
        offset = opt + opt_size + 40 * number
        _, virtual_size, rva, raw_size, raw = unpack("<8sIIII", offset)
        mappings.append((rva, max(virtual_size, raw_size), raw, raw_size))

    def position(rva: int, size: int = 1) -> int:
        for start, length, raw, raw_size in mappings:
            delta = rva - start
            if 0 <= delta < length and delta + size <= raw_size:
                at = raw + delta
                if at + size <= len(data):
                    return at
        raise ValueError("PE RVA outside file-backed sections")

    imports_rva, imports_size = unpack("<II", opt + 120)
    imports = []
    if not imports_rva or imports_size < 20 or imports_size > MAX_META:
        raise ValueError("Missing/invalid PE imports")
    for index in range(min(imports_size // 20, 4096)):
        descriptor = unpack("<IIIII", position(imports_rva + index * 20, 20))
        if not any(descriptor):
            break
        at = position(descriptor[3])
        name = data[at:at + 256].split(b"\0", 1)[0].decode("ascii").lower()
        if not re.fullmatch(r"[a-z0-9_.-]+\.dll", name):
            raise ValueError("Invalid PE import name")
        if name.startswith(("vcruntime", "msvcp", "api-ms-win-crt")) or name == "ucrtbase.dll":
            raise ValueError(f"Dynamic Visual C++ runtime dependency: {name}")
        imports.append(name)
    else:
        raise ValueError("Unterminated PE import table")
    return {"machine": "amd64", "subsystem": subsystem, "dll_characteristics": flags,
            "imports": sorted(set(imports))}



def check(path):
    data = path.read_bytes()
    pe = pe_info(data, 2)
    digest = hashlib.sha256(data).hexdigest()
    package = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    metadata = {
        "schema": 1, "name": package["name"], "version": package["version"],
        "source_commit": commit, "target": "x86_64-pc-windows-msvc", "signed": False,
        "file": path.name, "size": len(data), "sha256": digest, "pe": pe,
    }
    (path.parent / "BUILD.json").write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    (path.parent / "SHA256SUMS.txt").write_text(f"{digest}  {path.name}\n", encoding="ascii")
    notices = [ROOT / "fonts/OFL-notice.txt", ROOT / "fonts/OFL.txt",
               *sorted((ROOT / "third_party").glob("*-LICENSE.txt")),
               ROOT / "vendor/symphonia-core/LICENSE"]
    (path.parent / "COMPONENT-NOTICES.txt").write_text(
        "\n\n".join(f"{file.relative_to(ROOT)}\n\n{file.read_text(encoding='utf-8')}" for file in notices),
        encoding="utf-8",
    )
    print(f"Verified unsigned {package['name']} Windows x64 GUI / {commit}")


if __name__ == "__main__":
    check(Path(sys.argv[1]))
