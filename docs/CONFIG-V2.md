# Builder config schema v2

Builder configuration is strict TOML (`deny_unknown_fields`). `schema_version = 2` is mandatory;
v1 fields such as `base_features`, `union_feature`, module runtime addresses and frontend install paths
are rejected.

```toml
schema_version = 2
require_clean_sources = true

[distribution]
name = "unionc"
version = "0.5.0"
source = "../union-rust"
repository = "https://github.com/isarmg/union-rust.git" # optional if source exists
revision = "<full-40-character-git-id>"
package = "unionc"
binary = "unionc"
output = "dist/full"

[distribution.frontend]
directory = "web"
output = "dist"

[[module]]
id = "photo-backup"
source = "../photo-backup"
repository = "https://github.com/isarmg/photo-backup.git"
revision = "<full-40-character-git-id>"
package = "photo-backup-server"
binary = "photo-backup-server"
bundle = "."
module_auth_routes = ["upload-part", "mobile-api"]

# Optional npm frontend build. Without this table, bundle/frontend is already the module output.
[module.frontend]
directory = "module-web"
output = "dist"
```

Field ownership:

| Field group | Purpose | Not allowed to decide |
|---|---|---|
| `distribution` | Pin and build Core/Web Shell once | Business feature set, runtime module state |
| `[[module]]` | Select a complete module source package and exact module-auth route exceptions | Listen/Gateway/health/permission contract |
| module `manifest.json` | Runtime contract and all module contributions | Source revision or runtime enabled state |
| `union-release.json` | Exact included packages and dependency activation order | Database state or enable/disable state |

Every revision is a non-zero canonical lowercase 40-character Git object ID. If a custom composition
intentionally references modules from the official Union repository, schema validation requires those
entries to use the same revision as Core/Web Shell and caller materialization updates that set atomically.
The official profiles do not use that layout: Sunshine and Host Monitoring have independent repositories
and revisions.

The module `binary` names the Cargo artifact. Its install base name is taken from the validated source
Manifest `execution.executable`; this deliberately supports renaming, for example
`union-host-monitoring-worker` to `modules/host-monitoring/backend/host-monitoring`. Source Manifests
use this platform-neutral name, while Builder appends the target-specific `.exe` suffix to both the
packaged path and final Manifest on Windows. Process arguments such as Host Monitoring's `serve` remain
in Manifest `execution.args` and are consumed by Runtime, not duplicated in Builder config.

`module_auth_routes` defaults to an empty list. The listed values are exact Manifest backend route
IDs, not URL patterns: their set must equal the routes whose `auth` is `module`, and duplicate,
missing or extra IDs fail validation. This narrow exception exists only for non-browser device-token,
one-time pairing and short-lived media-capability flows that cannot use an interactive Union session. Management
routes continue to use platform authentication and RBAC. An exception never changes exposure:
workers still bind only to loopback, their Backend service keeps `platform` visibility, and every
request must enter through Union Gateway. Builder records the allowlist in `union-release.json` so
`verify` can enforce the same equality without the source profile.

## Union caller materialization

Reusable workflow callers cannot put their own final commit into a Builder profile before that commit exists.
`union-builder materialize` resolves this bootstrap edge without accepting a branch, tag or mutable source:

```bash
union-builder materialize \
  --config profiles/full.toml \
  --caller-repository https://github.com/isarmg/union-rust.git \
  --caller-source /workspace/union-rust \
  --caller-revision 0123456789abcdef0123456789abcdef01234567 \
  --output profiles/full.materialized.toml
```

The repository is an exact identity, not a URL prefix: only the official credential-free Union GitHub URL is
accepted. The distribution must match it. Any custom-profile module using that exact same repository is also
redirected to the verified worktree and revision, but current official profiles contain no such module, so
their materialization count is one. Sunshine, Host Monitoring, Dufs, Photo Backup and Sentinel stay pinned to
their independent repositories, and no module is added or removed.

The source must be a real directory at the Git worktree root; its `HEAD` must equal the supplied full revision.
Builder serializes and re-parses strict schema v2, writes a temporary file in the output directory, fsyncs it
and publishes with no-clobber semantics. Keep the output next to the selected config so unchanged relative
source and output paths retain their meaning.

The reusable workflow separately requires external callers to pass an immutable `builder-revision`. It is used
as the Builder checkout ref and must be the same full commit ID used in the caller's `uses@<sha>` reference.
The workflow compares that input with both GitHub's called-job `workflow_ref` and resolved `workflow_sha`
before checkout, so a branch, tag, different commit or merely SHA-shaped unrelated input is rejected. Builder
self-runs and manual dispatches use their own `github.sha`.
