"""Wrap the finalized BEAT cutout on the shared flat Windows app tile."""
from pathlib import Path
from PIL import Image, ImageDraw

root = Path(__file__).resolve().parents[1]
with Image.open(root / "source" / "mark-flow.png") as source:
    mark = source.convert("RGBA")
bounds = mark.getchannel("A").getbbox()
if bounds is None:
    raise ValueError("Empty BEAT mark")
mark = mark.crop(bounds)
mark.thumbnail((720, 720), Image.Resampling.LANCZOS)
tile = Image.new("RGBA", (1024, 1024))
ImageDraw.Draw(tile).rounded_rectangle((24, 24, 999, 999), radius=166, fill="white")
tile.alpha_composite(mark, ((1024 - mark.width) // 2, (1024 - mark.height) // 2))
tile.save(root / "beat-source.png", optimize=True)
print("Prepared BEAT / 03 source: black sound-flow mark on white rounded tile")
