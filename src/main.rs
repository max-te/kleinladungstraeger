use std::path::PathBuf;

use clap::Parser;
use futures::TryFutureExt;
use miette::{Context, IntoDiagnostic, Result};
use oci_spec::image::{Arch, Os};
use recipe::Recipe;
use registry_client::ClientScope;
use tracing::{debug, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod app_layer;
mod kitchen;
mod recipe;
mod registry_client;

use crate::app_layer::AppLayer;
use crate::kitchen::PreparationState;
use crate::registry_client::RegistryClient;

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
    let recipe: Recipe = crate::recipe::load_recipe(args.recipe_file)?;
    debug!("{:?}", &recipe);

    let base_provider = RegistryClient::new(
        &recipe.base.image.resolve_registry(),
        &recipe.base.image.repository(),
        &recipe.base.auth,
        ClientScope::Pull,
    )
    .await
    .context("creating base image registry client")?;
    let base_tag = recipe
        .base
        .image
        .digest()
        .or(recipe.base.image.tag())
        .unwrap_or("latest");
    let base = base_provider
        .get_tag_for_target(base_tag, Arch::Amd64, Os::Linux)
        .map_err(|e| e.context("getting base image"));
    let app_layer = AppLayer::build_from_directory(&recipe.modification.app_layer_folder)
        .map_err(|e| e.context("building app layer"));

    let target_client = RegistryClient::new(
        &recipe.target.registry,
        &recipe.target.repo,
        &recipe.target.auth,
        ClientScope::Push,
    )
    .map_err(|e| e.context("creating target registry client"));

    let ((base_image, base_config), app_layer, target_client) =
        flatten_results(tokio::join!(base, app_layer, target_client))?;

    let mut image = PreparationState::new(base_image, base_config, base_provider);

    image.apply_layer(app_layer);

    recipe
        .modification
        .execution_config
        .inspect(|patch| image.patch_execution_config(patch));

    image.set_annotations(recipe.modification.annotations);

    debug!("{:?}", &image.manifest);

    let digest = image
        .push_to(&target_client, recipe.target.tags())
        .await
        .with_context(|| "pushing image")?;
    info!(
        "successfully pushed image to {}/{}:{tags:?}",
        target_client.registry,
        target_client.repo,
        tags = recipe.target.tags()
    );
    if let Some(digest_file) = args.digest_file {
        std::fs::write(&digest_file, digest.to_string())
            .into_diagnostic()
            .with_context(|| format!("writing digest {} to {}", digest, digest_file.display()))?;
    }
    Ok(())
}

fn flatten_results<A, B, C, E>(
    tuple: (Result<A, E>, Result<B, E>, Result<C, E>),
) -> Result<(A, B, C), E> {
    Ok((tuple.0?, tuple.1?, tuple.2?))
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
