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



def component_notices() -> str:
    """Collect notices from the exact Windows build graph, including bundled fonts."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", "x86_64-pc-windows-msvc"], cwd=ROOT, timeout=120))
    packages = {item["id"]: item for item in metadata["packages"]}
    nodes = {item["id"]: item for item in metadata["resolve"]["nodes"]}
    pending, reachable = [metadata["resolve"]["root"]], set()
    while pending:
        identity = pending.pop()
        if identity not in reachable:
            reachable.add(identity)
            pending.extend(item["pkg"] for item in nodes[identity]["deps"])
    notices = []
    extra = [ROOT / "fonts/OFL-notice.txt", ROOT / "fonts/OFL.txt",
             *sorted((ROOT / "third_party").glob("*-LICENSE.txt"))]
    notices.extend(f"{file.relative_to(ROOT)}\n\n{file.read_text(encoding='utf-8')}" for file in extra)
    generic = {"MPL-2.0": ROOT / "vendor/symphonia-core/LICENSE",
               "Apache-2.0": ROOT / "third_party/licenses/APACHE-2.0.txt",
               "MIT OR Apache-2.0": ROOT / "third_party/licenses/APACHE-2.0.txt",
               "(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0": ROOT / "third_party/licenses/APACHE-2.0.txt",
               "BSL-1.0": ROOT / "third_party/licenses/BSL-1.0.txt"}
    root = metadata["resolve"]["root"]
    for identity in sorted(reachable, key=lambda item: (packages[item]["name"], packages[item]["version"])):
        if identity == root:
            continue
        package = packages[identity]
        folder = Path(package["manifest_path"]).parent
        files = sorted(file for file in folder.rglob("*") if file.is_file()
                       and file.name.lower().startswith(("license", "licence", "copying", "copyright", "notice")))
        if package["name"] == "epaint_default_fonts":
            files.extend(sorted((folder / "fonts").glob("*.txt")))
        license_id = package.get("license")
        header = f"{package['name']} {package['version']} / {license_id}\n{package.get('repository') or ''}"
        body = []
        for file in sorted(set(files)):
            if not 0 < file.stat().st_size <= MAX_META:
                raise ValueError(f"Invalid notice size: {package['name']} / {file.name}")
            body.append(f"{file.relative_to(folder)}\n\n{file.read_text(encoding='utf-8')}")
        # Workspace crates sometimes omit their root license files from the
        # registry archive. Select Apache for dual licensing; retain package
        # provenance, authors and the complete applicable generic text.
        if not files or package["name"] == "epaint_default_fonts":
            fallback = generic.get(license_id)
            if fallback is None:
                raise ValueError(f"Missing license notice: {package['name']} / {license_id}")
            body.append(f"Authors: {', '.join(package['authors'])}\n\n{fallback.read_text(encoding='utf-8')}")
        notices.append(header + "\n\n" + "\n\n".join(body))
    return "\n\n".join(notices)


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
    (path.parent / "COMPONENT-NOTICES.txt").write_text(component_notices(), encoding="utf-8")
    (path.parent / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
    print(f"Verified unsigned {package['name']} Windows x64 GUI / {commit}")


if __name__ == "__main__":
    check(Path(sys.argv[1]))
