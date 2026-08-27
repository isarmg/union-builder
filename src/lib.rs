use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    pub schema_version: u32,
    #[serde(default)]
    pub require_clean_sources: bool,
    pub distribution: Distribution,
    #[serde(default)]
    #[serde(rename = "module")]
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    pub repository: Option<String>,
    pub revision: String,
    pub package: String,
    pub binary: String,
    #[serde(default)]
    pub base_features: Vec<String>,
    pub frontend: Option<Frontend>,
    #[serde(default = "default_output")]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub id: String,
    pub source: PathBuf,
    pub repository: Option<String>,
    pub revision: String,
    pub package: String,
    pub binary: String,
    pub union_feature: String,
    #[serde(default)]
    pub cargo_features: Vec<String>,
    pub frontend: Option<Frontend>,
    pub runtime: Runtime,
}

/// A constrained Node/Vite build. The tool deliberately does not accept arbitrary shell commands:
/// every frontend runs exactly `npm ci` followed by `npm run build` in the declared directory.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frontend {
    pub directory: PathBuf,
    pub output: PathBuf,
    pub install: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub bind: SocketAddr,
    pub gateway_path: String,
    pub liveness_path: String,
    pub readiness_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CheckedConfig {
    pub config: BuildConfig,
    pub config_dir: PathBuf,
    pub distribution_source: SourceIdentity,
    pub module_sources: Vec<SourceIdentity>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceIdentity {
    pub path: PathBuf,
    pub revision: String,
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub profile: String,
    pub target: Option<String>,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub output: PathBuf,
    pub manifest: PathBuf,
    pub checksums: PathBuf,
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
}

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Serialize)]
struct Plan<'a> {
    distribution: PlanTarget<'a>,
    modules: Vec<PlanModule<'a>>,
}

#[derive(Debug, Serialize)]
struct PlanTarget<'a> {
    name: &'a str,
    version: &'a str,
    source: &'a Path,
    revision: &'a str,
    package: &'a str,
    binary: &'a str,
    features: Vec<&'a str>,
    frontend: Option<&'a Frontend>,
}

#[derive(Debug, Serialize)]
struct PlanModule<'a> {
    id: &'a str,
    source: &'a Path,
    revision: &'a str,
    package: &'a str,
    binary: &'a str,
    union_feature: &'a str,
    cargo_features: Vec<&'a str>,
    frontend: Option<&'a Frontend>,
    install_path: String,
    runtime: &'a Runtime,
}

#[derive(Debug, Serialize)]
struct ReleaseManifest<'a> {
    schema_version: u32,
    distribution: ReleaseDistribution<'a>,
    modules: Vec<ReleaseModule<'a>>,
}

#[derive(Debug, Serialize)]
struct ReleaseDistribution<'a> {
    name: &'a str,
    version: &'a str,
    revision: &'a str,
    executable: String,
    frontend: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReleaseModule<'a> {
    id: &'a str,
    revision: &'a str,
    executable: String,
    frontend: Option<String>,
    runtime: &'a Runtime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseManifest {
    schema_version: u32,
    distribution: StoredReleaseDistribution,
    modules: Vec<StoredReleaseModule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseDistribution {
    name: String,
    version: String,
    revision: String,
    executable: String,
    frontend: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReleaseModule {
    id: String,
    revision: String,
    executable: String,
    frontend: Option<String>,
    runtime: Runtime,
}

fn default_output() -> PathBuf {
    PathBuf::from("dist")
}

pub fn load_and_check(config_path: &Path) -> Result<CheckedConfig> {
    let config_path = absolute(config_path)?;
    let config_dir = config_path
        .parent()
        .context("build config has no parent directory")?
        .to_path_buf();
    let raw = fs::read_to_string(&config_path)
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

    Ok(CheckedConfig {
        config,
        config_dir,
        distribution_source,
        module_sources,
    })
}

fn validate_config(config: &BuildConfig) -> Result<()> {
    ensure!(config.schema_version == 1, "schema_version must be 1");
    validate_name("distribution name", &config.distribution.name)?;
    validate_name("distribution package", &config.distribution.package)?;
    validate_name("distribution binary", &config.distribution.binary)?;
    validate_version(&config.distribution.version)?;
    validate_revision(&config.distribution.revision)?;
    if let Some(repository) = &config.distribution.repository {
        validate_repository(repository)?;
    }
    let mut all_features = BTreeSet::new();
    for feature in &config.distribution.base_features {
        validate_name("distribution feature", feature)?;
        ensure!(
            all_features.insert(feature),
            "duplicate Union feature: {feature}"
        );
    }
    let mut frontend_installs = Vec::new();
    if let Some(frontend) = &config.distribution.frontend {
        validate_frontend("distribution frontend", frontend)?;
        frontend_installs.push(frontend.install.clone());
    }
    let mut ids = BTreeSet::new();
    let mut features = BTreeSet::new();
    let mut binds = BTreeSet::new();
    let mut gateways = BTreeSet::new();
    for module in &config.modules {
        validate_id(&module.id)?;
        ensure!(
            ids.insert(module.id.as_str()),
            "duplicate module id: {}",
            module.id
        );
        validate_name("module package", &module.package)?;
        validate_name("module binary", &module.binary)?;
        validate_name("Union feature", &module.union_feature)?;
        ensure!(
            features.insert(module.union_feature.as_str())
                && all_features.insert(&module.union_feature),
            "duplicate Union feature: {}",
            module.union_feature
        );
        let mut cargo_features = BTreeSet::new();
        for feature in &module.cargo_features {
            validate_name("module cargo feature", feature)?;
            ensure!(
                cargo_features.insert(feature),
                "duplicate cargo feature for module {}: {feature}",
                module.id
            );
        }
        validate_revision(&module.revision)?;
        if let Some(repository) = &module.repository {
            validate_repository(repository)?;
        }
        if let Some(frontend) = &module.frontend {
            validate_frontend(&format!("module {} frontend", module.id), frontend)?;
            for existing in &frontend_installs {
                ensure!(
                    !paths_overlap(existing, &frontend.install),
                    "overlapping frontend install paths: {} and {}",
                    existing.display(),
                    frontend.install.display()
                );
            }
            frontend_installs.push(frontend.install.clone());
        }
        ensure!(
            module.runtime.bind.ip().is_loopback(),
            "module {} must bind to a loopback address, found {}",
            module.id,
            module.runtime.bind
        );
        ensure!(
            binds.insert(module.runtime.bind),
            "duplicate module bind address: {}",
            module.runtime.bind
        );
        validate_path("gateway_path", &module.runtime.gateway_path)?;
        validate_path("liveness_path", &module.runtime.liveness_path)?;
        if let Some(path) = &module.runtime.readiness_path {
            validate_path("readiness_path", path)?;
        }
        ensure!(
            gateways.insert(module.runtime.gateway_path.as_str()),
            "duplicate module gateway path: {}",
            module.runtime.gateway_path
        );
    }
    Ok(())
}

fn validate_frontend(label: &str, frontend: &Frontend) -> Result<()> {
    validate_relative_path(&format!("{label} directory"), &frontend.directory)?;
    validate_relative_path(&format!("{label} output"), &frontend.output)?;
    validate_relative_path(&format!("{label} install"), &frontend.install)?;
    ensure!(
        frontend.install.starts_with("share/union") && frontend.install != Path::new("share/union"),
        "{label} install must be a child of share/union"
    );
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
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
        value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit()),
        "revision must be a full 40-character Git object id: {value}"
    );
    Ok(())
}

fn validate_path(label: &str, value: &str) -> Result<()> {
    ensure!(
        value.starts_with('/')
            && !value.starts_with("//")
            && !value.contains(['?', '#'])
            && !value.split('/').any(|part| matches!(part, "." | "..")),
        "{label} must be a safe absolute URL path: {value}"
    );
    Ok(())
}

fn validate_repository(value: &str) -> Result<()> {
    ensure!(
        value.starts_with("https://github.com/")
            && value.ends_with(".git")
            && !value.contains(['?', '#', '@']),
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

pub fn render_plan(checked: &CheckedConfig, format: OutputFormat) -> Result<String> {
    let plan = make_plan(checked);
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(&plan)?),
        OutputFormat::Text => {
            let mut lines = vec![format!(
                "distribution {} {}: cargo build -p {} --features {}",
                plan.distribution.name,
                plan.distribution.version,
                plan.distribution.package,
                plan.distribution.features.join(",")
            )];
            for module in plan.modules {
                lines.push(format!(
                    "module {}: cargo build -p {} --bin {} -> {} ({})",
                    module.id,
                    module.package,
                    module.binary,
                    module.install_path,
                    module.runtime.bind
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

fn make_plan(checked: &CheckedConfig) -> Plan<'_> {
    let mut features = checked
        .config
        .distribution
        .base_features
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    features.extend(
        checked
            .config
            .modules
            .iter()
            .map(|module| module.union_feature.as_str()),
    );
    Plan {
        distribution: PlanTarget {
            name: &checked.config.distribution.name,
            version: &checked.config.distribution.version,
            source: &checked.distribution_source.path,
            revision: &checked.distribution_source.revision,
            package: &checked.config.distribution.package,
            binary: &checked.config.distribution.binary,
            features,
            frontend: checked.config.distribution.frontend.as_ref(),
        },
        modules: checked
            .config
            .modules
            .iter()
            .zip(&checked.module_sources)
            .map(|(module, source)| PlanModule {
                id: &module.id,
                source: &source.path,
                revision: &source.revision,
                package: &module.package,
                binary: &module.binary,
                union_feature: &module.union_feature,
                cargo_features: module.cargo_features.iter().map(String::as_str).collect(),
                frontend: module.frontend.as_ref(),
                install_path: format!("libexec/union/modules/{}", module.id),
                runtime: &module.runtime,
            })
            .collect(),
    }
}

pub fn build(config_path: &Path, options: BuildOptions) -> Result<BuildResult> {
    ensure!(
        options.profile == "release" || options.profile == "debug",
        "profile must be release or debug"
    );
    if let Some(target) = &options.target {
        validate_name("target", target)?;
    }
    let checked = load_and_check(config_path)?;
    let output = options
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
        !output.exists(),
        "output {} already exists; refusing to overwrite it",
        output.display()
    );

    let plan = make_plan(&checked);
    if let Some(frontend) = &checked.config.distribution.frontend {
        let contains = |id: &str| checked.config.modules.iter().any(|module| module.id == id);
        let environment = [
            (
                "UNIONC_WEB_MODULE_SUNSHINE",
                if contains("sunshine") {
                    "true"
                } else {
                    "false"
                },
            ),
            (
                "UNIONC_WEB_MODULE_HOST_MONITORING",
                if contains("host-monitoring") {
                    "true"
                } else {
                    "false"
                },
            ),
        ];
        npm_build(&checked.config.distribution.source, frontend, &environment)?;
    }
    for module in &checked.config.modules {
        if let Some(frontend) = &module.frontend {
            npm_build(&module.source, frontend, &[])
                .with_context(|| format!("build module {} frontend", module.id))?;
        }
    }
    cargo_build(
        &checked.config.distribution.source,
        &checked.config.distribution.package,
        Some(&checked.config.distribution.binary),
        &plan.distribution.features,
        true,
        &options,
    )?;
    for module in &checked.config.modules {
        let features = module
            .cargo_features
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        cargo_build(
            &module.source,
            &module.package,
            Some(&module.binary),
            &features,
            true,
            &options,
        )?;
    }

    let bin_dir = output.join("bin");
    let modules_dir = output.join("libexec/union/modules");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&modules_dir)?;
    copy_legal_files(
        &checked.config.distribution.source,
        &output.join("share/licenses/unionc"),
    )?;
    let suffix = executable_suffix(options.target.as_deref());
    let distribution_artifact = artifact_path(
        &checked.config.distribution.source,
        &options,
        &checked.config.distribution.binary,
        suffix,
    );
    let distribution_install =
        bin_dir.join(format!("{}{}", checked.config.distribution.binary, suffix));
    copy_executable(&distribution_artifact, &distribution_install)?;
    let distribution_frontend = checked
        .config
        .distribution
        .frontend
        .as_ref()
        .map(|frontend| {
            let source = checked
                .config
                .distribution
                .source
                .join(&frontend.directory)
                .join(&frontend.output);
            let destination = output.join(&frontend.install);
            copy_tree(&source, &destination)?;
            relative(&output, &destination)
        })
        .transpose()?;
    for module in &checked.config.modules {
        let artifact = artifact_path(&module.source, &options, &module.binary, suffix);
        let install = modules_dir.join(format!("{}{}", module.id, suffix));
        copy_executable(&artifact, &install)?;
        copy_legal_files(
            &module.source,
            &output.join("share/licenses/modules").join(&module.id),
        )?;
        if let Some(frontend) = &module.frontend {
            copy_tree(
                &module
                    .source
                    .join(&frontend.directory)
                    .join(&frontend.output),
                &output.join(&frontend.install),
            )?;
        }
    }

    let manifest_path = output.join("union-release.json");
    let manifest = ReleaseManifest {
        schema_version: 1,
        distribution: ReleaseDistribution {
            name: &checked.config.distribution.name,
            version: &checked.config.distribution.version,
            revision: &checked.distribution_source.revision,
            executable: relative(&output, &distribution_install)?,
            frontend: distribution_frontend,
        },
        modules: checked
            .config
            .modules
            .iter()
            .zip(&checked.module_sources)
            .map(|(module, source)| {
                let install = modules_dir.join(format!("{}{}", module.id, suffix));
                Ok(ReleaseModule {
                    id: &module.id,
                    revision: &source.revision,
                    executable: relative(&output, &install)?,
                    frontend: module
                        .frontend
                        .as_ref()
                        .map(|frontend| relative(&output, &output.join(&frontend.install)))
                        .transpose()?,
                    runtime: &module.runtime,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;

    let checksums_path = output.join("SHA256SUMS");
    let mut checksum_files = Vec::new();
    collect_files(&output, &mut checksum_files)?;
    checksum_files.retain(|path| path != &checksums_path);
    checksum_files.sort();
    let mut checksum_text = String::new();
    for path in checksum_files {
        checksum_text.push_str(&format!(
            "{}  {}\n",
            sha256(&path)?,
            relative(&output, &path)?
        ));
    }
    fs::write(&checksums_path, checksum_text)?;

    Ok(BuildResult {
        output,
        manifest: manifest_path,
        checksums: checksums_path,
    })
}

/// Verify that a release directory is complete, internally consistent and byte-for-byte equal to
/// its SHA256SUMS inventory. Symlinks and unlisted files are rejected.
pub fn verify_release(release: &Path) -> Result<VerificationResult> {
    ensure_normal_directory("release", release)?;
    let manifest_path = release.join("union-release.json");
    let manifest: StoredReleaseManifest = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    validate_stored_manifest(release, &manifest)?;

    let checksums_path = release.join("SHA256SUMS");
    let checksum_text = fs::read_to_string(&checksums_path)
        .with_context(|| format!("read {}", checksums_path.display()))?;
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
        let actual = sha256(&release.join(path))?;
        ensure!(actual == *digest, "checksum mismatch for {path}");
    }

    let release_id = release_id(&manifest, &checksum_text)?;
    Ok(VerificationResult {
        release: release.to_path_buf(),
        release_id,
        files: expected.len(),
    })
}

/// Copy a verified release into an immutable slot below `<root>/releases`.
/// Existing slots are verified and reused; they are never overwritten.
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
        fs::rename(&payload, &destination).with_context(|| {
            format!(
                "publish staged release as immutable slot {}",
                destination.display()
            )
        })?;
    }
    Ok(InstallResult {
        release_id: verified.release_id,
        release: destination,
        previous_release_id: read_pointer(root, "current")?,
    })
}

/// Stage and atomically activate a release. On Unix, `current` and `previous` are relative
/// symlinks, so moving the complete install root keeps the installation valid.
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

/// Atomically reactivate `previous`, then retain the displaced release as the next rollback target.
/// Release directories are never changed, so rollback needs no source repository or network access.
pub fn rollback_install(root: &Path) -> Result<InstallResult> {
    ensure_normal_directory("install root", root)?;
    let current = read_pointer(root, "current")?.context("no active Union release")?;
    let previous = read_pointer(root, "previous")?.context("no previous Union release")?;
    let destination = root.join("releases").join(&previous);
    let verified = verify_release(&destination)?;
    ensure!(
        verified.release_id == previous,
        "previous release slot does not match its id"
    );
    switch_pointer(root, "current", &previous)?;
    switch_pointer(root, "previous", &current)?;
    Ok(InstallResult {
        release_id: previous,
        release: destination,
        previous_release_id: Some(current),
    })
}

fn validate_stored_manifest(release: &Path, manifest: &StoredReleaseManifest) -> Result<()> {
    ensure!(manifest.schema_version == 1, "unsupported release schema");
    validate_name("release distribution name", &manifest.distribution.name)?;
    validate_version(&manifest.distribution.version)?;
    validate_revision(&manifest.distribution.revision)?;
    validate_release_file(
        release,
        "distribution executable",
        &manifest.distribution.executable,
    )?;
    ensure!(
        Path::new(&manifest.distribution.executable).starts_with("bin"),
        "distribution executable must be below bin"
    );
    if let Some(frontend) = &manifest.distribution.frontend {
        validate_release_directory(release, "distribution frontend", frontend)?;
    }
    let mut ids = BTreeSet::new();
    let mut executables = BTreeSet::new();
    let mut binds = BTreeSet::new();
    let mut gateways = BTreeSet::new();
    for module in &manifest.modules {
        validate_id(&module.id)?;
        ensure!(
            ids.insert(&module.id),
            "duplicate release module id: {}",
            module.id
        );
        validate_revision(&module.revision)?;
        validate_release_file(release, "module executable", &module.executable)?;
        ensure!(
            executables.insert(&module.executable),
            "duplicate module executable: {}",
            module.executable
        );
        ensure!(
            Path::new(&module.executable).starts_with("libexec/union/modules"),
            "module executable must be below libexec/union/modules"
        );
        if let Some(frontend) = &module.frontend {
            validate_release_directory(release, "module frontend", frontend)?;
        }
        ensure!(
            module.runtime.bind.ip().is_loopback(),
            "module bind must be loopback"
        );
        ensure!(
            binds.insert(module.runtime.bind),
            "duplicate module bind: {}",
            module.runtime.bind
        );
        validate_path("gateway_path", &module.runtime.gateway_path)?;
        ensure!(
            gateways.insert(&module.runtime.gateway_path),
            "duplicate gateway path: {}",
            module.runtime.gateway_path
        );
        validate_path("liveness_path", &module.runtime.liveness_path)?;
        if let Some(path) = &module.runtime.readiness_path {
            validate_path("readiness_path", path)?;
        }
    }
    Ok(())
}

fn validate_release_file(release: &Path, label: &str, value: &str) -> Result<()> {
    let path = Path::new(value);
    validate_relative_path(label, path)?;
    let absolute = release.join(path);
    let metadata = fs::symlink_metadata(&absolute)
        .with_context(|| format!("{label} {} is missing", absolute.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "{label} must be executable"
        );
    }
    Ok(())
}

fn validate_release_directory(release: &Path, label: &str, value: &str) -> Result<()> {
    let path = Path::new(value);
    validate_relative_path(label, path)?;
    ensure!(
        path.starts_with("share/union"),
        "{label} must be below share/union"
    );
    ensure_normal_directory(label, &release.join(path))
}

fn parse_checksums(value: &str) -> Result<BTreeMap<String, String>> {
    let mut checksums = BTreeMap::new();
    ensure!(!value.is_empty(), "SHA256SUMS is empty");
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

fn ensure_install_root(root: &Path) -> Result<()> {
    if root.exists() {
        ensure_normal_directory("install root", root)
    } else {
        fs::create_dir_all(root)
            .with_context(|| format!("create install root {}", root.display()))?;
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

fn npm_build(source: &Path, frontend: &Frontend, environment: &[(&str, &str)]) -> Result<()> {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    npm_build_with(source, frontend, Path::new(npm), environment)
}

fn npm_build_with(
    source: &Path,
    frontend: &Frontend,
    npm: &Path,
    environment: &[(&str, &str)],
) -> Result<()> {
    let directory = source.join(&frontend.directory);
    ensure!(
        directory.join("package.json").is_file() && directory.join("package-lock.json").is_file(),
        "frontend {} must contain package.json and package-lock.json",
        directory.display()
    );
    run(Command::new(npm).current_dir(&directory).args(["ci"]))?;
    let mut build = Command::new(npm);
    build.current_dir(&directory).args(["run", "build"]);
    for (name, value) in environment {
        build.env(name, value);
    }
    run(&mut build)?;
    let generated = directory.join(&frontend.output);
    ensure!(
        generated.is_dir(),
        "frontend build did not create {}",
        generated.display()
    );
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        source.is_dir(),
        "asset directory {} is missing",
        source.display()
    );
    ensure!(
        !destination.exists(),
        "asset destination {} already exists",
        destination.display()
    );
    fs::create_dir_all(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_symlink() {
            anyhow::bail!(
                "frontend output may not contain symlinks: {}",
                entry.path().display()
            );
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        } else {
            anyhow::bail!(
                "frontend output contains an unsupported file type: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
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
        ensure!(
            metadata.file_type().is_file(),
            "legal file must be a non-symlink regular file: {}",
            path.display()
        );
        fs::create_dir_all(destination)?;
        let target = destination.join(name);
        fs::copy(&path, &target).with_context(|| {
            format!("copy legal file {} to {}", path.display(), target.display())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
        }
        copied_license |= matches!(name, "LICENSE" | "LICENSE-APACHE");
    }
    ensure!(
        copied_license,
        "source {} must provide LICENSE or LICENSE-APACHE",
        source.display()
    );
    Ok(())
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            anyhow::bail!(
                "distribution may not contain symlinks: {}",
                entry.path().display()
            );
        }
        if kind.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        } else {
            anyhow::bail!(
                "distribution contains an unsupported file type: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn cargo_build(
    source: &Path,
    package: &str,
    binary: Option<&str>,
    features: &[&str],
    no_default_features: bool,
    options: &BuildOptions,
) -> Result<()> {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(source.join("Cargo.toml"))
        .arg("--package")
        .arg(package);
    if let Some(binary) = binary {
        command.args(["--bin", binary]);
    }
    if options.profile == "release" {
        command.arg("--release");
    }
    if let Some(target) = &options.target {
        command.args(["--target", target]);
    }
    if no_default_features {
        command.arg("--no-default-features");
    }
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let debug = format!("{command:?}");
    let status = command.status().with_context(|| format!("run {debug}"))?;
    ensure!(status.success(), "command failed: {debug}");
    Ok(())
}

fn artifact_path(source: &Path, options: &BuildOptions, binary: &str, suffix: &str) -> PathBuf {
    let mut path = source.join("target");
    if let Some(target) = &options.target {
        path.push(target);
    }
    path.push(&options.profile);
    path.push(format!("{binary}{suffix}"));
    path
}

fn executable_suffix(target: Option<&str>) -> &'static str {
    match target {
        Some(target) if target.contains("-windows-") => ".exe",
        Some(_) => "",
        None => std::env::consts::EXE_SUFFIX,
    }
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        source.is_file(),
        "expected build artifact {} is missing",
        source.display()
    );
    fs::copy(source, destination)
        .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
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
    fn module_runtime_must_be_private_and_unique() {
        let mut config = sample_config();
        config.modules[0].runtime.bind = "0.0.0.0:18081".parse().unwrap();
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );

        config.modules[0].runtime.bind = "127.0.0.1:18081".parse().unwrap();
        config.modules.push(config.modules[0].clone());
        config.modules[1].id = "other".into();
        config.modules[1].union_feature = "module-other".into();
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("bind")
        );
    }

    #[test]
    fn revisions_and_paths_are_strict() {
        let mut config = sample_config();
        config.distribution.revision = "main".into();
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("revision")
        );
        config.distribution.revision = "a".repeat(40);
        config.modules[0].runtime.gateway_path = "https://example.test/".into();
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("gateway_path")
        );
    }

    #[test]
    fn executable_suffix_follows_the_target_not_the_host() {
        assert_eq!(executable_suffix(Some("x86_64-pc-windows-msvc")), ".exe");
        assert_eq!(executable_suffix(Some("x86_64-unknown-linux-musl")), "");
        assert_eq!(executable_suffix(Some("aarch64-apple-darwin")), "");
        assert_eq!(executable_suffix(None), std::env::consts::EXE_SUFFIX);
    }

    #[test]
    fn a_core_only_distribution_is_valid() {
        let mut config = sample_config();
        config.modules.clear();
        validate_config(&config).unwrap();
    }

    #[test]
    fn frontend_paths_are_safe_and_non_overlapping() {
        let mut config = sample_config();
        config.distribution.frontend = Some(Frontend {
            directory: "web".into(),
            output: "dist".into(),
            install: "share/union/web".into(),
        });
        config.modules[0].frontend = Some(Frontend {
            directory: "web".into(),
            output: "dist".into(),
            install: "share/union/web/photo".into(),
        });
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("overlapping")
        );

        config.modules[0].frontend.as_mut().unwrap().install = "../outside".into();
        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("safe relative")
        );
    }

    #[test]
    fn official_profiles_are_valid_and_have_expected_graphs() {
        let profiles = [
            (include_str!("../profiles/minimal.toml"), 0),
            (include_str!("../profiles/storage.toml"), 2),
            (include_str!("../profiles/monitoring.toml"), 2),
            (include_str!("../profiles/full.toml"), 5),
        ];
        for (profile, module_count) in profiles {
            let config: BuildConfig = toml::from_str(profile).unwrap();
            validate_config(&config).unwrap();
            assert_eq!(config.modules.len(), module_count);
            assert!(config.distribution.frontend.is_some());
        }
    }

    #[test]
    fn frontend_tree_is_copied_recursively() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(source.join("assets")).unwrap();
        fs::write(source.join("index.html"), "index").unwrap();
        fs::write(source.join("assets/app.js"), "app").unwrap();
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("index.html")).unwrap(),
            "index"
        );
        assert_eq!(
            fs::read_to_string(destination.join("assets/app.js")).unwrap(),
            "app"
        );
    }

    #[test]
    fn legal_files_are_mandatory_and_notice_is_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("legal");
        fs::create_dir_all(&source).unwrap();
        assert!(copy_legal_files(&source, &destination).is_err());
        fs::write(source.join("LICENSE-APACHE"), "Apache-2.0").unwrap();
        fs::write(source.join("NOTICE"), "notices").unwrap();
        copy_legal_files(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("LICENSE-APACHE")).unwrap(),
            "Apache-2.0"
        );
        assert_eq!(
            fs::read_to_string(destination.join("NOTICE")).unwrap(),
            "notices"
        );
    }

    #[cfg(unix)]
    #[test]
    fn frontend_build_runs_only_locked_install_then_build() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let web = source.join("web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), "{}").unwrap();
        fs::write(web.join("package-lock.json"), "{}").unwrap();
        let npm = temporary.path().join("npm-test");
        fs::write(
            &npm,
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> invocations\nif [ \"$*\" = 'run build' ]; then printf '%s,%s' \"${UNIONC_WEB_MODULE_SUNSHINE-unset}\" \"${UNIONC_WEB_MODULE_HOST_MONITORING-unset}\" > build-environment; mkdir -p dist/assets; printf built > dist/assets/app.js; fi\n",
        )
        .unwrap();
        fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();
        let frontend = Frontend {
            directory: "web".into(),
            output: "dist".into(),
            install: "share/union/web".into(),
        };

        npm_build_with(
            &source,
            &frontend,
            &npm,
            &[
                ("UNIONC_WEB_MODULE_SUNSHINE", "true"),
                ("UNIONC_WEB_MODULE_HOST_MONITORING", "false"),
            ],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(web.join("invocations")).unwrap(),
            "ci\nrun build\n"
        );
        assert_eq!(
            fs::read_to_string(web.join("dist/assets/app.js")).unwrap(),
            "built"
        );
        assert_eq!(
            fs::read_to_string(web.join("build-environment")).unwrap(),
            "true,false"
        );
    }

    #[cfg(unix)]
    #[test]
    fn frontend_tree_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(temporary.path().join("outside"), "outside").unwrap();
        symlink(temporary.path().join("outside"), source.join("link")).unwrap();
        assert!(
            copy_tree(&source, &temporary.path().join("destination"))
                .unwrap_err()
                .to_string()
                .contains("symlinks")
        );
    }

    #[test]
    fn release_verification_rejects_unlisted_or_changed_files() {
        let temporary = tempfile::tempdir().unwrap();
        let release = temporary.path().join("release");
        make_fake_release(&release, "1.0.0", "first");
        let verified = verify_release(&release).unwrap();
        assert_eq!(verified.files, 2);

        fs::write(release.join("unexpected"), "not inventoried").unwrap();
        assert!(
            verify_release(&release)
                .unwrap_err()
                .to_string()
                .contains("inventory")
        );
        fs::remove_file(release.join("unexpected")).unwrap();
        fs::write(release.join("bin/unionc"), "changed").unwrap();
        assert!(
            verify_release(&release)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_and_rollback_switch_complete_releases() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        let root = temporary.path().join("install");
        make_fake_release(&first, "1.0.0", "first");
        make_fake_release(&second, "1.1.0", "second");

        let installed_first = install_release(&first, &root).unwrap();
        assert_eq!(
            read_pointer(&root, "current").unwrap(),
            Some(installed_first.release_id.clone())
        );
        assert_eq!(read_pointer(&root, "previous").unwrap(), None);

        let installed_second = install_release(&second, &root).unwrap();
        assert_eq!(
            installed_second.previous_release_id,
            Some(installed_first.release_id.clone())
        );
        assert_eq!(
            read_pointer(&root, "current").unwrap(),
            Some(installed_second.release_id.clone())
        );
        assert_eq!(
            read_pointer(&root, "previous").unwrap(),
            Some(installed_first.release_id.clone())
        );

        let rolled_back = rollback_install(&root).unwrap();
        assert_eq!(rolled_back.release_id, installed_first.release_id);
        assert_eq!(
            read_pointer(&root, "previous").unwrap(),
            Some(installed_second.release_id)
        );
        verify_release(&rolled_back.release).unwrap();
    }

    fn make_fake_release(path: &Path, version: &str, executable: &str) {
        fs::create_dir_all(path.join("bin")).unwrap();
        fs::write(path.join("bin/unionc"), executable).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.join("bin/unionc"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let manifest = serde_json::json!({
            "schema_version": 1,
            "distribution": {
                "name": "unionc",
                "version": version,
                "revision": "a".repeat(40),
                "executable": "bin/unionc",
                "frontend": null
            },
            "modules": []
        });
        fs::write(
            path.join("union-release.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
        let checksums = format!(
            "{}  bin/unionc\n{}  union-release.json\n",
            sha256(&path.join("bin/unionc")).unwrap(),
            sha256(&path.join("union-release.json")).unwrap()
        );
        fs::write(path.join("SHA256SUMS"), checksums).unwrap();
    }

    fn sample_config() -> BuildConfig {
        BuildConfig {
            schema_version: 1,
            require_clean_sources: false,
            distribution: Distribution {
                name: "unionc".into(),
                version: "1.2.3".into(),
                source: "union-rust".into(),
                repository: None,
                revision: "a".repeat(40),
                package: "unionc".into(),
                binary: "unionc".into(),
                base_features: vec![],
                frontend: None,
                output: "dist".into(),
            },
            modules: vec![Module {
                id: "photo-backup".into(),
                source: "photo-backup".into(),
                repository: None,
                revision: "b".repeat(40),
                package: "photo-backup-server".into(),
                binary: "photo-backup-server".into(),
                union_feature: "module-photo-backup".into(),
                cargo_features: vec![],
                frontend: None,
                runtime: Runtime {
                    bind: "127.0.0.1:18081".parse().unwrap(),
                    gateway_path: "/modules/photo-backup".into(),
                    liveness_path: "/health/live".into(),
                    readiness_path: Some("/health/ready".into()),
                },
            }],
        }
    }
}
