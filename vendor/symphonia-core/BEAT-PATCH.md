# Symphonia core allocation guard

Source: `symphonia-core 0.5.5` from crates.io, under MPL-2.0.
The original revision is recorded in `.cargo_vcs_info.json`.
Original crates.io archive SHA-256:
`ea00cc4f79b7f6bb7ff87eddc065a1066f3a43fe1875979056672c9ef948c2af`.

BEAT changes only `ReadBytes::read_boxed_slice_exact` in `src/io/mod.rs`:

- Refuse declared binary fields over 32 MiB before allocating or reading.
- Use `try_reserve_exact` to report allocation failure as an I/O error.

This shared boundary protects metadata and packet readers, including Matroska
strings, codec-private data and blocks, from oversized length declarations.
Leading ID3/FLAC metadata has an additional aggregate guard in `src/media.rs`.
This is not a process sandbox or a universal bound on all decoder allocations.

Keep this patch when updating Symphonia. Compare the vendored source against
the original crate, update the recorded revision and run the malformed-header,
real-format and seeking tests. Remove the override only when the upstream
reader enforces an equivalent allocation limit. Cargo.lock and RustSec must
continue to include `symphonia-core` with its actual upstream version.
