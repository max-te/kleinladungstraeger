use std::path::PathBuf;

use clap::Parser;
use miette::{Context, IntoDiagnostic, Result};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod app_layer;
mod image_assembly;
mod recipe;
mod registry_client;


#[derive(Parser)]
struct Args {
    /// Path to the recipe TOML file
    recipe_file: PathBuf,

    /// Output the digest of the resulting image to the specified file
    #[clap(short, long)]
    digest_file: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    setup_logging_tracing()?;

    let args = Args::parse();
    run(args).await
}

async fn run(args: Args) -> Result<()> {
    let recipe = crate::recipe::load_recipe(args.recipe_file)?;
    let digest = image_assembly::build_image(&recipe).await?;
    if let Some(digest_file) = args.digest_file {
        std::fs::write(&digest_file, digest.to_string())
            .into_diagnostic()
            .with_context(|| format!("writing digest {} to {}", digest, digest_file.display()))?;
    }
    Ok(())
}

fn setup_logging_tracing() -> Result<()> {
    better_panic::install();
    tracing_subscriber::registry()
        .with(fmt::layer().without_time())
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .into_diagnostic()?,
        )
        .init();
    Ok(())
}
