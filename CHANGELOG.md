# Changelog

## 1.0.0 - 2026-08-27

- Make the CLI the single official path for selecting, building and packaging Union modules.
- Build locked npm frontends without allowing manifest-provided shell commands and include every
  generated asset in the release inventory.
- Add official `minimal`, `storage`, `monitoring` and `full` compile-time profiles.
- Let the reusable workflow select exactly one caller config or official profile, default manual
  runs to `full`, and pin Node.js 26.7.0 for Union/Sentinel frontends.
- Add strict release verification, immutable staging, atomic Unix activation and offline rollback.
- Reject unlisted files, symlinks, unsafe paths and overlapping frontend install destinations.
- Preserve executable modes across GitHub artifact transport, require version-matched release tags,
  and include each selected source's Apache license/NOTICE files in the checksummed distribution.
- Resolve all official profile revisions and assemble/verify the full Union distribution before
  publishing Builder, while keeping validation artifacts out of the Builder CLI Release.
- Derive executable suffixes from an explicit cross-compilation target instead of the host OS.

## 0.2.0 - 2026-08-27

- Add a pinned full-transition Union composition profile.
- Add a reusable GitHub Actions workflow that invokes the CLI and uploads one Union artifact.
- Support an exact core-only build with no optional process modules.
- Always disable Union default features so the manifest is the complete source of module selection.

## 0.1.1 - 2026-08-27

- Stage Unix release binaries with portable commands supported by both GNU/Linux and macOS.

## 0.1.0 - 2026-08-27

- Add strict TOML validation and exact Git revision checks.
- Add compile-time Union feature planning and process-module Cargo builds.
- Add one-distribution assembly, release manifest and SHA-256 checksums.
- Add optional exact-revision source fetching for CI and clean machines.
