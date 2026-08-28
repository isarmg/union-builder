use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use sarmg_platform_core::{
    Execution, MigrationEngine, PLATFORM_API_VERSION, PLUGIN_API_VERSION, PermissionDefinition,
    PlatformVersions, PluginCatalog, PluginManifest, ReleaseChannel, RouteAuth, ServiceVisibility,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RELEASE_SCHEMA_VERSION: u32 = 2;
const OFFICIAL_UNION_REPOSITORY: &str = "https://github.com/isarmg/union-rust.git";
const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TREE_FILES: u64 = 100_000;
const MAX_SINGLE_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TREE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 1024;
const MAX_TREE_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy)]
struct TreeLimits {
    files: u64,
    single_file_bytes: u64,
    total_bytes: u64,
    path_bytes: usize,
    depth: usize,
}

const RELEASE_TREE_LIMITS: TreeLimits = TreeLimits {
    files: MAX_TREE_FILES,
    single_file_bytes: MAX_SINGLE_FILE_BYTES,
    total_bytes: MAX_TREE_BYTES,
    path_bytes: MAX_RELATIVE_PATH_BYTES,
    depth: MAX_TREE_DEPTH,
};

#[derive(Debug, Default)]
struct TreeStats {
    files: u64,
    bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub require_clean_sources: bool,
    pub distribution: Distribution,
    #[serde(default, rename = "module")]
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub revision: String,
    pub package: String,
    pub binary: String,
    pub frontend: Frontend,
    #[serde(default = "default_output")]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub id: String,
    pub source: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub revision: String,
    pub package: String,
    pub binary: String,
    #[serde(default = "default_bundle")]
    pub bundle: PathBuf,
    /// Exact route-id allowlist for exceptional Manifest routes whose authentication is owned by
    /// the module. All other routes must use Union platform authentication and RBAC.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_auth_routes: Vec<String>,
    /// Optional npm build. Otherwise the bundle's static frontend is copied independently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend: Option<Frontend>,
}

/// Builder always executes exactly `npm ci` followed by `npm run build`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frontend {
    pub directory: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CheckedConfig {
    pub config: BuildConfig,
    pub config_dir: PathBuf,
    pub distribution_source: SourceIdentity,
    pub module_sources: Vec<SourceIdentity>,
    module_packages: Vec<CheckedModulePackage>,
    activation_order: Vec<String>,
}

#[derive(Debug, Clone)]
struct CheckedModulePackage {
    bundle_root: PathBuf,
    manifest: PluginManifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceIdentity {
    pub path: PathBuf,
    pub revision: String,
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Cargo artifact profile. Release composition is selected only by the config file.
    pub cargo_profile: String,
    /// Linux server distribution target. If omitted, Builder derives it from a supported Linux
    /// host; release workflows should always pass it explicitly.
    pub server_target: Option<ServerTarget>,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ServerTarget {
    #[value(name = "linux-amd64")]
    LinuxAmd64,
    #[value(name = "linux-arm64")]
    LinuxArm64,
}

impl ServerTarget {
    pub const fn name(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }

    pub const fn platform(self) -> &'static str {
        "linux"
    }

    pub const fn architecture(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "amd64",
            Self::LinuxArm64 => "arm64",
        }
    }

    pub const fn rust_target(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
        }
    }

    fn from_release_fields(platform: &str, architecture: &str) -> Result<Self> {
        match (platform, architecture) {
            ("linux", "amd64") => Ok(Self::LinuxAmd64),
            ("linux", "arm64") => Ok(Self::LinuxArm64),
            _ => anyhow::bail!(
                "unsupported server distribution target {platform}/{architecture}; supported targets are linux/amd64 and linux/arm64"
            ),
        }
    }
}

impl std::fmt::Display for ServerTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Resolve an omitted local target from the host. Union server distributions are Linux-only;
/// Windows and Apple Builder CLIs must pass an explicit Linux cross-build target.
pub fn resolve_server_target(target: Option<ServerTarget>) -> Result<ServerTarget> {
    if let Some(target) = target {
        return Ok(target);
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok(ServerTarget::LinuxAmd64),
        ("linux", "aarch64") => Ok(ServerTarget::LinuxArm64),
        (platform, architecture) => anyhow::bail!(
            "cannot infer a supported Union server target from {platform}/{architecture}; pass --server-target linux-amd64 or --server-target linux-arm64"
        ),
    }
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub output: PathBuf,
    pub manifest: PathBuf,
    pub checksums: PathBuf,
    pub server_target: ServerTarget,
}

#[derive(Debug, Clone)]
pub struct MaterializeResult {
    pub output: PathBuf,
    pub matched_entries: usize,
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub release_id: String,
    pub release: PathBuf,
    pub previous_release_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationResult {
    pub release: PathBuf,
    pub release_id: String,
    pub files: usize,
    pub server_target: ServerTarget,
}

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
}

/// Materialize a schema-v2 composition for a Union workflow caller without changing its release
/// inclusion set. Only the distribution and modules whose repository exactly matches the official
/// Union repository are redirected to the verified caller checkout.
pub fn materialize_caller_checkout(
    config_path: &Path,
    caller_repository: &str,
    caller_source: &Path,
    caller_revision: &str,
    output_path: &Path,
) -> Result<MaterializeResult> {
    ensure!(
        caller_repository == OFFICIAL_UNION_REPOSITORY,
        "caller repository must be the exact official Union GitHub URL: {OFFICIAL_UNION_REPOSITORY}"
    );
    validate_repository(caller_repository)?;
    validate_revision(caller_revision)?;

    ensure_normal_directory("caller source", caller_source)?;
    let caller_source = caller_source
        .canonicalize()
        .with_context(|| format!("resolve caller source {}", caller_source.display()))?;
    let git_root = capture(
        Command::new("git")
            .args(["-C"])
            .arg(&caller_source)
            .args(["rev-parse", "--show-toplevel"]),
    )?;
    let git_root = PathBuf::from(git_root)
        .canonicalize()
        .context("resolve caller Git worktree root")?;
    ensure!(
        git_root == caller_source,
        "caller source must be the Git worktree root: {}",
        caller_source.display()
    );
    let actual_revision = capture(Command::new("git").args(["-C"]).arg(&caller_source).args([
        "rev-parse",
        "--verify",
        "HEAD",
    ]))?;
    ensure!(
        actual_revision == caller_revision,
        "caller source {} is at {}, expected {}",
        caller_source.display(),
        actual_revision,
        caller_revision
    );

    let config_path = absolute(config_path)?;
    let raw = read_text_bounded(&config_path, "build config")
        .with_context(|| format!("read build config {}", config_path.display()))?;
    let mut config: BuildConfig = toml::from_str(&raw)
        .with_context(|| format!("parse build config {}", config_path.display()))?;
    let matched_entries = materialize_config_for_caller(
        &mut config,
        caller_repository,
        &caller_source,
        caller_revision,
    )?;

    let mut rendered =
        toml::to_string_pretty(&config).context("serialize schema-v2 build config")?;
    rendered.push('\n');
    let round_trip: BuildConfig =
        toml::from_str(&rendered).context("parse serialized schema-v2 build config")?;
    validate_config(&round_trip).context("validate serialized schema-v2 build config")?;

    let output_path = absolute(output_path)?;
    let output_name = output_path
        .file_name()
        .context("materialized output must name a file")?;
    ensure!(
        matches!(
            Path::new(output_name).components().next(),
            Some(std::path::Component::Normal(_))
        ),
        "materialized output has an invalid file name"
    );
    let output_parent = output_path
        .parent()
        .context("materialized output has no parent directory")?;
    ensure_normal_directory("materialized output parent", output_parent)?;
    let output_path = output_parent.canonicalize()?.join(output_name);
    match fs::symlink_metadata(&output_path) {
        Ok(_) => anyhow::bail!(
            "refusing to overwrite materialized output {}",
            output_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mut temporary = tempfile::NamedTempFile::new_in(
        output_path
            .parent()
            .context("materialized output has no parent directory")?,
    )?;
    temporary.write_all(rendered.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&output_path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish materialized config {}", output_path.display()))?;

    Ok(MaterializeResult {
        output: output_path,
        matched_entries,
    })
}

fn materialize_config_for_caller(
    config: &mut BuildConfig,
    caller_repository: &str,
    caller_source: &Path,
    caller_revision: &str,
) -> Result<usize> {
    ensure!(
        caller_repository == OFFICIAL_UNION_REPOSITORY,
        "caller repository must be the exact official Union GitHub URL: {OFFICIAL_UNION_REPOSITORY}"
    );
    validate_repository(caller_repository)?;
    validate_revision(caller_revision)?;
    ensure!(
        caller_source.is_absolute(),
        "caller source must be an absolute checkout path"
    );
    validate_config(config)?;
    ensure!(
        config.distribution.repository.as_deref() == Some(caller_repository),
        "distribution repository does not exactly match caller repository {caller_repository}"
    );

    config.distribution.source = caller_source.to_path_buf();
    config.distribution.revision = caller_revision.to_owned();
    let mut matched_entries = 1;
    for module in &mut config.modules {
        if module.repository.as_deref() == Some(caller_repository) {
            module.source = caller_source.to_path_buf();
            module.revision = caller_revision.to_owned();
            matched_entries += 1;
        }
    }
    validate_config(config)?;
    Ok(matched_entries)
}

#[derive(Debug, Serialize)]
struct Plan<'a> {
    distribution: PlanTarget<'a>,
    modules: Vec<PlanModule<'a>>,
    activation_order: &'a [String],
}

#[derive(Debug, Serialize)]
struct PlanTarget<'a> {
    name: &'a str,
    version: &'a str,
    source: &'a Path,
    revision: &'a str,
    package: &'a str,
    binary: &'a str,
    frontend: &'a Frontend,
    install_path: String,
    platform: &'static str,
    architecture: &'static str,
    rust_target: &'static str,
}

#[derive(Debug, Serialize)]
struct PlanModule<'a> {
    id: &'a str,
    version: &'a str,
    source: &'a Path,
    revision: &'a str,
    package: &'a str,
    binary: &'a str,
    frontend: Option<&'a Frontend>,
    package_path: String,
    module_auth_routes: &'a [String],
}

#[derive(Debug, Serialize)]
struct ReleaseManifest<'a> {
    schema_version: u32,
    distribution: ReleaseDistribution<'a>,
    modules: Vec<ReleaseModule<'a>>,
    activation_order: &'a [String],
}

#[derive(Debug, Serialize)]
struct ReleaseDistribution<'a> {
    name: &'a str,
    version: &'a str,
    revision: &'a str,
    platform: &'static str,
    architecture: &'static str,
    executable: String,
    web_shell: String,
}

#[derive(Debug, Serialize)]
struct ReleaseModule<'a> {
    id: &'a str,
    version: &'a str,
    revision: &'a str,
    package: String,
    manifest: String,
    module_auth_routes: &'a [String],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseManifest {
    schema_version: u32,
    distribution: StoredReleaseDistribution,
    modules: Vec<StoredReleaseModule>,
    activation_order: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseDistribution {
    name: String,
    version: String,
    revision: String,
    platform: String,
    architecture: String,
    executable: String,
    web_shell: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseModule {
    id: String,
    version: String,
    revision: String,
    package: String,
    manifest: String,
    #[serde(default)]
    module_auth_routes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTemplate {
    manifest_version: u32,
    id: String,
    version: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    distribution: Option<String>,
    #[serde(default)]
    compatibility: Option<CompatibilitySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFile {
    manifest_version: u32,
    id: String,
    version: String,
    source_revision: String,
    compatibility: CompatibilitySnapshot,
    channel: String,
    distribution: String,
    license: String,
    platform_api_version: String,
    plugin_api_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatibilitySnapshot {
    core: String,
    platform_api: String,
    plugin_api: String,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    manifest_path: PathBuf,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

fn default_output() -> PathBuf {
    PathBuf::from("dist")
}

fn default_bundle() -> PathBuf {
    PathBuf::from(".")
}

pub fn load_and_check(config_path: &Path) -> Result<CheckedConfig> {
    let config_path = absolute(config_path)?;
    let config_dir = config_path
        .parent()
        .context("build config has no parent directory")?
        .to_path_buf();
    let raw = read_text_bounded(&config_path, "build config")
        .with_context(|| format!("read build config {}", config_path.display()))?;
    let mut config: BuildConfig = toml::from_str(&raw)
        .with_context(|| format!("parse build config {}", config_path.display()))?;
    validate_config(&config)?;

    config.distribution.source = resolve_source(
        &config_dir,
        &config.distribution.source,
        config.distribution.repository.as_deref(),
        &config.distribution.revision,
    )?;
    for module in &mut config.modules {
        module.source = resolve_source(
            &config_dir,
            &module.source,
            module.repository.as_deref(),
            &module.revision,
        )?;
    }

    let distribution_source = check_source(
        &config.distribution.source,
        &config.distribution.revision,
        config.require_clean_sources,
    )?;
    let module_sources = config
        .modules
        .iter()
        .map(|module| {
            check_source(
                &module.source,
                &module.revision,
                config.require_clean_sources,
            )
            .with_context(|| format!("check module {} source", module.id))
        })
        .collect::<Result<Vec<_>>>()?;
    let module_packages = config
        .modules
        .iter()
        .map(check_module_package)
        .collect::<Result<Vec<_>>>()?;
    validate_cargo_artifact(
        &config.distribution.source,
        &config.distribution.package,
        &config.distribution.binary,
        &config.distribution.version,
        "Core",
    )?;
    for (module, package) in config.modules.iter().zip(&module_packages) {
        validate_cargo_artifact(
            &module.source,
            &module.package,
            &module.binary,
            &package.manifest.version,
            &format!("module {}", module.id),
        )?;
    }
    let catalog = PluginCatalog::new(
        module_packages
            .iter()
            .map(|package| package.manifest.clone())
            .collect(),
    )
    .context("validate selected module dependency graph")?;
    let platform = PlatformVersions::parse(
        &config.distribution.version,
        PLATFORM_API_VERSION,
        PLUGIN_API_VERSION,
    )?;
    catalog
        .ensure_platform_compatible(&platform)
        .context("validate selected module compatibility")?;
    let activation_order = catalog
        .activation_order()
        .map(|manifest| manifest.id.clone())
        .collect();

    Ok(CheckedConfig {
        config,
        config_dir,
        distribution_source,
        module_sources,
        module_packages,
        activation_order,
    })
}

fn validate_config(config: &BuildConfig) -> Result<()> {
    ensure!(
        config.schema_version == RELEASE_SCHEMA_VERSION,
        "schema_version must be {RELEASE_SCHEMA_VERSION}"
    );
    validate_name("distribution name", &config.distribution.name)?;
    validate_name("distribution package", &config.distribution.package)?;
    validate_name("distribution binary", &config.distribution.binary)?;
    validate_version(&config.distribution.version)?;
    validate_revision(&config.distribution.revision)?;
    if let Some(repository) = &config.distribution.repository {
        validate_repository(repository)?;
    }
    validate_frontend("Web Shell", &config.distribution.frontend)?;

    let mut ids = BTreeSet::new();
    for module in &config.modules {
        validate_id(&module.id)?;
        ensure!(ids.insert(&module.id), "duplicate module id: {}", module.id);
        validate_name("module package", &module.package)?;
        validate_name("module binary", &module.binary)?;
        validate_revision(&module.revision)?;
        if let Some(repository) = &module.repository {
            validate_repository(repository)?;
        }
        validate_bundle_path(&module.bundle)?;
        validate_module_auth_route_ids(&module.id, &module.module_auth_routes)?;
        if let Some(frontend) = &module.frontend {
            validate_frontend(&format!("module {} frontend", module.id), frontend)?;
        }
        if config.distribution.repository.as_deref() == Some(OFFICIAL_UNION_REPOSITORY)
            && module.repository.as_deref() == Some(OFFICIAL_UNION_REPOSITORY)
        {
            ensure!(
                module.revision == config.distribution.revision,
                "Union-owned module {} must use the same revision as Core/Web Shell",
                module.id
            );
        }
    }
    Ok(())
}

fn check_module_package(module: &Module) -> Result<CheckedModulePackage> {
    let root = if module.bundle == Path::new(".") {
        ensure_normal_directory("module source", &module.source)?;
        module.source.clone()
    } else {
        ensure_safe_directory(&module.source, &module.bundle, "module bundle")?
    };
    ensure_normal_directory("module bundle", &root)
        .with_context(|| format!("module {} bundle", module.id))?;
    let manifest_path = root.join("manifest.json");
    ensure_safe_regular_file(&root, Path::new("manifest.json"), "module manifest", false)?;
    let raw = read_text_bounded(&manifest_path, "module manifest")?;
    let manifest = PluginManifest::parse_json(&raw)
        .with_context(|| format!("validate {}", manifest_path.display()))?;
    ensure!(
        manifest.id == module.id,
        "module config id {} does not match manifest id {}",
        module.id,
        manifest.id
    );
    ensure!(
        manifest.version_metadata.source_revision.is_none(),
        "module {} source manifest must omit source_revision; Builder stamps the locked revision",
        module.id
    );
    validate_union_release_policy(&manifest, &module.module_auth_routes)?;
    ensure!(
        !process_executable(&manifest)?
            .to_ascii_lowercase()
            .ends_with(".exe"),
        "module {} source executable must omit the Windows .exe suffix",
        module.id
    );
    validate_permissions_file(&root, &manifest)?;
    validate_config_schema(&root, &manifest)?;
    validate_version_template(&root, &manifest)?;
    validate_bundle_references(&root, &manifest, false)?;
    Ok(CheckedModulePackage {
        bundle_root: root,
        manifest,
    })
}

fn validate_permissions_file(root: &Path, manifest: &PluginManifest) -> Result<()> {
    let path = root.join("permissions.json");
    ensure_safe_regular_file(
        root,
        Path::new("permissions.json"),
        "permissions.json",
        false,
    )?;
    let permissions: Vec<PermissionDefinition> =
        serde_json::from_str(&read_text_bounded(&path, "permissions.json")?)
            .with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        permissions == manifest.permissions,
        "permissions.json must exactly match manifest permissions for {}",
        manifest.id
    );
    Ok(())
}

fn validate_config_schema(root: &Path, manifest: &PluginManifest) -> Result<()> {
    ensure!(
        manifest.configuration.schema == "config/schema.json",
        "module {} configuration schema must be config/schema.json",
        manifest.id
    );
    let path = root.join("config/schema.json");
    ensure_safe_regular_file(
        root,
        Path::new("config/schema.json"),
        "config/schema.json",
        false,
    )?;
    let schema: serde_json::Value =
        serde_json::from_str(&read_text_bounded(&path, "config/schema.json")?)
            .with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        schema.is_object(),
        "config/schema.json for {} must be a JSON object",
        manifest.id
    );
    Ok(())
}

fn validate_version_template(root: &Path, manifest: &PluginManifest) -> Result<()> {
    let path = root.join("version.json");
    ensure_safe_regular_file(root, Path::new("version.json"), "version.json", false)?;
    let version: VersionTemplate = serde_json::from_str(&read_text_bounded(&path, "version.json")?)
        .with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        version.manifest_version == manifest.manifest_version
            && version.id == manifest.id
            && version.version == manifest.version,
        "version.json identity/version does not match manifest for {}",
        manifest.id
    );
    if let Some(channel) = version.channel {
        ensure!(
            channel == release_channel(&manifest.version_metadata.channel),
            "version.json channel does not match manifest for {}",
            manifest.id
        );
    }
    if let Some(license) = version.license {
        ensure!(
            license == manifest.version_metadata.license,
            "version.json license does not match manifest for {}",
            manifest.id
        );
    }
    if let Some(distribution) = version.distribution {
        ensure!(
            distribution == "bundled",
            "version.json distribution must be bundled for {}",
            manifest.id
        );
    }
    if let Some(compatibility) = version.compatibility {
        let expected = CompatibilitySnapshot {
            core: manifest.compatibility.core.clone(),
            platform_api: manifest.compatibility.platform_api.clone(),
            plugin_api: manifest.compatibility.plugin_api.clone(),
        };
        ensure!(
            compatibility == expected,
            "version.json compatibility does not match manifest for {}",
            manifest.id
        );
    }
    Ok(())
}

fn validate_bundle_references(
    root: &Path,
    manifest: &PluginManifest,
    require_backend: bool,
) -> Result<()> {
    let executable = process_executable(manifest)?;
    let frontend = &manifest.frontend;
    ensure!(
        frontend.entry.starts_with("frontend/"),
        "module {} frontend entry must be below frontend/",
        manifest.id
    );
    ensure_safe_regular_file(root, Path::new(&frontend.entry), "frontend entry", false)?;
    for style in &frontend.styles {
        ensure!(
            style.starts_with("frontend/"),
            "module {} frontend styles must be below frontend/",
            manifest.id
        );
        ensure_safe_regular_file(root, Path::new(style), "frontend style", false)?;
    }
    let frontend_directory =
        ensure_safe_directory(root, Path::new("frontend"), "frontend directory")?;
    let config_directory = ensure_safe_directory(root, Path::new("config"), "config directory")?;
    scan_tree(&frontend_directory)?;
    scan_tree(&config_directory)?;
    if let Some(notes) = &manifest.version_metadata.release_notes {
        ensure_safe_regular_file(root, Path::new(notes), "release notes", false)?;
    }
    for migration in &manifest.migrations {
        match migration.engine {
            MigrationEngine::Embedded => {}
            MigrationEngine::Postgresql | MigrationEngine::Sqlite => {
                let directory = migration
                    .directory
                    .as_deref()
                    .context("non-embedded migration has no directory")?;
                let directory =
                    ensure_safe_directory(root, Path::new(directory), "migration directory")?;
                scan_tree(&directory)?;
            }
        }
    }
    if require_backend {
        ensure_safe_regular_file(root, Path::new(executable), "module executable", true)?;
    }
    Ok(())
}

fn process_executable(manifest: &PluginManifest) -> Result<&str> {
    let executable = match &manifest.execution {
        Execution::Process { executable, .. } => executable.as_str(),
        _ => anyhow::bail!("release module {} is not a process", manifest.id),
    };
    let path = Path::new(executable);
    let components = path.components().collect::<Vec<_>>();
    ensure!(
        components.len() == 2
            && components[0].as_os_str() == "backend"
            && matches!(components[1], std::path::Component::Normal(_)),
        "module {} executable must be backend/<executable>",
        manifest.id
    );
    Ok(executable)
}

fn validate_union_release_policy(
    manifest: &PluginManifest,
    module_auth_routes: &[String],
) -> Result<()> {
    ensure!(
        matches!(manifest.execution, Execution::Process { .. }),
        "module {} must use process execution in a Union release",
        manifest.id
    );
    let allowed_module_auth_routes =
        validate_module_auth_route_ids(&manifest.id, module_auth_routes)?;
    let manifest_module_auth_routes = manifest
        .backend
        .routes
        .iter()
        .filter(|route| matches!(route.auth, RouteAuth::Module))
        .map(|route| route.id.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        manifest_module_auth_routes == allowed_module_auth_routes,
        "module {} Manifest module-auth route ids {:?} must exactly match profile module_auth_routes {:?}",
        manifest.id,
        manifest_module_auth_routes,
        allowed_module_auth_routes,
    );
    let backend_service = manifest
        .services
        .iter()
        .find(|service| service.name == manifest.backend.service)
        .context("validated module has no backend service")?;
    ensure!(
        matches!(backend_service.visibility, ServiceVisibility::Platform),
        "module {} backend service must have platform visibility",
        manifest.id
    );
    Ok(())
}

fn validate_module_auth_route_ids<'a>(
    module_id: &str,
    route_ids: &'a [String],
) -> Result<BTreeSet<&'a str>> {
    let mut unique = BTreeSet::new();
    for route_id in route_ids {
        ensure!(
            !route_id.is_empty()
                && route_id.len() <= 64
                && route_id.starts_with(|character: char| character.is_ascii_lowercase())
                && route_id.ends_with(|character: char| character.is_ascii_alphanumeric())
                && route_id
                    .chars()
                    .all(|character| character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || character == '-')
                && !route_id.contains("--"),
            "module {module_id} has invalid module_auth_routes route id: {route_id}"
        );
        ensure!(
            unique.insert(route_id.as_str()),
            "module {module_id} has duplicate module_auth_routes route id: {route_id}"
        );
    }
    Ok(unique)
}

fn validate_frontend(label: &str, frontend: &Frontend) -> Result<()> {
    validate_relative_path(&format!("{label} directory"), &frontend.directory)?;
    validate_relative_path(&format!("{label} output"), &frontend.output)
}

fn validate_bundle_path(value: &Path) -> Result<()> {
    if value == Path::new(".") {
        return Ok(());
    }
    validate_relative_path("module bundle", value)
}

fn validate_relative_path(label: &str, value: &Path) -> Result<()> {
    ensure!(
        !value.as_os_str().is_empty()
            && !value.is_absolute()
            && value
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "{label} must be a safe relative path: {}",
        value.display()
    );
    Ok(())
}

fn validate_name(label: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character)),
        "{label} is invalid: {value}"
    );
    Ok(())
}

fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value.starts_with(|character: char| character.is_ascii_lowercase())
            && value.ends_with(|character: char| character.is_ascii_alphanumeric())
            && value.chars().all(|character| character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'),
        "module id is invalid: {value}"
    );
    Ok(())
}

fn validate_version(value: &str) -> Result<()> {
    let parts = value.split('.').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty()
                    && part.chars().all(|character| character.is_ascii_digit())),
        "distribution version must be MAJOR.MINOR.PATCH: {value}"
    );
    Ok(())
}

fn validate_revision(value: &str) -> Result<()> {
    ensure!(
        value.len() == 40
            && value.bytes().any(|byte| byte != b'0')
            && value
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()),
        "revision must be a non-zero canonical lowercase 40-character Git object id: {value}"
    );
    Ok(())
}

fn validate_repository(value: &str) -> Result<()> {
    let identity = value
        .strip_prefix("https://github.com/")
        .and_then(|value| value.strip_suffix(".git"));
    let components = identity
        .map(|value| value.split('/').collect::<Vec<_>>())
        .unwrap_or_default();
    ensure!(
        components.len() == 2
            && components.iter().all(|component| {
                !component.is_empty()
                    && component
                        .chars()
                        .any(|character| character.is_ascii_alphanumeric())
                    && component.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                    })
            }),
        "repository must be a credential-free HTTPS GitHub URL ending in .git: {value}"
    );
    Ok(())
}

fn resolve_source(
    config_dir: &Path,
    source: &Path,
    repository: Option<&str>,
    revision: &str,
) -> Result<PathBuf> {
    let joined = if source.is_absolute() {
        source.to_path_buf()
    } else {
        config_dir.join(source)
    };
    if !joined.exists() {
        let repository = repository.with_context(|| {
            format!(
                "source directory {} is missing and no repository was configured",
                joined.display()
            )
        })?;
        fetch_source(repository, revision, &joined)?;
    }
    joined
        .canonicalize()
        .with_context(|| format!("resolve source directory {}", joined.display()))
}

fn fetch_source(repository: &str, revision: &str, destination: &Path) -> Result<()> {
    ensure!(
        !destination.exists(),
        "refusing to replace existing source path {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .context("source destination has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".union-source-")
        .tempdir_in(parent)?;
    let checkout = temporary.path();
    run(Command::new("git").args(["init", "--quiet"]).arg(checkout))?;
    run(Command::new("git")
        .args(["-C"])
        .arg(checkout)
        .args(["remote", "add", "origin", repository]))?;
    run(Command::new("git")
        .args(["-C"])
        .arg(checkout)
        .args(["fetch", "--quiet", "--depth", "1", "origin", revision]))?;
    run(Command::new("git").args(["-C"]).arg(checkout).args([
        "checkout",
        "--quiet",
        "--detach",
        "FETCH_HEAD",
    ]))?;
    let checkout = temporary.keep();
    fs::rename(&checkout, destination).with_context(|| {
        format!(
            "publish fetched source {} as {}",
            checkout.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn check_source(
    path: &Path,
    expected_revision: &str,
    require_clean: bool,
) -> Result<SourceIdentity> {
    ensure!(
        path.join("Cargo.toml").is_file(),
        "{} has no Cargo.toml",
        path.display()
    );
    validate_legal_source(path)?;
    let revision = capture(
        Command::new("git")
            .args(["-C"])
            .arg(path)
            .args(["rev-parse", "HEAD"]),
    )?;
    ensure!(
        revision == expected_revision,
        "source {} is at {}, expected {}",
        path.display(),
        revision,
        expected_revision
    );
    if require_clean {
        let status = capture(
            Command::new("git")
                .args(["-C"])
                .arg(path)
                .args(["status", "--porcelain"]),
        )?;
        ensure!(
            status.is_empty(),
            "source {} has uncommitted changes",
            path.display()
        );
    }
    Ok(SourceIdentity {
        path: path.to_path_buf(),
        revision,
    })
}

fn validate_legal_source(source: &Path) -> Result<()> {
    let mut found = false;
    for name in ["LICENSE", "LICENSE-APACHE"] {
        let path = source.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "legal file must be a non-symlink regular file: {}",
                    path.display()
                );
                found = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        found,
        "source {} must provide LICENSE or LICENSE-APACHE",
        source.display()
    );
    Ok(())
}

fn validate_cargo_artifact(
    source: &Path,
    package: &str,
    binary: &str,
    expected_version: &str,
    label: &str,
) -> Result<()> {
    let raw = capture(
        Command::new("cargo")
            .args(["metadata", "--no-deps", "--locked", "--format-version", "1"])
            .arg("--manifest-path")
            .arg(source.join("Cargo.toml")),
    )
    .with_context(|| format!("inspect {label} Cargo workspace"))?;
    let metadata: CargoMetadata =
        serde_json::from_str(&raw).with_context(|| format!("parse {label} Cargo metadata"))?;
    let packages = metadata
        .packages
        .iter()
        .filter(|candidate| candidate.name == package)
        .collect::<Vec<_>>();
    ensure!(
        packages.len() == 1,
        "{label} Cargo package {package} must exist exactly once"
    );
    let package = packages[0];
    ensure!(
        package.version == expected_version,
        "{label} Cargo version {} does not match release version {expected_version}",
        package.version
    );
    validate_cargo_source_path(
        source,
        &package.manifest_path,
        &format!("{label} Cargo manifest"),
    )?;
    let target = package
        .targets
        .iter()
        .find(|target| target.name == binary && target.kind.iter().any(|kind| kind == "bin"))
        .with_context(|| format!("{label} Cargo package has no binary target named {binary}"))?;
    validate_cargo_source_path(source, &target.src_path, &format!("{label} binary source"))?;
    Ok(())
}

fn validate_cargo_source_path(source: &Path, path: &Path, label: &str) -> Result<()> {
    let relative = path
        .strip_prefix(source)
        .with_context(|| format!("{label} {} escapes locked source", path.display()))?;
    let resolved = resolve_safe_member(source, relative, label)?;
    ensure_regular_file(label, &resolved, false)
}

fn capture(command: &mut Command) -> Result<String> {
    let debug = format!("{command:?}");
    let output = command.output().with_context(|| format!("run {debug}"))?;
    ensure!(
        output.status.success(),
        "command failed: {debug}\n{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn run(command: &mut Command) -> Result<()> {
    let debug = format!("{command:?}");
    let status = command.status().with_context(|| format!("run {debug}"))?;
    ensure!(status.success(), "command failed: {debug}");
    Ok(())
}

pub fn render_plan(
    checked: &CheckedConfig,
    server_target: Option<ServerTarget>,
    format: OutputFormat,
) -> Result<String> {
    let server_target = resolve_server_target(server_target)?;
    let plan = make_plan(checked, server_target);
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(&plan)?),
        OutputFormat::Text => {
            let mut lines = vec![
                format!(
                    "server target {} (Rust target {})",
                    server_target,
                    server_target.rust_target()
                ),
                format!(
                    "core {} {}: build Cargo package {} once -> {}",
                    plan.distribution.name,
                    plan.distribution.version,
                    plan.distribution.package,
                    plan.distribution.install_path
                ),
            ];
            lines.push(format!(
                "Web Shell: npm ci && npm run build in {}",
                plan.distribution.frontend.directory.display()
            ));
            for module in plan.modules {
                lines.push(format!(
                    "bundled module {} {}: build {} -> {}",
                    module.id, module.version, module.package, module.package_path
                ));
                if !module.module_auth_routes.is_empty() {
                    lines.push(format!(
                        "  module-auth route exceptions: {}",
                        module.module_auth_routes.join(", ")
                    ));
                }
            }
            lines.push(
                "runtime enable/disable state: owned by Union Core and absent from this plan"
                    .to_owned(),
            );
            if !plan.activation_order.is_empty() {
                lines.push(format!(
                    "dependency activation order: {}",
                    plan.activation_order.join(" -> ")
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

fn make_plan(checked: &CheckedConfig, server_target: ServerTarget) -> Plan<'_> {
    Plan {
        distribution: PlanTarget {
            name: &checked.config.distribution.name,
            version: &checked.config.distribution.version,
            source: &checked.distribution_source.path,
            revision: &checked.distribution_source.revision,
            package: &checked.config.distribution.package,
            binary: &checked.config.distribution.binary,
            frontend: &checked.config.distribution.frontend,
            install_path: format!("bin/{}", checked.config.distribution.binary),
            platform: server_target.platform(),
            architecture: server_target.architecture(),
            rust_target: server_target.rust_target(),
        },
        modules: checked
            .config
            .modules
            .iter()
            .zip(&checked.module_sources)
            .zip(&checked.module_packages)
            .map(|((module, source), package)| PlanModule {
                id: &module.id,
                version: &package.manifest.version,
                source: &source.path,
                revision: &source.revision,
                package: &module.package,
                binary: &module.binary,
                frontend: module.frontend.as_ref(),
                package_path: format!("modules/{}", module.id),
                module_auth_routes: &module.module_auth_routes,
            })
            .collect(),
        activation_order: &checked.activation_order,
    }
}

pub fn build(config_path: &Path, options: BuildOptions) -> Result<BuildResult> {
    ensure!(
        options.cargo_profile == "release" || options.cargo_profile == "debug",
        "Cargo profile must be release or debug"
    );
    let server_target = resolve_server_target(options.server_target)?;
    let checked = load_and_check(config_path)?;
    let final_output = options
        .output
        .as_ref()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                checked.config_dir.join(path)
            }
        })
        .unwrap_or_else(|| checked.config_dir.join(&checked.config.distribution.output));
    ensure!(
        !final_output.exists(),
        "output {} already exists; refusing to overwrite it",
        final_output.display()
    );

    npm_build(
        &checked.config.distribution.source,
        &checked.config.distribution.frontend,
    )?;
    cargo_build(
        &checked.config.distribution.source,
        &checked.config.distribution.package,
        &checked.config.distribution.binary,
        &options,
        server_target,
    )?;
    for module in &checked.config.modules {
        if let Some(frontend) = &module.frontend {
            npm_build(&module.source, frontend)
                .with_context(|| format!("build module {} frontend", module.id))?;
        }
        cargo_build(
            &module.source,
            &module.package,
            &module.binary,
            &options,
            server_target,
        )
        .with_context(|| format!("build module {} backend", module.id))?;
    }
    if checked.config.require_clean_sources {
        check_source(
            &checked.config.distribution.source,
            &checked.distribution_source.revision,
            true,
        )
        .context("Core/Web Shell source changed during the build")?;
        for (module, source) in checked.config.modules.iter().zip(&checked.module_sources) {
            check_source(&module.source, &source.revision, true)
                .with_context(|| format!("module {} source changed during the build", module.id))?;
        }
    }

    let output_parent = final_output
        .parent()
        .context("release output has no parent directory")?;
    fs::create_dir_all(output_parent)?;
    ensure_normal_directory("release output parent", output_parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".union-build-")
        .tempdir_in(output_parent)?;
    let output = staging.path().join("payload");

    let bin_dir = output.join("bin");
    let modules_dir = output.join("modules");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&modules_dir)?;
    copy_legal_files(
        &checked.config.distribution.source,
        &output.join("share/licenses/unionc"),
    )?;
    let core_artifact = artifact_path(
        &checked.config.distribution.source,
        &options,
        &checked.config.distribution.binary,
        server_target,
    );
    let core_install = bin_dir.join(&checked.config.distribution.binary);
    copy_executable(&core_artifact, &core_install)?;
    let shell_source = checked
        .config
        .distribution
        .source
        .join(&checked.config.distribution.frontend.directory)
        .join(&checked.config.distribution.frontend.output);
    let shell_install = output.join("share/union/web");
    copy_tree(&shell_source, &shell_install)?;

    for ((module, source), package) in checked
        .config
        .modules
        .iter()
        .zip(&checked.module_sources)
        .zip(&checked.module_packages)
    {
        let destination = modules_dir.join(&module.id);
        copy_bundle_template(
            &package.bundle_root,
            &destination,
            &package.manifest,
            module.frontend.is_some(),
        )?;
        if let Some(frontend) = &module.frontend {
            let generated = module
                .source
                .join(&frontend.directory)
                .join(&frontend.output);
            copy_tree(&generated, &destination.join("frontend"))?;
        }
        let mut manifest = package.manifest.clone();
        manifest.version_metadata.source_revision = Some(source.revision.clone());
        let executable = stamp_target_executable(&mut manifest)?;
        fs::write(
            destination.join("manifest.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest)?),
        )?;
        fs::write(
            destination.join("version.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&final_version_file(&manifest, &source.revision))?
            ),
        )?;
        let artifact = artifact_path(&module.source, &options, &module.binary, server_target);
        let install = destination.join(&executable);
        copy_executable(&artifact, &install)?;
        copy_legal_files(
            &module.source,
            &output.join("share/licenses/modules").join(&module.id),
        )?;
        validate_built_package(
            &destination,
            &module.id,
            &manifest.version,
            &source.revision,
            &module.module_auth_routes,
        )?;
    }

    let manifest_path = output.join("union-release.json");
    let manifest = ReleaseManifest {
        schema_version: RELEASE_SCHEMA_VERSION,
        distribution: ReleaseDistribution {
            name: &checked.config.distribution.name,
            version: &checked.config.distribution.version,
            revision: &checked.distribution_source.revision,
            platform: server_target.platform(),
            architecture: server_target.architecture(),
            executable: relative(&output, &core_install)?,
            web_shell: relative(&output, &shell_install)?,
        },
        modules: checked
            .config
            .modules
            .iter()
            .zip(&checked.module_sources)
            .zip(&checked.module_packages)
            .map(|((module, source), package)| ReleaseModule {
                id: &module.id,
                version: &package.manifest.version,
                revision: &source.revision,
                package: format!("modules/{}", module.id),
                manifest: format!("modules/{}/manifest.json", module.id),
                module_auth_routes: &module.module_auth_routes,
            })
            .collect(),
        activation_order: &checked.activation_order,
    };
    fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    let checksums_path = output.join("SHA256SUMS");
    scan_tree(&output)?;
    write_checksums(&output, &checksums_path)?;
    verify_release_for_target(&output, server_target)?;
    fs::rename(&output, &final_output)
        .with_context(|| format!("publish release as {}", final_output.display()))?;
    Ok(BuildResult {
        manifest: final_output.join("union-release.json"),
        checksums: final_output.join("SHA256SUMS"),
        output: final_output,
        server_target,
    })
}

fn final_version_file(manifest: &PluginManifest, revision: &str) -> VersionFile {
    VersionFile {
        manifest_version: manifest.manifest_version,
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        source_revision: revision.to_owned(),
        compatibility: CompatibilitySnapshot {
            core: manifest.compatibility.core.clone(),
            platform_api: manifest.compatibility.platform_api.clone(),
            plugin_api: manifest.compatibility.plugin_api.clone(),
        },
        channel: release_channel(&manifest.version_metadata.channel).to_owned(),
        distribution: "bundled".to_owned(),
        license: manifest.version_metadata.license.clone(),
        platform_api_version: PLATFORM_API_VERSION.to_owned(),
        plugin_api_version: PLUGIN_API_VERSION.to_owned(),
    }
}

fn release_channel(channel: &ReleaseChannel) -> &'static str {
    match channel {
        ReleaseChannel::Development => "development",
        ReleaseChannel::Beta => "beta",
        ReleaseChannel::Stable => "stable",
    }
}

/// Verify package contracts, executable modes, and the exact SHA256SUMS inventory.
pub fn verify_release(release: &Path) -> Result<VerificationResult> {
    ensure_normal_directory("release", release)?;
    scan_tree(release)?;
    let manifest_path = release.join("union-release.json");
    let manifest: StoredReleaseManifest =
        serde_json::from_str(&read_text_bounded(&manifest_path, "release manifest")?)
            .with_context(|| format!("parse {}", manifest_path.display()))?;
    let server_target = validate_stored_manifest(release, &manifest)?;
    let checksums_path = release.join("SHA256SUMS");
    let checksum_text = read_text_bounded(&checksums_path, "SHA256SUMS")?;
    let expected = parse_checksums(&checksum_text)?;
    let mut files = Vec::new();
    collect_files(release, &mut files)?;
    let actual_paths = files
        .iter()
        .filter(|path| *path != &checksums_path)
        .map(|path| relative(release, path))
        .collect::<Result<BTreeSet<_>>>()?;
    let expected_paths = expected.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        actual_paths == expected_paths,
        "SHA256SUMS inventory does not exactly match release files"
    );
    for (path, digest) in &expected {
        ensure!(
            sha256(&release.join(path))? == *digest,
            "checksum mismatch for {path}"
        );
    }
    let release_id = release_id(&manifest, &checksum_text)?;
    Ok(VerificationResult {
        release: release.to_path_buf(),
        release_id,
        files: expected.len(),
        server_target,
    })
}

/// Verify a release and require the architecture selected by its caller.
pub fn verify_release_for_target(
    release: &Path,
    expected_target: ServerTarget,
) -> Result<VerificationResult> {
    let verified = verify_release(release)?;
    ensure!(
        verified.server_target == expected_target,
        "release target is {}, expected {}",
        verified.server_target,
        expected_target
    );
    Ok(verified)
}

fn validate_stored_manifest(
    release: &Path,
    manifest: &StoredReleaseManifest,
) -> Result<ServerTarget> {
    ensure!(
        manifest.schema_version == RELEASE_SCHEMA_VERSION,
        "unsupported release schema"
    );
    validate_name("release distribution name", &manifest.distribution.name)?;
    validate_version(&manifest.distribution.version)?;
    validate_revision(&manifest.distribution.revision)?;
    let server_target = ServerTarget::from_release_fields(
        &manifest.distribution.platform,
        &manifest.distribution.architecture,
    )?;
    validate_release_file(
        release,
        "Core executable",
        &manifest.distribution.executable,
        true,
    )?;
    let executable_components = Path::new(&manifest.distribution.executable)
        .components()
        .collect::<Vec<_>>();
    ensure!(
        executable_components.len() == 2
            && executable_components[0].as_os_str() == "bin"
            && matches!(executable_components[1], std::path::Component::Normal(_)),
        "Core executable must use canonical bin/<executable> layout"
    );
    ensure!(
        manifest.distribution.web_shell == "share/union/web",
        "Web Shell must use canonical share/union/web layout"
    );
    let web_shell = ensure_safe_directory(
        release,
        Path::new(&manifest.distribution.web_shell),
        "Web Shell",
    )?;
    ensure_safe_regular_file(
        &web_shell,
        Path::new("index.html"),
        "Web Shell index.html",
        false,
    )?;
    let mut ids = BTreeSet::new();
    let mut manifests = Vec::new();
    for module in &manifest.modules {
        validate_id(&module.id)?;
        ensure!(
            ids.insert(&module.id),
            "duplicate release module id: {}",
            module.id
        );
        validate_version(&module.version)?;
        validate_revision(&module.revision)?;
        let expected_package = format!("modules/{}", module.id);
        let expected_manifest = format!("{expected_package}/manifest.json");
        ensure!(
            module.package == expected_package && module.manifest == expected_manifest,
            "release module {} must use canonical modules/<id> paths",
            module.id
        );
        validate_release_directory(release, "module package", &module.package)?;
        validate_release_file(release, "module manifest", &module.manifest, false)?;
        manifests.push(validate_built_package(
            &release.join(&module.package),
            &module.id,
            &module.version,
            &module.revision,
            &module.module_auth_routes,
        )?);
    }
    validate_module_directory_inventory(release, &ids)?;
    let catalog = PluginCatalog::new(manifests).context("validate release dependency graph")?;
    let platform = PlatformVersions::parse(
        &manifest.distribution.version,
        PLATFORM_API_VERSION,
        PLUGIN_API_VERSION,
    )?;
    catalog.ensure_platform_compatible(&platform)?;
    let order = catalog
        .activation_order()
        .map(|plugin| plugin.id.clone())
        .collect::<Vec<_>>();
    ensure!(
        order == manifest.activation_order,
        "invalid activation_order"
    );
    Ok(server_target)
}

fn validate_built_package(
    root: &Path,
    expected_id: &str,
    expected_version: &str,
    expected_revision: &str,
    module_auth_routes: &[String],
) -> Result<PluginManifest> {
    ensure_normal_directory("module package", root)?;
    scan_tree(root)?;
    let manifest_path = root.join("manifest.json");
    ensure_regular_file("module manifest", &manifest_path, false)?;
    let manifest =
        PluginManifest::parse_json(&read_text_bounded(&manifest_path, "module manifest")?)
            .with_context(|| format!("validate {}", manifest_path.display()))?;
    ensure!(
        manifest.id == expected_id && manifest.version == expected_version,
        "module package identity/version mismatch for {expected_id}"
    );
    ensure!(
        manifest.version_metadata.source_revision.as_deref() == Some(expected_revision),
        "module package source revision mismatch for {expected_id}"
    );
    validate_union_release_policy(&manifest, module_auth_routes)?;
    validate_permissions_file(root, &manifest)?;
    validate_config_schema(root, &manifest)?;
    validate_bundle_references(root, &manifest, true)?;
    let actual_version: VersionFile = serde_json::from_str(&read_text_bounded(
        &root.join("version.json"),
        "version.json",
    )?)
    .with_context(|| format!("parse {}/version.json", root.display()))?;
    ensure!(
        actual_version == final_version_file(&manifest, expected_revision),
        "version.json generated metadata mismatch for {expected_id}"
    );
    Ok(manifest)
}

fn validate_module_directory_inventory(release: &Path, expected: &BTreeSet<&String>) -> Result<()> {
    let modules = release.join("modules");
    ensure_normal_directory("modules directory", &modules)?;
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(&modules)? {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_dir(),
            "modules/ may contain only directories"
        );
        actual.insert(entry.file_name().to_string_lossy().into_owned());
    }
    let expected = expected
        .iter()
        .map(|id| (*id).clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        actual == expected,
        "modules directory does not match release manifest"
    );
    Ok(())
}

fn resolve_safe_member(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    validate_relative_path(label, relative)?;
    ensure_normal_directory("bundle root", root)?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            unreachable!("relative path was validated")
        };
        cursor.push(component);
        let metadata = fs::symlink_metadata(&cursor)
            .with_context(|| format!("{label} {} is missing", cursor.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "{label} path may not contain symlinks: {}",
            cursor.display()
        );
    }
    Ok(cursor)
}

fn ensure_safe_regular_file(
    root: &Path,
    relative: &Path,
    label: &str,
    executable: bool,
) -> Result<PathBuf> {
    let path = resolve_safe_member(root, relative, label)?;
    ensure_regular_file(label, &path, executable)?;
    Ok(path)
}

fn ensure_safe_directory(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    let path = resolve_safe_member(root, relative, label)?;
    ensure_normal_directory(label, &path)?;
    Ok(path)
}

fn ensure_regular_file(label: &str, path: &Path, executable: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} {} is missing", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular file"
    );
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "{label} must be executable"
        );
    }
    let _ = executable;
    Ok(())
}

fn read_text_bounded(path: &Path, label: &str) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} {} is missing", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular file"
    );
    ensure!(
        metadata.len() <= MAX_METADATA_BYTES,
        "{label} exceeds the {MAX_METADATA_BYTES}-byte metadata limit"
    );
    fs::read_to_string(path).with_context(|| format!("read {label} {}", path.display()))
}

fn validate_release_file(release: &Path, label: &str, value: &str, executable: bool) -> Result<()> {
    let path = Path::new(value);
    validate_relative_path(label, path)?;
    ensure_regular_file(label, &release.join(path), executable)
}

fn validate_release_directory(release: &Path, label: &str, value: &str) -> Result<()> {
    let path = Path::new(value);
    validate_relative_path(label, path)?;
    ensure_normal_directory(label, &release.join(path))
}

fn write_checksums(release: &Path, checksums_path: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect_files(release, &mut files)?;
    files.retain(|path| path != checksums_path);
    files.sort();
    let mut text = String::new();
    for path in files {
        text.push_str(&format!(
            "{}  {}\n",
            sha256(&path)?,
            relative(release, &path)?
        ));
    }
    fs::write(checksums_path, text)?;
    Ok(())
}

fn parse_checksums(value: &str) -> Result<BTreeMap<String, String>> {
    let mut checksums = BTreeMap::new();
    ensure!(!value.is_empty(), "SHA256SUMS is empty");
    ensure!(
        value.ends_with('\n') && !value.contains('\r'),
        "SHA256SUMS must use canonical LF-terminated lines"
    );
    let mut previous_path: Option<&str> = None;
    for (index, line) in value.lines().enumerate() {
        let (digest, path) = line
            .split_once("  ")
            .with_context(|| format!("invalid SHA256SUMS line {}", index + 1))?;
        ensure!(
            digest.len() == 64
                && digest
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()
                        && !character.is_ascii_uppercase()),
            "invalid SHA-256 on line {}",
            index + 1
        );
        validate_relative_path("checksum path", Path::new(path))?;
        ensure!(path != "SHA256SUMS", "SHA256SUMS must not hash itself");
        ensure!(
            previous_path.is_none_or(|previous| previous < path),
            "SHA256SUMS paths must be strictly sorted"
        );
        previous_path = Some(path);
        ensure!(
            checksums
                .insert(path.to_owned(), digest.to_owned())
                .is_none(),
            "duplicate checksum path: {path}"
        );
    }
    Ok(checksums)
}

fn release_id(manifest: &StoredReleaseManifest, checksums: &str) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(checksums.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let value = format!(
        "{}-{}-{}",
        manifest.distribution.name, manifest.distribution.version, digest
    );
    validate_release_id(&value)?;
    Ok(value)
}

pub fn stage_release(release: &Path, root: &Path) -> Result<InstallResult> {
    let verified = verify_release(release)?;
    ensure_install_root(root)?;
    let releases = root.join("releases");
    fs::create_dir_all(&releases)?;
    ensure_normal_directory("releases directory", &releases)?;
    let destination = releases.join(&verified.release_id);
    if destination.exists() {
        let installed = verify_release(&destination)?;
        ensure!(
            installed.release_id == verified.release_id,
            "installed release slot does not match its id"
        );
    } else {
        let temporary = tempfile::Builder::new()
            .prefix(".union-stage-")
            .tempdir_in(&releases)?;
        let payload = temporary.path().join("payload");
        copy_tree(release, &payload)?;
        let staged = verify_release(&payload)?;
        ensure!(
            staged.release_id == verified.release_id,
            "staged release changed while it was copied"
        );
        fs::rename(&payload, &destination)?;
    }
    Ok(InstallResult {
        release_id: verified.release_id,
        release: destination,
        previous_release_id: read_pointer(root, "current")?,
    })
}

/// Runtime enable/disable state remains owned by Core and is not changed here.
pub fn install_release(release: &Path, root: &Path) -> Result<InstallResult> {
    let mut result = stage_release(release, root)?;
    let current = read_pointer(root, "current")?;
    let previous = read_pointer(root, "previous")?;
    ensure!(
        current.is_some() || previous.is_none(),
        "install root has previous but no current pointer"
    );
    if current.as_deref() == Some(&result.release_id) {
        result.previous_release_id = previous;
        return Ok(result);
    }
    if let Some(current) = &current {
        switch_pointer(root, "previous", current)?;
    }
    switch_pointer(root, "current", &result.release_id)?;
    result.previous_release_id = current;
    Ok(result)
}

/// This rolls back immutable release files, never module databases or stored data.
pub fn rollback_install(root: &Path) -> Result<InstallResult> {
    ensure_normal_directory("install root", root)?;
    let current = read_pointer(root, "current")?.context("no active Union release")?;
    let previous = read_pointer(root, "previous")?.context("no previous Union release")?;
    let destination = root.join("releases").join(&previous);
    let verified = verify_release(&destination)?;
    ensure!(verified.release_id == previous, "previous slot id mismatch");
    switch_pointer(root, "current", &previous)?;
    switch_pointer(root, "previous", &current)?;
    Ok(InstallResult {
        release_id: previous,
        release: destination,
        previous_release_id: Some(current),
    })
}

fn ensure_install_root(root: &Path) -> Result<()> {
    if root.exists() {
        ensure_normal_directory("install root", root)
    } else {
        fs::create_dir_all(root)?;
        ensure_normal_directory("install root", root)
    }
}

fn ensure_normal_directory(label: &str, path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} {} is missing", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "{label} must be a real directory"
    );
    Ok(())
}

fn validate_release_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            }),
        "invalid release id: {value}"
    );
    Ok(())
}

fn read_pointer(root: &Path, name: &str) -> Result<Option<String>> {
    let path = root.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_symlink(),
        "{} must be a symlink",
        path.display()
    );
    let target = fs::read_link(&path)?;
    let components = target.components().collect::<Vec<_>>();
    ensure!(
        components.len() == 2
            && components[0].as_os_str() == "releases"
            && matches!(components[1], std::path::Component::Normal(_)),
        "{} has an unsafe target",
        path.display()
    );
    let release_id = components[1].as_os_str().to_string_lossy().into_owned();
    validate_release_id(&release_id)?;
    ensure_normal_directory("pointed release", &root.join(&target))?;
    Ok(Some(release_id))
}

#[cfg(unix)]
fn switch_pointer(root: &Path, name: &str, release_id: &str) -> Result<()> {
    use std::os::unix::fs::symlink;
    validate_release_id(release_id)?;
    ensure_normal_directory("release slot", &root.join("releases").join(release_id))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = root.join(format!(".{name}-{}-{nonce}.tmp", std::process::id()));
    symlink(Path::new("releases").join(release_id), &temporary)?;
    let destination = root.join(name);
    if destination.exists() && !fs::symlink_metadata(&destination)?.file_type().is_symlink() {
        fs::remove_file(&temporary)?;
        anyhow::bail!("refusing to replace non-symlink {}", destination.display());
    }
    if let Err(error) = fs::rename(&temporary, &destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("atomically switch {}", destination.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn switch_pointer(_root: &Path, _name: &str, _release_id: &str) -> Result<()> {
    anyhow::bail!("atomic install activation is currently supported on Unix only")
}

fn npm_build(source: &Path, frontend: &Frontend) -> Result<()> {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    npm_build_with(source, frontend, Path::new(npm))
}

fn npm_build_with(source: &Path, frontend: &Frontend, npm: &Path) -> Result<()> {
    let directory = ensure_safe_directory(source, &frontend.directory, "frontend source")?;
    ensure_safe_regular_file(
        &directory,
        Path::new("package.json"),
        "frontend package.json",
        false,
    )?;
    ensure_safe_regular_file(
        &directory,
        Path::new("package-lock.json"),
        "frontend package-lock.json",
        false,
    )?;
    run(Command::new(npm).current_dir(&directory).args(["ci"]))?;
    run(Command::new(npm)
        .current_dir(&directory)
        .args(["run", "build"]))?;
    let generated = ensure_safe_directory(&directory, &frontend.output, "frontend build output")
        .context("frontend build did not create a safe output directory")?;
    scan_tree(&generated)?;
    Ok(())
}

fn copy_bundle_template(
    source: &Path,
    destination: &Path,
    manifest: &PluginManifest,
    replace_frontend: bool,
) -> Result<()> {
    ensure_normal_directory("module bundle", source)?;
    ensure!(!destination.exists(), "bundle destination already exists");
    fs::create_dir_all(destination)?;
    for file in ["manifest.json", "permissions.json", "version.json"] {
        copy_declared_file(source, destination, Path::new(file))?;
    }
    copy_declared_file(source, destination, Path::new("config/schema.json"))?;
    if !replace_frontend {
        copy_tree(&source.join("frontend"), &destination.join("frontend"))?;
    }
    let mut migration_directories = BTreeSet::new();
    for migration in &manifest.migrations {
        if let Some(directory) = &migration.directory
            && migration_directories.insert(directory)
        {
            copy_tree(&source.join(directory), &destination.join(directory))?;
        }
    }
    if let Some(notes) = &manifest.version_metadata.release_notes {
        copy_declared_file(source, destination, Path::new(notes))?;
    }
    Ok(())
}

fn copy_declared_file(source: &Path, destination: &Path, relative: &Path) -> Result<()> {
    validate_relative_path("declared bundle file", relative)?;
    let from = ensure_safe_regular_file(source, relative, "declared bundle file", false)?;
    let to = destination.join(relative);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure!(
        !to.exists(),
        "duplicate declared bundle file: {}",
        relative.display()
    );
    fs::copy(from, to)?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure_normal_directory("asset directory", source)?;
    ensure!(!destination.exists(), "asset destination already exists");
    fs::create_dir_all(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("source tree may not contain symlinks: {}", source.display());
    }
    if metadata.file_type().is_dir() {
        copy_tree(source, destination)
    } else if metadata.file_type().is_file() {
        fs::copy(source, destination)?;
        Ok(())
    } else {
        anyhow::bail!("unsupported source file type: {}", source.display())
    }
}

fn copy_legal_files(source: &Path, destination: &Path) -> Result<()> {
    let mut copied_license = false;
    for name in ["LICENSE", "LICENSE-APACHE", "NOTICE"] {
        let path = source.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(metadata.file_type().is_file(), "legal file must be regular");
        fs::create_dir_all(destination)?;
        let target = destination.join(name);
        fs::copy(&path, &target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
        }
        copied_license |= matches!(name, "LICENSE" | "LICENSE-APACHE");
    }
    ensure!(copied_license, "source {} has no license", source.display());
    Ok(())
}

fn scan_tree(directory: &Path) -> Result<()> {
    ensure_normal_directory("directory", directory)?;
    let mut stats = TreeStats::default();
    scan_tree_inner(directory, directory, 0, RELEASE_TREE_LIMITS, &mut stats)
}

fn scan_tree_inner(
    root: &Path,
    directory: &Path,
    depth: usize,
    limits: TreeLimits,
    stats: &mut TreeStats,
) -> Result<()> {
    ensure!(
        depth <= limits.depth,
        "tree exceeds maximum depth {}",
        limits.depth
    );
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        ensure!(
            relative.as_os_str().as_encoded_bytes().len() <= limits.path_bytes,
            "tree path exceeds maximum length: {}",
            relative.display()
        );
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            anyhow::bail!("tree may not contain symlinks: {}", entry.path().display());
        }
        if kind.is_dir() {
            scan_tree_inner(root, &entry.path(), depth + 1, limits, stats)?;
        } else if kind.is_file() {
            let length = entry.metadata()?.len();
            ensure!(
                length <= limits.single_file_bytes,
                "file exceeds maximum size: {}",
                relative.display()
            );
            stats.files = stats.files.checked_add(1).context("file count overflow")?;
            stats.bytes = stats
                .bytes
                .checked_add(length)
                .context("tree size overflow")?;
            ensure!(
                stats.files <= limits.files,
                "tree exceeds maximum file count"
            );
            ensure!(
                stats.bytes <= limits.total_bytes,
                "tree exceeds maximum total size"
            );
        } else {
            anyhow::bail!(
                "tree contains unsupported file type: {}",
                relative.display()
            );
        }
    }
    Ok(())
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            anyhow::bail!("distribution may not contain symlinks");
        }
        if kind.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        } else {
            anyhow::bail!("distribution contains unsupported file type");
        }
    }
    Ok(())
}

fn cargo_build(
    source: &Path,
    package: &str,
    binary: &str,
    options: &BuildOptions,
    server_target: ServerTarget,
) -> Result<()> {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(source.join("Cargo.toml"))
        // Keep artifact discovery deterministic even when the caller exports CARGO_TARGET_DIR or
        // a source checkout contains a Cargo target-dir override.
        .arg("--target-dir")
        .arg(source.join("target"))
        .arg("--package")
        .arg(package)
        .args(["--bin", binary]);
    if options.cargo_profile == "release" {
        command.arg("--release");
    }
    command.args(["--target", server_target.rust_target()]);
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let debug = format!("{command:?}");
    let status = command.status().with_context(|| format!("run {debug}"))?;
    ensure!(status.success(), "command failed: {debug}");
    Ok(())
}

fn artifact_path(
    source: &Path,
    options: &BuildOptions,
    binary: &str,
    server_target: ServerTarget,
) -> PathBuf {
    let mut path = source.join("target");
    path.push(server_target.rust_target());
    path.push(&options.cargo_profile);
    path.push(binary);
    path
}

fn stamp_target_executable(manifest: &mut PluginManifest) -> Result<String> {
    let Execution::Process { executable, .. } = &mut manifest.execution else {
        anyhow::bail!("release module {} is not a process", manifest.id);
    };
    ensure!(
        !executable.to_ascii_lowercase().ends_with(".exe"),
        "module {} source executable must omit the Windows .exe suffix",
        manifest.id
    );
    process_executable(manifest).map(str::to_owned)
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        source.is_file(),
        "expected artifact {} is missing",
        source.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure!(
        !destination.exists(),
        "refusing to overwrite package path with executable: {}",
        destination.display()
    );
    fs::copy(source, destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_v2_rejects_legacy_union_features() {
        validate_config(&sample_config()).unwrap();
        let legacy = r#"
schema_version = 2
[distribution]
name = "unionc"
version = "0.5.0"
source = "union-rust"
revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
package = "unionc"
binary = "unionc"
base_features = ["module-photo-backup"]
[distribution.frontend]
directory = "web"
output = "dist"
"#;
        assert!(toml::from_str::<BuildConfig>(legacy).is_err());
    }

    #[test]
    fn official_profiles_are_v2_inclusion_sets() {
        for (profile, expected, expected_module_auth) in [
            (include_str!("../profiles/minimal.toml"), &[][..], &[][..]),
            (
                include_str!("../profiles/storage.toml"),
                &["dufs", "photo-backup"][..],
                &[("photo-backup", &["mobile-api", "upload-part"][..])][..],
            ),
            (
                include_str!("../profiles/monitoring.toml"),
                &["host-monitoring", "sentinel-monitor"][..],
                &[
                    (
                        "host-monitoring",
                        &[
                            "agent-activate",
                            "agent-report",
                            "pairing-create",
                            "pairing-read",
                            "pairing-status",
                        ][..],
                    ),
                    ("sentinel-monitor", &["media-hls"][..]),
                ][..],
            ),
            (
                include_str!("../profiles/full.toml"),
                &[
                    "dufs",
                    "host-monitoring",
                    "photo-backup",
                    "sentinel-monitor",
                    "sunshine",
                ][..],
                &[
                    (
                        "host-monitoring",
                        &[
                            "agent-activate",
                            "agent-report",
                            "pairing-create",
                            "pairing-read",
                            "pairing-status",
                        ][..],
                    ),
                    ("photo-backup", &["mobile-api", "upload-part"][..]),
                    ("sentinel-monitor", &["media-hls"][..]),
                ][..],
            ),
        ] {
            let config: BuildConfig = toml::from_str(profile).unwrap();
            validate_config(&config).unwrap();
            let mut actual = config
                .modules
                .iter()
                .map(|module| module.id.as_str())
                .collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, expected);
            let actual_module_auth = config
                .modules
                .iter()
                .filter(|module| !module.module_auth_routes.is_empty())
                .map(|module| {
                    let mut routes = module
                        .module_auth_routes
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>();
                    routes.sort_unstable();
                    (module.id.as_str(), routes)
                })
                .collect::<BTreeMap<_, _>>();
            let expected_module_auth = expected_module_auth
                .iter()
                .map(|(module, routes)| (*module, routes.to_vec()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(actual_module_auth, expected_module_auth);
        }
    }

    #[test]
    fn module_auth_routes_default_empty_and_reject_duplicates() {
        let rendered = toml::to_string_pretty(&sample_config()).unwrap();
        assert!(!rendered.contains("module_auth_routes"));
        let parsed: BuildConfig = toml::from_str(&rendered).unwrap();
        assert!(parsed.modules[0].module_auth_routes.is_empty());

        let mut config = sample_config();
        config.modules[0].module_auth_routes = vec!["mobile-api".into(), "mobile-api".into()];
        let error = validate_config(&config).unwrap_err().to_string();
        assert!(error.contains("duplicate module_auth_routes"));
    }

    #[test]
    fn reusable_workflow_pins_builder_checkout_to_an_explicit_commit() {
        let workflow = include_str!("../.github/workflows/build-union.yml");
        assert!(workflow.contains("builder-revision:"));
        assert!(workflow.contains("BUILDER_REVISION: ${{ inputs['builder-revision'] }}"));
        assert!(workflow.contains("CALLED_WORKFLOW_REF: ${{ job.workflow_ref }}"));
        assert!(workflow.contains("CALLED_WORKFLOW_SHA: ${{ job.workflow_sha }}"));
        assert!(workflow.contains(
            "external callers must provide builder-revision as a canonical lowercase 40-character"
        ));
        assert!(workflow.contains(
            "expected_workflow_ref=\"isarmg/union-builder/.github/workflows/build-union.yml@$BUILDER_REVISION\""
        ));
        assert!(workflow.contains("$CALLED_WORKFLOW_REF\" != \"$expected_workflow_ref"));
        assert!(workflow.contains("$CALLED_WORKFLOW_SHA\" != \"$BUILDER_REVISION"));
        assert!(
            workflow
                .contains("isarmg/union-rust callers must materialize their exact caller checkout")
        );
        assert!(workflow.contains("revision=\"$WORKFLOW_REVISION\""));
        assert!(workflow.contains("ref: ${{ steps.builder-source.outputs.revision }}"));
        assert!(
            !workflow
                .lines()
                .any(|line| line.trim_start().starts_with("ref:") && line.contains("v2.0.0")),
            "reusable workflow must not check out Builder through a movable release tag"
        );
    }

    #[test]
    fn server_distribution_workflows_are_native_linux_and_target_qualified() {
        let reusable = include_str!("../.github/workflows/build-union.yml");
        for required in [
            "server-target:",
            "linux-amd64",
            "linux-arm64",
            "ubuntu-24.04-arm",
            "ubuntu-24.04",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "artifact-name-prefix",
            "artifact_name=$ARTIFACT_NAME_PREFIX-$SERVER_TARGET",
            "archive_name=union-distribution-$SERVER_TARGET.tar",
            "--server-target \"${{ steps.server_target.outputs.server_target }}\"",
        ] {
            assert!(
                reusable.contains(required),
                "missing workflow policy: {required}"
            );
        }
        assert!(!reusable.contains("windows-"));
        assert!(!reusable.contains("macos-"));

        let release = include_str!("../.github/workflows/release.yml");
        assert!(release.contains("validate-full-bundled-distribution:"));
        assert!(release.contains("server-target: ${{ matrix.server_target }}"));
        assert!(release.contains("artifact-name-prefix: union-full-validation"));
    }

    #[test]
    fn official_caller_materialization_rewrites_only_distribution() {
        let mut config = sample_union_config();
        let included_before = config
            .modules
            .iter()
            .map(|module| module.id.clone())
            .collect::<Vec<_>>();
        let module_metadata_before = config
            .modules
            .iter()
            .map(|module| {
                (
                    module.id.clone(),
                    module.source.clone(),
                    module.revision.clone(),
                    module.package.clone(),
                    module.binary.clone(),
                    module.bundle.clone(),
                    module.repository.clone(),
                    module.module_auth_routes.clone(),
                )
            })
            .collect::<Vec<_>>();
        let distribution_identity = (
            config.distribution.name.clone(),
            config.distribution.version.clone(),
            config.distribution.package.clone(),
            config.distribution.binary.clone(),
        );
        let caller_source = Path::new("/workspaces/union-rust");
        let caller_revision = "c".repeat(40);

        let matched = materialize_config_for_caller(
            &mut config,
            OFFICIAL_UNION_REPOSITORY,
            caller_source,
            &caller_revision,
        )
        .unwrap();

        assert_eq!(matched, 1);
        assert_eq!(config.distribution.source, caller_source);
        assert_eq!(config.distribution.revision, caller_revision);
        assert_eq!(
            (
                config.distribution.name.clone(),
                config.distribution.version.clone(),
                config.distribution.package.clone(),
                config.distribution.binary.clone(),
            ),
            distribution_identity
        );
        assert_eq!(
            config
                .modules
                .iter()
                .map(|module| module.id.clone())
                .collect::<Vec<_>>(),
            included_before
        );
        assert_eq!(
            config
                .modules
                .iter()
                .map(|module| {
                    (
                        module.id.clone(),
                        module.source.clone(),
                        module.revision.clone(),
                        module.package.clone(),
                        module.binary.clone(),
                        module.bundle.clone(),
                        module.repository.clone(),
                        module.module_auth_routes.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            module_metadata_before
        );

        let rendered = toml::to_string_pretty(&config).unwrap();
        let round_trip: BuildConfig = toml::from_str(&rendered).unwrap();
        validate_config(&round_trip).unwrap();
        assert_eq!(round_trip.modules.len(), included_before.len());
    }

    #[test]
    fn caller_materialization_keeps_generic_same_repository_support() {
        let mut config = sample_union_config();
        config.modules.push(Module {
            id: "future-module".into(),
            source: "union-rust".into(),
            repository: Some(OFFICIAL_UNION_REPOSITORY.into()),
            revision: config.distribution.revision.clone(),
            package: "future-module-worker".into(),
            binary: "future-module-worker".into(),
            bundle: "future-module".into(),
            module_auth_routes: Vec::new(),
            frontend: None,
        });
        let caller_source = Path::new("/workspaces/union-rust");
        let caller_revision = "c".repeat(40);

        let matched = materialize_config_for_caller(
            &mut config,
            OFFICIAL_UNION_REPOSITORY,
            caller_source,
            &caller_revision,
        )
        .unwrap();

        assert_eq!(matched, 2);
        let module = config
            .modules
            .iter()
            .find(|module| module.id == "future-module")
            .unwrap();
        assert_eq!(module.source, caller_source);
        assert_eq!(module.revision, caller_revision);
    }

    #[test]
    fn caller_materialization_rejects_ambiguous_identity_and_invalid_inputs() {
        let checkout = Path::new("/workspaces/union-rust");
        let revision = "c".repeat(40);

        let mut config = sample_union_config();
        assert!(
            materialize_config_for_caller(
                &mut config,
                "https://example.com/isarmg/union-rust.git",
                checkout,
                &revision,
            )
            .unwrap_err()
            .to_string()
            .contains("exact official Union GitHub URL")
        );

        let mut config = sample_union_config();
        assert!(
            materialize_config_for_caller(
                &mut config,
                OFFICIAL_UNION_REPOSITORY,
                checkout,
                "main",
            )
            .unwrap_err()
            .to_string()
            .contains("lowercase 40-character")
        );

        let mut config = sample_union_config();
        config.distribution.repository =
            Some("https://github.com/isarmg/not-union-rust.git".into());
        assert!(
            materialize_config_for_caller(
                &mut config,
                OFFICIAL_UNION_REPOSITORY,
                checkout,
                &revision,
            )
            .unwrap_err()
            .to_string()
            .contains("does not exactly match")
        );

        let mut config = sample_union_config();
        assert!(
            materialize_config_for_caller(
                &mut config,
                OFFICIAL_UNION_REPOSITORY,
                Path::new("relative/union-rust"),
                &revision,
            )
            .unwrap_err()
            .to_string()
            .contains("absolute checkout path")
        );
    }

    #[test]
    fn materialized_config_is_published_atomically_without_overwrite() {
        let temporary = tempfile::tempdir().unwrap();
        let checkout = temporary.path().join("union-rust");
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("tracked"), "caller checkout\n").unwrap();
        run(Command::new("git").args(["init", "--quiet"]).arg(&checkout)).unwrap();
        run(Command::new("git")
            .args(["-C"])
            .arg(&checkout)
            .args(["add", "tracked"]))
        .unwrap();
        run(Command::new("git").args(["-C"]).arg(&checkout).args([
            "-c",
            "user.name=Union Builder Test",
            "-c",
            "user.email=builder-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]))
        .unwrap();
        let revision = capture(
            Command::new("git")
                .args(["-C"])
                .arg(&checkout)
                .args(["rev-parse", "HEAD"]),
        )
        .unwrap();
        let input = temporary.path().join("profile.toml");
        fs::write(
            &input,
            format!(
                "{}\n",
                toml::to_string_pretty(&sample_union_config()).unwrap()
            ),
        )
        .unwrap();
        let output = temporary.path().join("profile.materialized.toml");

        let result = materialize_caller_checkout(
            &input,
            OFFICIAL_UNION_REPOSITORY,
            &checkout,
            &revision,
            &output,
        )
        .unwrap();
        assert_eq!(result.matched_entries, 1);
        assert_eq!(result.output, output);
        let emitted: BuildConfig = toml::from_str(&fs::read_to_string(&output).unwrap()).unwrap();
        validate_config(&emitted).unwrap();
        assert_eq!(
            emitted.distribution.source,
            checkout.canonicalize().unwrap()
        );

        assert!(
            materialize_caller_checkout(
                &input,
                OFFICIAL_UNION_REPOSITORY,
                &checkout,
                &revision,
                &output,
            )
            .unwrap_err()
            .to_string()
            .contains("refusing to overwrite")
        );
    }

    #[test]
    fn revision_and_bundle_paths_are_strict() {
        let mut config = sample_config();
        config.distribution.revision = "main".into();
        assert!(validate_config(&config).is_err());
        config.distribution.revision = "0".repeat(40);
        assert!(validate_config(&config).is_err());
        config.distribution.revision = "a".repeat(40);
        config.modules[0].bundle = "../plugin".into();
        assert!(validate_config(&config).is_err());

        let mut config = sample_union_config();
        config.modules[1].revision = "B".repeat(40);
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("canonical lowercase")
        );

        let mut config = sample_config();
        config.distribution.repository = Some(OFFICIAL_UNION_REPOSITORY.into());
        config.modules[0].id = "future-module".into();
        config.modules[0].repository = Some(OFFICIAL_UNION_REPOSITORY.into());
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("same revision as Core/Web Shell")
        );

        let mut config = sample_config();
        config.distribution.repository = Some("https://github.com/owner/../repo.git".into());
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn server_targets_are_linux_only_and_keep_platform_neutral_executables() {
        assert_eq!(ServerTarget::LinuxAmd64.platform(), "linux");
        assert_eq!(ServerTarget::LinuxAmd64.architecture(), "amd64");
        assert_eq!(
            ServerTarget::LinuxAmd64.rust_target(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(ServerTarget::LinuxArm64.platform(), "linux");
        assert_eq!(ServerTarget::LinuxArm64.architecture(), "arm64");
        assert_eq!(
            ServerTarget::LinuxArm64.rust_target(),
            "aarch64-unknown-linux-gnu"
        );
        assert!(ServerTarget::from_release_fields("windows", "amd64").is_err());
        assert!(ServerTarget::from_release_fields("linux", "x86_64").is_err());
        let mut manifest = PluginManifest::parse_json(&sample_manifest().to_string()).unwrap();
        assert_eq!(
            stamp_target_executable(&mut manifest).unwrap(),
            "backend/photo-backup"
        );
        assert_eq!(
            process_executable(&manifest).unwrap(),
            "backend/photo-backup"
        );
    }

    #[test]
    fn module_template_and_generated_metadata_are_cross_checked() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_path_buf();
        make_plugin_template(&root);
        let mut module = sample_config().modules.remove(0);
        module.source = temporary.path().to_path_buf();
        let package = check_module_package(&module).unwrap();
        let revision = "b".repeat(40);
        let mut manifest = package.manifest;
        manifest.version_metadata.source_revision = Some(revision.clone());
        fs::write(
            root.join("manifest.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        fs::write(
            root.join("version.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&final_version_file(&manifest, &revision)).unwrap()
            ),
        )
        .unwrap();
        fs::create_dir_all(root.join("backend")).unwrap();
        fs::write(root.join("backend/photo-backup"), "binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.join("backend/photo-backup"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        validate_built_package(&root, "photo-backup", "0.1.0", &revision, &[]).unwrap();
    }

    #[test]
    fn module_backend_uses_the_canonical_single_file_layout() {
        let temporary = tempfile::tempdir().unwrap();
        make_plugin_template(temporary.path());
        let manifest_path = temporary.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest["execution"]["executable"] = "backend/nested/worker".into();
        fs::write(
            &manifest_path,
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        let mut module = sample_config().modules.remove(0);
        module.source = temporary.path().to_path_buf();
        assert!(
            check_module_package(&module)
                .unwrap_err()
                .to_string()
                .contains("backend/<executable>")
        );
    }

    #[test]
    fn module_auth_routes_require_an_exact_profile_allowlist() {
        let mut value = sample_manifest();
        value["backend"]["routes"] = serde_json::json!([{
            "id": "mobile-api",
            "path": "/v1/{*path}",
            "upstream_path": "/v1/{*path}",
            "methods": ["POST"],
            "auth": "module",
            "permission": null
        }]);
        let manifest = PluginManifest::parse_json(&value.to_string()).unwrap();
        validate_union_release_policy(&manifest, &["mobile-api".into()]).unwrap();

        assert!(
            validate_union_release_policy(&manifest, &[])
                .unwrap_err()
                .to_string()
                .contains("must exactly match")
        );
        assert!(
            validate_union_release_policy(&manifest, &["mobile-api".into(), "upload-part".into()])
                .unwrap_err()
                .to_string()
                .contains("must exactly match")
        );
    }

    #[test]
    fn module_auth_exceptions_do_not_relax_process_or_gateway_contracts() {
        let mut manifest = PluginManifest::parse_json(&sample_manifest().to_string()).unwrap();
        manifest.execution = Execution::Service {
            service: "photo-backup.api".into(),
        };
        assert!(
            validate_union_release_policy(&manifest, &[])
                .unwrap_err()
                .to_string()
                .contains("process execution")
        );

        let mut manifest = PluginManifest::parse_json(&sample_manifest().to_string()).unwrap();
        manifest.services[0].visibility = ServiceVisibility::Module;
        assert!(
            validate_union_release_policy(&manifest, &[])
                .unwrap_err()
                .to_string()
                .contains("platform visibility")
        );

        let mut value = sample_manifest();
        value["execution"]["bind"]["host"] = "0.0.0.0".into();
        assert!(PluginManifest::parse_json(&value.to_string()).is_err());
    }

    #[test]
    fn root_bundle_copy_is_manifest_driven_not_a_source_tree_copy() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("package");
        make_plugin_template(&source);
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::create_dir_all(source.join("target")).unwrap();
        fs::write(source.join("src/secret.rs"), "not packaged").unwrap();
        fs::write(source.join(".git/config"), "not packaged").unwrap();
        fs::write(source.join("target/artifact"), "not packaged").unwrap();
        let manifest = PluginManifest::parse_json(
            &read_text_bounded(&source.join("manifest.json"), "manifest").unwrap(),
        )
        .unwrap();
        copy_bundle_template(&source, &destination, &manifest, false).unwrap();
        assert!(destination.join("manifest.json").is_file());
        assert!(destination.join("frontend/entry.js").is_file());
        assert!(!destination.join("src").exists());
        assert!(!destination.join(".git").exists());
        assert!(!destination.join("target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn bundle_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_path_buf();
        make_plugin_template(&root);
        fs::write(temporary.path().join("outside"), "outside").unwrap();
        symlink(temporary.path().join("outside"), root.join("frontend/link")).unwrap();
        assert!(scan_tree(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_bundle_symlink_cannot_escape_the_source() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let outside = temporary.path().join("outside/package");
        fs::create_dir_all(&source).unwrap();
        make_plugin_template(&outside);
        symlink(temporary.path().join("outside"), source.join("redirect")).unwrap();
        let mut module = sample_config().modules.remove(0);
        module.source = source;
        module.bundle = "redirect/package".into();
        assert!(
            check_module_package(&module)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
    }

    #[test]
    fn bounded_scanner_and_checksum_parser_reject_supply_chain_abuse() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("oversized"), "12345").unwrap();
        let limits = TreeLimits {
            files: 10,
            single_file_bytes: 4,
            total_bytes: 10,
            path_bytes: 64,
            depth: 4,
        };
        assert!(
            scan_tree_inner(
                temporary.path(),
                temporary.path(),
                0,
                limits,
                &mut TreeStats::default(),
            )
            .unwrap_err()
            .to_string()
            .contains("maximum size")
        );
        assert!(
            parse_checksums(&format!("{}  ../escape\n", "a".repeat(64)))
                .unwrap_err()
                .to_string()
                .contains("safe relative")
        );
        assert!(
            parse_checksums(&format!(
                "{}  second\n{}  first\n",
                "a".repeat(64),
                "b".repeat(64)
            ))
            .unwrap_err()
            .to_string()
            .contains("strictly sorted")
        );
    }

    #[cfg(unix)]
    #[test]
    fn frontend_build_is_constrained() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let web = temporary.path().join("web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), "{}").unwrap();
        fs::write(web.join("package-lock.json"), "{}").unwrap();
        let npm = temporary.path().join("npm-test");
        fs::write(
            &npm,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> invocations\nif [ \"$*\" = 'run build' ]; then mkdir -p dist; printf built > dist/entry.js; fi\n",
        )
        .unwrap();
        fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
        npm_build_with(
            temporary.path(),
            &Frontend {
                directory: "web".into(),
                output: "dist".into(),
            },
            &npm,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(web.join("invocations")).unwrap(),
            "ci\nrun build\n"
        );
    }

    #[test]
    fn verification_rejects_unlisted_or_changed_files() {
        let temporary = tempfile::tempdir().unwrap();
        let release = temporary.path().join("release");
        make_fake_release(&release, "0.5.0", "first");
        let verified = verify_release(&release).unwrap();
        assert_eq!(verified.files, 3);
        assert_eq!(verified.server_target, ServerTarget::LinuxAmd64);
        assert!(verify_release_for_target(&release, ServerTarget::LinuxArm64).is_err());
        fs::remove_file(release.join("share/union/web/index.html")).unwrap();
        write_checksums(&release, &release.join("SHA256SUMS")).unwrap();
        assert!(
            verify_release(&release)
                .unwrap_err()
                .to_string()
                .contains("index.html")
        );
        fs::write(release.join("share/union/web/index.html"), "shell").unwrap();
        write_checksums(&release, &release.join("SHA256SUMS")).unwrap();
        let checksum_path = release.join("SHA256SUMS");
        let mut checksums = fs::read_to_string(&checksum_path).unwrap();
        checksums.push_str(&format!("{}  ghost\n", "a".repeat(64)));
        fs::write(&checksum_path, checksums).unwrap();
        assert!(verify_release(&release).is_err());
        write_checksums(&release, &checksum_path).unwrap();
        fs::write(release.join("unexpected"), "x").unwrap();
        assert!(verify_release(&release).is_err());
        fs::remove_file(release.join("unexpected")).unwrap();
        fs::write(release.join("bin/unionc"), "changed").unwrap();
        assert!(verify_release(&release).is_err());
    }

    #[test]
    fn verification_rejects_non_linux_server_distribution_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let release = temporary.path().join("release");
        make_fake_release(&release, "0.5.0", "first");
        let manifest_path = release.join("union-release.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        manifest["distribution"]["platform"] = "windows".into();
        fs::write(
            &manifest_path,
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        write_checksums(&release, &release.join("SHA256SUMS")).unwrap();
        assert!(
            verify_release(&release)
                .unwrap_err()
                .to_string()
                .contains("supported targets are linux/amd64 and linux/arm64")
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_and_rollback_switch_complete_releases() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        let root = temporary.path().join("install");
        make_fake_release(&first, "0.5.0", "first");
        make_fake_release(&second, "0.5.1", "second");
        let a = install_release(&first, &root).unwrap();
        let b = install_release(&second, &root).unwrap();
        assert_eq!(b.previous_release_id, Some(a.release_id.clone()));
        assert_eq!(rollback_install(&root).unwrap().release_id, a.release_id);
    }

    fn make_fake_release(path: &Path, version: &str, executable: &str) {
        fs::create_dir_all(path.join("bin")).unwrap();
        fs::create_dir_all(path.join("modules")).unwrap();
        fs::create_dir_all(path.join("share/union/web")).unwrap();
        fs::write(path.join("bin/unionc"), executable).unwrap();
        fs::write(path.join("share/union/web/index.html"), "shell").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.join("bin/unionc"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        fs::write(
            path.join("union-release.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema_version": 2,
                    "distribution": {
                        "name": "unionc", "version": version,
                        "revision": "a".repeat(40),
                        "platform": "linux", "architecture": "amd64",
                        "executable": "bin/unionc",
                        "web_shell": "share/union/web"
                    },
                    "modules": [], "activation_order": []
                }))
                .unwrap()
            ),
        )
        .unwrap();
        write_checksums(path, &path.join("SHA256SUMS")).unwrap();
    }

    fn make_plugin_template(root: &Path) {
        fs::create_dir_all(root.join("frontend")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(
            root.join("manifest.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&sample_manifest()).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            root.join("permissions.json"),
            "[{\"id\":\"photo-backup.read\",\"description\":\"Read photos\",\"default_roles\":[\"admin\"]}]\n",
        )
        .unwrap();
        fs::write(
            root.join("version.json"),
            "{\"manifest_version\":1,\"id\":\"photo-backup\",\"version\":\"0.1.0\",\"channel\":\"stable\",\"distribution\":\"bundled\",\"license\":\"Apache-2.0\",\"compatibility\":{\"core\":\">=0.5.0, <0.6.0\",\"platform_api\":\"^1.0.0\",\"plugin_api\":\"^1.0.0\"}}\n",
        )
        .unwrap();
        fs::write(root.join("config/schema.json"), "{\"type\":\"object\"}\n").unwrap();
        fs::write(root.join("frontend/entry.js"), "export default {};\n").unwrap();
    }

    fn sample_manifest() -> serde_json::Value {
        serde_json::json!({
            "manifest_version": 1, "id": "photo-backup", "display_name": "Photo Backup",
            "description": "Photo backup module", "version": "0.1.0",
            "version_metadata": {"channel":"stable","distribution":"bundled","license":"Apache-2.0"},
            "compatibility": {"core":">=0.5.0, <0.6.0","platform_api":"^1.0.0","plugin_api":"^1.0.0"},
            "dependencies": [],
            "execution": {"mode":"process","executable":"backend/photo-backup","args":[],"environment":[],"bind":{"host":"127.0.0.1","port":0}},
            "backend": {"api_version":"v1","base_path":"/api/modules/photo-backup","service":"photo-backup.api","routes":[]},
            "frontend": {"entry":"frontend/entry.js","styles":[],"components":["PhotoOverview"],"api_base":"/api/modules/photo-backup","routes":[{"path":"/modules/photo-backup","component":"PhotoOverview","permission":"photo-backup.read"}],"menu":[{"id":"overview","label":"Photos","route":"/modules/photo-backup","permission":"photo-backup.read","order":100}]},
            "permissions": [{"id":"photo-backup.read","description":"Read photos","default_roles":["admin"]}],
            "migrations": [],
            "configuration": {"schema":"config/schema.json","version":1,"secret_fields":[]},
            "health": {"kind":"http","service":"photo-backup.api","liveness_path":"/health/live","readiness_path":"/health/ready","interval_seconds":10,"timeout_seconds":2},
            "lifecycle": {"startup_timeout_seconds":30,"shutdown_timeout_seconds":30,"restart_policy":"on_failure","max_restarts":5},
            "services": [{"name":"photo-backup.api","protocol":"http","visibility":"platform"}],
            "events": {"publishes":[],"subscribes":[]}
        })
    }

    fn sample_config() -> BuildConfig {
        BuildConfig {
            schema_version: 2,
            require_clean_sources: false,
            distribution: Distribution {
                name: "unionc".into(),
                version: "0.5.0".into(),
                source: "union-rust".into(),
                repository: None,
                revision: "a".repeat(40),
                package: "unionc".into(),
                binary: "unionc".into(),
                frontend: Frontend {
                    directory: "web".into(),
                    output: "dist".into(),
                },
                output: "dist".into(),
            },
            modules: vec![Module {
                id: "photo-backup".into(),
                source: "photo-backup".into(),
                repository: None,
                revision: "b".repeat(40),
                package: "photo-backup-server".into(),
                binary: "photo-backup-server".into(),
                bundle: ".".into(),
                module_auth_routes: Vec::new(),
                frontend: None,
            }],
        }
    }

    fn sample_union_config() -> BuildConfig {
        let mut config = sample_config();
        config.distribution.repository = Some(OFFICIAL_UNION_REPOSITORY.into());
        config.modules[0].repository = Some("https://github.com/isarmg/photo-backup.git".into());
        for (id, source, repository, revision, package, binary, bundle) in [
            (
                "sunshine",
                "sunshine-worker",
                "https://github.com/isarmg/sunshine-worker.git",
                "c".repeat(40),
                "unionc-sunshine-worker",
                "unionc-sunshine-worker",
                ".",
            ),
            (
                "host-monitoring",
                "host-monitoring",
                "https://github.com/isarmg/host-monitoring.git",
                "d".repeat(40),
                "union-host-monitoring-worker",
                "union-host-monitoring-worker",
                "host-monitoring-worker",
            ),
        ] {
            config.modules.push(Module {
                id: id.into(),
                source: source.into(),
                repository: Some(repository.into()),
                revision,
                package: package.into(),
                binary: binary.into(),
                bundle: bundle.into(),
                module_auth_routes: Vec::new(),
                frontend: None,
            });
        }
        config
    }
}
