use futures::TryFutureExt;
use miette::{Context, IntoDiagnostic, Result};
use oci_spec::image::{Arch, Os};
use recipe::Recipe;
use registry_client::ClientScope;
use tracing::{debug, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod app_layer;
mod kitchen;
mod recipe;
mod registry_client;

use crate::app_layer::AppLayer;
use crate::kitchen::PreparationState;
use crate::registry_client::RegistryClient;

fn flatten<A, B, C, E>(tuple: (Result<A, E>, Result<B, E>, Result<C, E>)) -> Result<(A, B, C), E> {
    Ok((tuple.0?, tuple.1?, tuple.2?))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    better_panic::install();
    tracing_subscriber::registry()
        .with(fmt::layer().without_time())
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .into_diagnostic()?,
        )
        .init();
    let recipe_file = std::env::args().nth(1).unwrap_or("recipe.toml".into());
    let recipe: Recipe = crate::recipe::load_recipe(recipe_file)?;
    debug!("{:?}", &recipe);

    let base_provider = RegistryClient::new(
        &recipe.base.registry,
        &recipe.base.repo,
        &recipe.base.auth,
        ClientScope::Pull,
    )
    .await
    .context("creating base image registry client")?;
    let base = base_provider
        .get_tag_for_target(&recipe.base.tag, Arch::Amd64, Os::Linux)
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
        flatten(tokio::join!(base, app_layer, target_client))?;

    let mut image = PreparationState::new(base_image, base_config, base_provider);

    image.apply_layer(app_layer);

    recipe
        .modification
        .execution_config
        .inspect(|patch| image.patch_execution_config(patch));

    debug!("{:?}", &image.manifest);

    image
        .push_to(&target_client, recipe.target.tags())
        .await
        .with_context(|| "pushing image")?;
    info!(
        "successfully pushed image to {}/{}:{tags:?}",
        target_client.registry,
        target_client.repo,
        tags = recipe.target.tags()
    );
    Ok(())
}
