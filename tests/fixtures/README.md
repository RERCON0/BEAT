# Opus test fixtures

The files contain synthetic 440 Hz tones generated with FFmpeg/libopus,
not recordings of copyrighted songs. `tone.opus` includes BEAT's existing
icon as a Vorbis-comment picture to test embedded artwork.

- `tone.opus`: 3 seconds, stereo, Ogg, title/artist/album and picture.
- `tone.webm`: 3 seconds, mono, WebM, title/artist/album.
- `short.opus`: 0.25 seconds, stereo, Ogg with a single audio page.

Regenerate from the repository root:

```powershell
python -B tests/fixtures/generate.py C:\path\to\ffmpeg.exe
```

Tests consume committed bytes and do not need FFmpeg. Ogg tests check the
exact sample count, including pre-skip and end padding. WebM permits a 2 ms
duration tolerance because its duration is represented in whole milliseconds.
