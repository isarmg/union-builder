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
    }
    Ok(())
}
