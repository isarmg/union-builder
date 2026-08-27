use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    pub runtime: Runtime,
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
}

#[derive(Debug, Serialize)]
struct ReleaseModule<'a> {
    id: &'a str,
    revision: &'a str,
    executable: String,
    runtime: &'a Runtime,
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
            features.insert(module.union_feature.as_str()),
            "duplicate Union feature: {}",
            module.union_feature
        );
        validate_revision(&module.revision)?;
        if let Some(repository) = &module.repository {
            validate_repository(repository)?;
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
            !features.is_empty(),
            &options,
        )?;
    }

    let bin_dir = output.join("bin");
    let modules_dir = output.join("libexec/union/modules");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&modules_dir)?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let distribution_artifact = artifact_path(
        &checked.config.distribution.source,
        &options,
        &checked.config.distribution.binary,
        suffix,
    );
    let distribution_install =
        bin_dir.join(format!("{}{}", checked.config.distribution.binary, suffix));
    copy_executable(&distribution_artifact, &distribution_install)?;
    for module in &checked.config.modules {
        let artifact = artifact_path(&module.source, &options, &module.binary, suffix);
        let install = modules_dir.join(format!("{}{}", module.id, suffix));
        copy_executable(&artifact, &install)?;
    }

    let manifest_path = output.join("union-release.json");
    let manifest = ReleaseManifest {
        schema_version: 1,
        distribution: ReleaseDistribution {
            name: &checked.config.distribution.name,
            version: &checked.config.distribution.version,
            revision: &checked.distribution_source.revision,
            executable: relative(&output, &distribution_install)?,
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
    let mut checksum_files = vec![distribution_install, manifest_path.clone()];
    checksum_files.extend(
        checked
            .config
            .modules
            .iter()
            .map(|module| modules_dir.join(format!("{}{}", module.id, suffix))),
    );
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
    fn a_core_only_distribution_is_valid() {
        let mut config = sample_config();
        config.modules.clear();
        validate_config(&config).unwrap();
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
