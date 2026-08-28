use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use union_builder::{BuildOptions, OutputFormat};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pin Union-owned entries to a verified workflow caller checkout and emit a new config.
    Materialize {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
        /// Exact repository identity used to match the distribution and same-repository modules.
        #[arg(long)]
        caller_repository: String,
        /// Local Git worktree root checked out by the calling workflow.
        #[arg(long)]
        caller_source: PathBuf,
        /// Canonical lowercase 40-character Git ID of the caller checkout.
        #[arg(long)]
        caller_revision: String,
        /// New schema-v2 config; an existing path is never overwritten.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Validate source revisions and the complete build graph without compiling.
    Check {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
    },
    /// Print the exact Core, Web Shell and release-bundled module packages to build.
    Plan {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Build Core once, build selected modules independently, and assemble one distribution.
    Build {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
        /// Cargo artifact profile; module inclusion is selected only by --config.
        #[arg(long, default_value = "release")]
        cargo_profile: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Verify a complete Union release, including its exact file inventory and checksums.
    Verify {
        #[arg(long)]
        release: PathBuf,
    },
    /// Copy a verified release into an immutable install slot without activating it.
    Stage {
        #[arg(long)]
        release: PathBuf,
        #[arg(long)]
        root: PathBuf,
    },
    /// Stage and atomically activate one complete Union release (Unix only).
    Install {
        #[arg(long)]
        release: PathBuf,
        #[arg(long)]
        root: PathBuf,
    },
    /// Atomically reactivate the previous complete Union release (Unix only).
    Rollback {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Materialize {
            config,
            caller_repository,
            caller_source,
            caller_revision,
            output,
        } => {
            let result = union_builder::materialize_caller_checkout(
                &config,
                &caller_repository,
                &caller_source,
                &caller_revision,
                &output,
            )?;
            println!(
                "materialized {} Union-owned entry/entries at {}",
                result.matched_entries,
                result.output.display()
            );
        }
        Command::Check { config } => {
            let checked = union_builder::load_and_check(&config)?;
            println!(
                "ok: {} {} with {} release-bundled process module(s)",
                checked.config.distribution.name,
                checked.config.distribution.version,
                checked.config.modules.len()
            );
        }
        Command::Plan { config, format } => {
            let checked = union_builder::load_and_check(&config)?;
            let output = match format {
                Format::Text => OutputFormat::Text,
                Format::Json => OutputFormat::Json,
            };
            println!("{}", union_builder::render_plan(&checked, output)?);
        }
        Command::Build {
            config,
            cargo_profile,
            target,
            output,
        } => {
            let result = union_builder::build(
                &config,
                BuildOptions {
                    cargo_profile,
                    target,
                    output,
                },
            )?;
            println!("assembled {}", result.output.display());
            println!("manifest {}", result.manifest.display());
            println!("checksums {}", result.checksums.display());
        }
        Command::Verify { release } => {
            let result = union_builder::verify_release(&release)?;
            println!(
                "verified {} ({} files, id {})",
                result.release.display(),
                result.files,
                result.release_id
            );
        }
        Command::Stage { release, root } => {
            let result = union_builder::stage_release(&release, &root)?;
            println!(
                "staged {} at {}",
                result.release_id,
                result.release.display()
            );
        }
        Command::Install { release, root } => {
            let result = union_builder::install_release(&release, &root)?;
            println!(
                "active {} at {}",
                result.release_id,
                result.release.display()
            );
            if let Some(previous) = result.previous_release_id {
                println!("previous {previous}");
            }
        }
        Command::Rollback { root } => {
            let result = union_builder::rollback_install(&root)?;
            println!(
                "active {} at {}",
                result.release_id,
                result.release.display()
            );
            if let Some(previous) = result.previous_release_id {
                println!("previous {previous}");
            }
        }
    }
    Ok(())
}
