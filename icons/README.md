# BEAT icon

The selected design is concept 03: three flowing wave lobes, black on a white
rounded-square tile. It was finalized with the built-in image generation tool.

- `source/mark-flow.png`: transparent black mark.
- `source/mark-flow-prompt.txt`: the exact finalization prompt.
- `beat-source.png`: 1024px icon artwork used by the existing Rust exporter.
- `beat-256.png`: shared GUI header/window/taskbar icon.
- `beat.ico`: the executable icon in 16, 32, 48, 64, 128 and 256px sizes.

To recreate the tile from the mark, run `python icons/source/export_source.py`
with Pillow. Then export the runtime and executable resources with
`cargo run --example gen_icons --locked`, and rebuild with
`cargo build --release --locked`.
