use futures::stream::FuturesUnordered;
use futures::{TryFutureExt, TryStreamExt};
use miette::{Context, IntoDiagnostic, Result};
use oci_spec::image::{
    Arch, Config as ExecConfig, Digest, Descriptor, HistoryBuilder, ImageConfiguration, ImageManifest, Os,
};
use recipe::Recipe;
use std::collections::HashMap;
use std::fmt::Display;
use std::{future::Future, pin::Pin};
use tracing::{debug, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod app_layer;
mod recipe;
mod registry_client;

use crate::app_layer::AppLayer;
use crate::registry_client::RegistryClient;

async fn ensure_base_layer(
    provider: &RegistryClient,
    target: &RegistryClient,
    digest: &Digest,
) -> Result<()> {
    if !target.has_blob(digest).await? {
        info!("base layer {digest} is not known at target, copying from upstream");
        let layer = provider.get_binary_blob(digest).await?.to_vec();
        target.upload_blob(digest, layer).await?;
    } else {
        info!("base layer {digest} is already known at target");
    }
    Ok(())
}

struct PreparationState {
    manifest: ImageManifest,
    configuration: ImageConfiguration,
    base_layers: Vec<Descriptor>,
    own_layers: Vec<AppLayer>,
    base_provider: RegistryClient,
}

impl PreparationState {
    fn new(
        mut manifest: ImageManifest,
        configuration: ImageConfiguration,
        base_provider: RegistryClient,
    ) -> Self {
        let base_layers = manifest.layers().clone();
        manifest.set_media_type(Some(oci_spec::image::MediaType::ImageManifest));
        manifest
            .layers_mut()
            .iter_mut()
            .filter(|layer| {
                *layer.media_type()
                    == oci_spec::image::MediaType::Other(String::from(
                        "application/vnd.docker.image.rootfs.diff.tar.gzip",
                    ))
            })
            .for_each(|layer| {
                layer.set_media_type(oci_spec::image::MediaType::ImageLayerGzip);
            });
        Self {
            manifest,
            configuration,
            base_layers,
            own_layers: Vec::new(),
            base_provider,
        }
    }

    fn apply_layer(&mut self, layer: AppLayer) {
        self.configuration
            .rootfs_mut()
            .diff_ids_mut()
            .push(layer.diff_id.to_string());
        self.configuration.history_mut().push(
            HistoryBuilder::default()
                .created_by(&layer.created_by)
                .build()
                .unwrap(),
        );
        self.manifest.layers_mut().push(layer.descriptor.clone());
        self.own_layers.push(layer);
    }

    fn patch_execution_config(&mut self, patch: &ExecConfig) {
        let mut exec_config = self
            .configuration
            .config()
            .as_ref()
            .cloned()
            .unwrap_or_default();
        if let Some(user) = patch.user() {
            info!("setting user to {}", user);
            exec_config.set_user(Some(user.clone()));
        }
        if let Some(working_dir) = patch.working_dir() {
            info!("setting working dir to {}", working_dir);
            exec_config.set_working_dir(Some(working_dir.clone()));
        }
        if let Some(cmd) = patch.cmd() {
            info!("setting cmd to {:?}", cmd);
            exec_config.set_cmd(Some(cmd.clone()));
        }
        if let Some(stop_signal) = patch.stop_signal() {
            info!("setting stop signal to {}", stop_signal);
            exec_config.set_stop_signal(Some(stop_signal.clone()));
        }
        if let Some(new_ports) = patch.exposed_ports() {
            info!("adding exposed ports {new_ports:?}");
            let mut ports = exec_config.exposed_ports().clone().unwrap_or_default();
            ports.extend_from_slice(new_ports);
            exec_config.set_exposed_ports(Some(ports));
        }
        if let Some(new_volumes) = patch.volumes() {
            info!("adding volumes {new_volumes:?}");
            let mut volumes = exec_config.volumes().clone().unwrap_or_default();
            volumes.extend_from_slice(new_volumes);
            exec_config.set_volumes(Some(volumes));
        }
        if let Some(new_env) = patch.env() {
            info!("adding environment variables {new_env:?}");
            let mut env = exec_config.env().clone().unwrap_or_default();
            env.extend_from_slice(new_env);
            exec_config.set_env(Some(env));
        }
        if let Some(new_labels) = patch.labels() {
            info!("adding labels {new_labels:?}");
            let mut labels: HashMap<String, String> =
                exec_config.labels().clone().unwrap_or_default();
            labels.extend(new_labels.iter().map(|(k, v)| (k.clone(), v.clone())));
            exec_config.set_labels(Some(labels));
        }
        self.configuration.history_mut().push(
            HistoryBuilder::default()
                .empty_layer(true)
                .created_by(format!("KLT {:?}", exec_config))
                .build()
                .unwrap(),
        );
        self.configuration.set_config(Some(exec_config));
    }

    #[tracing::instrument(skip_all)]
    async fn push_to(mut self, target: &RegistryClient, tag: impl Display) -> Result<()> {
        info!("pushing image to {}/{}:{tag}", target.registry, target.repo);
        let tasks: FuturesUnordered<Pin<Box<dyn Future<Output = Result<()>> + Send>>> =
            FuturesUnordered::new();

        for layer in self.base_layers.iter() {
            tasks.push(Box::pin(ensure_base_layer(
                &self.base_provider,
                &target,
                layer.digest(),
            )));
        }

        for layer in self.own_layers {
            tasks.push(Box::pin(
                target.upload_blob(layer.descriptor.digest().clone(), layer.contents)
            ));
        }

        let (conf_bytes, conf_desc) = image_configuration_to_blob(&self.configuration);
        tasks.push(Box::pin(
            target.upload_blob(conf_desc.digest().clone(), conf_bytes)
        ));

        self.manifest.set_config(conf_desc);

        tasks.try_collect::<Vec<()>>().await?;

        target.upload_manifest(self.manifest, tag).await?;
        Ok(())
    }
}

fn image_configuration_to_blob(config: &ImageConfiguration) -> (Vec<u8>, Descriptor) {
    let config_bytes = config.to_string_pretty().unwrap().as_bytes().to_vec();
    let config_digest = app_layer::sha256_digest(&config_bytes);
    let config_descriptor = Descriptor::new(
        oci_spec::image::MediaType::ImageConfig,
        config_bytes.len() as u64,
        config_digest,
    );

    (config_bytes, config_descriptor)
}

fn flatten<A, B, C, E>(tuple: (Result<A, E>, Result<B, E>, Result<C, E>)) -> Result<(A, B, C), E> {
    Ok((tuple.0?, tuple.1?, tuple.2?))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    better_panic::install();
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new("info"))
                .into_diagnostic()?,
        )
        .init();
    let recipe_file = std::env::args().nth(1).unwrap_or("recipe.toml".into());
    let recipe: Recipe = crate::recipe::load_recipe(recipe_file)?;
    debug!("{:?}", &recipe);

    let base_provider =
        RegistryClient::new(&recipe.base.registry, &recipe.base.repo, &recipe.base.auth)
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
        .push_to(&target_client, recipe.target.tag.clone())
        .await
        .with_context(|| "pushing image")?;
    info!("successfully pushed image to {}/{}:{tag}", target_client.registry, target_client.repo, tag = recipe.target.tag);
    Ok(())
}
