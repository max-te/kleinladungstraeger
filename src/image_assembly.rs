use futures::TryFutureExt;
use miette::{Context, Result};
use oci_spec::image::{Arch, Digest, ImageConfiguration, ImageManifest, Os};
use tracing::{debug, info};

mod state;

use crate::app_layer::AppLayer;
use crate::recipe::Recipe;
use crate::registry_client::{ClientScope, RegistryClient};
use state::PreparationState;

/// Build and push an OCI image from a recipe.
#[tracing::instrument(skip_all)]
pub async fn build_image(recipe: &Recipe) -> Result<Digest> {
    debug!("{:?}", &recipe);

    let base_client = create_base_client(recipe).await?;
    let (base_manifest, base_config, app_layer, target_client) =
        pull_base_and_build_app_layer(recipe, &base_client).await?;

    let image = assemble_image(recipe, base_manifest, base_config, base_client, app_layer);

    debug!("{:?}", &image.manifest());

    let digest = image
        .push_to(&target_client, recipe.target.tags())
        .await
        .with_context(|| "pushing image")?;

    info!(
        "successfully pushed image to {}/{}:{:?}",
        target_client.registry,
        target_client.repo,
        recipe.target.tags()
    );

    Ok(digest)
}

/// Create the base registry client.
async fn create_base_client(recipe: &Recipe) -> Result<RegistryClient> {
    RegistryClient::new(
        &recipe.base.image.resolve_registry(),
        &recipe.base.image.repository(),
        &recipe.base.auth,
        ClientScope::Pull,
    )
    .await
    .context("creating base image registry client")
}

/// Pull the base image manifest + config, build the app layer, and create the target
/// client -- all three run concurrently.
async fn pull_base_and_build_app_layer(
    recipe: &Recipe,
    base_client: &RegistryClient,
) -> Result<(ImageManifest, ImageConfiguration, AppLayer, RegistryClient)> {
    let base_tag = recipe
        .base
        .image
        .digest()
        .or(recipe.base.image.tag())
        .unwrap_or("latest");

    let base = base_client
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

    let ((base_manifest, base_config), app_layer, target_client) =
        flatten_results(tokio::join!(base, app_layer, target_client))?;

    Ok((base_manifest, base_config, app_layer, target_client))
}

/// Assemble the new image from the base image and modifications in the recipe.
fn assemble_image(
    recipe: &Recipe,
    base_manifest: ImageManifest,
    base_config: ImageConfiguration,
    base_client: RegistryClient,
    app_layer: AppLayer,
) -> PreparationState {
    let mut image = PreparationState::new(base_manifest, base_config, base_client);

    image.apply_layer(app_layer);

    recipe
        .modification
        .execution_config
        .as_ref()
        .inspect(|patch| image.patch_execution_config(patch));

    image.set_annotations(recipe.modification.annotations.clone());

    image
}

fn flatten_results<A, B, C, E>(
    tuple: (Result<A, E>, Result<B, E>, Result<C, E>),
) -> Result<(A, B, C), E> {
    Ok((tuple.0?, tuple.1?, tuple.2?))
}
