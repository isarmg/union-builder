# Changelog

## 2.0.0 - 2026-08-28

- Replace compile-time Union business feature selection with release-bundled Plugin Manifest v1
  packages; Core and Web Shell are each built once.
- Define profiles as immutable release inclusion sets, explicitly separate from Core runtime
  enable/disable state.
- Validate manifests through `sarmg-platform-core`, then validate platform compatibility,
  dependency versions/order, Cargo package/binary/version identity, required package files and
  package identity across metadata files.
- Assemble canonical `modules/<id>` packages containing a private process backend, independent
  frontend, permission definitions, configuration schema, version metadata and migrations.
- Generate final `source_revision` metadata from the locked source revision to avoid self-referential
  source commits.
- Reject symlinks, path escape, missing or non-executable workers, mismatched permissions/config/
  version files, non-canonical backend paths, extra release files and SHA-256 mismatches.
- Assemble into a temporary sibling directory and publish the output only after complete package and
  checksum verification, so failed builds do not expose a partial distribution.
- Preserve immutable staging and atomic Unix file activation/rollback while documenting that file
  rollback never rolls back module databases or stored data.
- Keep GitHub Actions as a thin CLI caller and keep modules out of independent public releases.
- Add an auditable `materialize` command and explicit reusable-workflow opt-in that bind Union-owned
  profile entries to the verified `isarmg/union-rust` caller SHA without creating a final-SHA cycle;
  official profiles now materialize only the Core/Web distribution because Sunshine and Host
  Monitoring are pinned from their own repositories.
- Pin Sunshine and Host Monitoring as independently versioned module sources while retaining their
  existing Cargo artifacts and bundle layouts.
- Require external reusable-workflow callers to pin the Builder checkout with an explicit full
  `builder-revision`; Builder self-runs use `github.sha` and never resolve source from a movable tag.
- Replace the blanket platform-auth route policy with a deny-by-default, exact-ID
  `module_auth_routes` allowlist for non-browser device-token and short-lived media flows, persisted
  into the release manifest and rechecked by offline verification.
- Restrict Union server distributions to native Linux amd64/arm64 builds, record mandatory
  `platform` and `architecture` identity in schema-v2 release manifests, and reject every other
  server target during build and verification.
- Add target-aware reusable-workflow inputs/outputs and collision-free architecture-qualified
  artifact/archive names; validate the full profile on fixed native x64 and arm64 Ubuntu runners.
- Keep the Union Builder helper CLI's Linux, Windows and macOS releases separate from the narrower
  Union Server support matrix.
- Keep cross-machine staging available while refusing install/rollback when a release target does
  not match the current Linux host; document the Ubuntu 24.04 GNU/glibc compatibility baseline.

## 1.0.0 - 2026-08-27

- Initial compile-time feature-selection release. This architecture is superseded by 2.0.0.
