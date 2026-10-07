# Dependency maintenance

The lockfile and [toolchain](../rust-toolchain.toml) are part of every release.
Update groups are separated into audio, interface and networking so failures
have a smaller review scope. Vulnerability scanning covers the complete lockfile,
including the retained audio family; no advisory is hidden by an allowlist.

## 0.2.1 decisions

| Family | Decision and checks |
|---|---|
| Rust | Pin 1.99.0; eframe 0.36 requires a newer compiler than the old 1.92 pin |
| Interface | eframe/egui 0.36.2; migrate panels, frames, fonts, image and lifecycle APIs; preserve both themes and transport glyphs |
| Networking | reqwest 0.13.5 with explicit `native-tls-no-alpn`, `system-proxy`, `query`, `json` and `blocking`; keep Windows Schannel and the existing redirect/time/size limits |
| Hashing | md-5 0.11; shared lowercase hex conversion preserves Subsonic tokens and existing profile/cache filenames |
| Audio | Rodio 0.22.2, Symphonia 0.5.5, libopus adapter 0.2.9 and opusic-sys 0.7.5 remain a compatible family; preserve the allocation guard |

The [reqwest manifest](https://github.com/seanmonstar/reqwest/blob/v0.13.5/Cargo.toml)
changes the default TLS backend. Explicit features preserve BEAT's existing
Windows TLS behavior. The new egui font stack removes the old `ttf-parser`
maintenance warning; RustSec reports no advisories for this lockfile at the
0.2.1 review date (8 October 2026).

## Symphonia 0.6 migration gate

This is a compatibility constraint, not an ignored security finding.
[Rodio 0.22.2](https://github.com/RustAudio/rodio/blob/v0.22.2/Cargo.toml)
uses Symphonia 0.5.5. The 0.3 libopus adapter targets Symphonia 0.6. Updating
only the direct dependencies mixes incompatible probe, decoder, metadata and
source types. Review the upstream [0.6 migration guide](https://github.com/pdeljanov/Symphonia/blob/main/docs/guides/migration/0p6.md)
before moving the whole audio stack.

Dependabot can propose patch updates to the current Symphonia/adapter versions;
minor/major jumps of these two crates are temporarily excluded. Revisit this
gate when Rodio supports 0.6, or when deliberately porting BEAT's decoding
pipeline. An applicable security advisory triggers an immediate review,
regardless of this gate.

Before accepting that migration:

1. Use a compatible Rodio, Symphonia and Opus adapter together; check for duplicate
   Symphonia families with `cargo tree -d`.
2. Port or replace the [core allocation guard](../vendor/symphonia-core/BEAT-PATCH.md).
   Keep rejection before allocating an oversized declared media field.
3. Verify MP3, FLAC, Vorbis, WAV, AAC/MP4 and Opus in Ogg/WebM, including malformed
   headers, streaming from a growing file, seeking, cancellation and decoder panics.
4. Verify sample-clock transitions, repeat/shuffle, restored sessions and output-device
   recovery; inspect both themes and run exact-commit CI/security checks.

The application controllers and worker events now live in `src/app/`; drawing
modules collect typed commands and dispatch them after drawing. Background
processing runs in eframe's `logic` hook, including while the window is hidden.
This keeps new API migrations and playback behavior out of individual buttons.
