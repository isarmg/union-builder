# Security Policy

Only the latest Builder release is supported. Report vulnerabilities privately through GitHub
Security Advisories.

Build configs and module source bundles are trusted project inputs, but Builder treats every path
and generated release as hostile structure: it rejects unknown config fields, unsafe relative paths,
symlinks, special files, missing required metadata, public process binds through the Platform
Manifest validator, unallowlisted module-owned route authentication, identity/version conflicts,
dependency incompatibility and unlisted output.
Package/release scans are bounded by file count, per-file bytes, total bytes, relative-path length and
directory depth; textual manifests and inventories also have an explicit read limit.

Source repositories must be credential-free GitHub HTTPS URLs and revisions must be canonical
lowercase 40-character Git object IDs. Selected Cargo package, binary target and version metadata
must agree with the release contract. Builder never accepts config-provided shell commands. Rust
builds use Cargo directly; frontend builds are limited to `npm ci` then `npm run build` in validated
relative directories.

`verify`, `stage` and `install` rerun module contract validation and exact SHA-256 inventory checks.
Install roots and release slots must be real directories. SHA-256 detects corruption but does not
authenticate the publisher, so obtain distributions from a trusted signed/TLS channel.

Modules are release-bundled private processes, not arbitrary online extensions. Builder does not
download plugins at runtime, manage secrets, expose module ports, start services, execute Migration,
or touch module data. Run builds without elevated privileges and grant install-root write access only
for staging/activation.

Module-owned authentication is denied by default. A profile may opt in exact route IDs through
`module_auth_routes` only for non-browser device-token or short-lived media-capability flows; the
Manifest set must match it exactly and the allowlist is persisted for offline release verification.
This is not a public-listener exception: process binds remain loopback-only, Backend services retain
platform visibility, all traffic passes through Union Gateway, and management routes use platform
authentication and RBAC.

File rollback does not restore databases or business data. Treat module backup, Migration safety and
data recovery as separate security and operations controls.
