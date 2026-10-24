# Dependency review, 2026-10-25

- No open GitHub Dependabot security alerts at review time.
- RUSTSEC-2026-0386: upgrade Tokio to 1.53.3. The affected AsyncFd API is Unix-only; BEAT runs on Windows.
- Replace yanked clipboard-win 5.4.1 with 5.4.3.
- Dependabot PRs #3/#4 propose rodio 0.23 and eframe 0.37. These are routine minor-version migrations of 0.x APIs, not reported vulnerabilities. They require audio/UI compatibility work and are kept separate from this patch release.

Verdict: Dependabot version PRs are useful maintenance leads, but an empty alert list does not replace an up-to-date RustSec audit. Findings above are checked against the actual dependency graph and application use. Informational maintenance notices are tracked with their migration trigger.
