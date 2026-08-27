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
    /// Validate source revisions and the complete build graph without compiling.
    Check {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
    },
    /// Print the exact packages, features, binaries and install paths that would be built.
    Plan {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Compile the selected graph and assemble one Union distribution directory.
    Build {
        #[arg(short, long, default_value = "union-build.toml")]
        config: PathBuf,
        #[arg(long, default_value = "release")]
        profile: String,
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
        Command::Check { config } => {
            let checked = union_builder::load_and_check(&config)?;
            println!(
                "ok: {} {} with {} process module(s)",
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
            profile,
            target,
            output,
        } => {
            let result = union_builder::build(
                &config,
                BuildOptions {
                    profile,
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
