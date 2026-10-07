"""Regenerate synthetic Opus fixtures; requires FFmpeg with libopus."""
import base64
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).resolve().parent
FFMPEG = sys.argv[1] if len(sys.argv) > 1 else "ffmpeg"


def blob(data):
    return struct.pack(">I", len(data)) + data


def generate():
    picture = (struct.pack(">I", 3) + blob(b"image/png") + blob(b"BEAT test cover")
               + struct.pack(">IIII", 256, 256, 32, 0) + blob((ROOT / "icons/beat-256.png").read_bytes()))
    with tempfile.TemporaryDirectory(prefix="beat-opus-fixtures-") as temporary:
        metadata = Path(temporary) / "tags.ffmetadata"
        metadata.write_text(
            ";FFMETADATA1\ntitle=Opus test\nartist=BEAT test artist\nalbum=BEAT test album\n"
            + "METADATA_BLOCK_PICTURE=" + base64.b64encode(picture).decode() + "\n", encoding="utf-8",
        )
        for name, seconds, channels in [("tone.opus", "3", "2"), ("tone.webm", "3", "1"), ("short.opus", "0.25", "2")]:
            command = [FFMPEG, "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
                       f"sine=frequency=440:sample_rate=48000:duration={seconds}"]
            if name == "tone.opus":
                command += ["-f", "ffmetadata", "-i", str(metadata), "-map_metadata", "1"]
            elif name == "tone.webm":
                command += ["-metadata", "title=Opus test", "-metadata", "artist=BEAT test artist",
                            "-metadata", "album=BEAT test album"]
            command += ["-c:a", "libopus", "-b:a", "32k" if name == "short.opus" else "64k",
                        "-ac", channels, str(FIXTURES / name)]
            subprocess.run(command, check=True)


if __name__ == "__main__":
    generate()
