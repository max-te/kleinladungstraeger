use futures::stream::FuturesUnordered;
use futures::TryStreamExt;
use miette::Result;
use oci_spec::image::{
    Arch, Config as ExecConfig, Descriptor, History, HistoryBuilder, ImageConfiguration,
    ImageManifest, Os,
};
use recipe::Recipe;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Display;
use std::{future::Future, pin::Pin};

mod app_layer;
mod recipe;
mod registry_client;

use crate::app_layer::AppLayer;
use crate::registry_client::RegistryClient;

async fn ensure_base_layer(
    provider: &RegistryClient,
    target: &RegistryClient,
    digest: &str,
) -> Result<()> {
    if !target.has_blob(digest).await? {
        let layer = provider.get_binary_blob(&digest).await?.to_vec();
        target.upload_blob(digest, layer).await?;
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
        manifest: ImageManifest,
        configuration: ImageConfiguration,
        base_provider: RegistryClient,
    ) -> Self {
        let base_layers = manifest.layers().clone();
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
            .push(layer.diff_id.clone());
        self.configuration.history_mut().push(History::default());
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
        if patch.user().is_some() {
            exec_config.set_user(patch.user().clone());
        }
        if patch.working_dir().is_some() {
            exec_config.set_working_dir(patch.working_dir().clone());
        }
        if patch.cmd().is_some() {
            exec_config.set_cmd(patch.cmd().clone());
        }
        if patch.stop_signal().is_some() {
            exec_config.set_stop_signal(patch.stop_signal().clone());
        }
        if let Some(new_ports) = patch.exposed_ports() {
            let mut ports = exec_config.exposed_ports().clone().unwrap_or_default();
            ports.extend_from_slice(new_ports);
            exec_config.set_exposed_ports(Some(ports));
        }
        if let Some(new_volumes) = patch.volumes() {
            let mut volumes = exec_config.volumes().clone().unwrap_or_default();
            volumes.extend_from_slice(new_volumes);
            exec_config.set_volumes(Some(volumes));
        }
        if let Some(new_env) = patch.env() {
            let mut env = exec_config.env().clone().unwrap_or_default();
            env.extend_from_slice(new_env);
            exec_config.set_env(Some(env));
        }
        if let Some(new_labels) = patch.labels() {
            let mut labels: HashMap<String, String> =
                exec_config.labels().clone().unwrap_or_default();
            labels.extend(new_labels.iter().map(|(k, v)| (k.clone(), v.clone())));
            exec_config.set_labels(Some(labels));
        }
        self.configuration
            .history_mut()
            .push(HistoryBuilder::default().empty_layer(true).build().unwrap());
        self.configuration.set_config(Some(exec_config));
    }

    async fn push_to(mut self, target: &RegistryClient, tag: impl Display) {
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
                target.upload_blob(layer.descriptor.digest().clone(), layer.contents),
            ));
        }

        let (conf_bytes, conf_desc) = image_configuration_to_blob(&self.configuration);
        tasks.push(Box::pin(
            target.upload_blob(conf_desc.digest().clone(), conf_bytes),
        ));

        self.manifest.set_config(conf_desc);

        tasks.try_collect::<Vec<()>>().await.unwrap();

        target.upload_manifest(self.manifest, tag).await.unwrap();
    }
}

fn image_configuration_to_blob(config: &ImageConfiguration) -> (Vec<u8>, Descriptor) {
    let config_bytes = config.to_string_pretty().unwrap().as_bytes().to_vec();
    let config_digest = base16ct::lower::encode_string(&Sha256::digest(&config_bytes));
    let config_descriptor = Descriptor::new(
        oci_spec::image::MediaType::ImageConfig,
        config_bytes.len() as i64,
        format!("sha256:{config_digest}"),
    );

    (config_bytes, config_descriptor)
}

fn flatten<A, B, C, E>(tuple: (Result<A, E>, Result<B, E>, Result<C, E>)) -> Result<(A, B, C), E> {
    Ok((tuple.0?, tuple.1?, tuple.2?))
}

#[tokio::main]
async fn main() {
    let recipe_file = std::env::args().nth(1).unwrap_or("recipe.toml".into());
    let recipe: Recipe = crate::recipe::load_recipe(recipe_file).unwrap();
    dbg!(&recipe);

    let base_provider =
        RegistryClient::new(&recipe.base.registry, &recipe.base.repo, &recipe.base.auth)
            .await
            .unwrap();
    let base = base_provider.get_tag_for_target(&recipe.base.tag, Arch::Amd64, Os::Linux);
    let app_layer = AppLayer::build_from_directory(&recipe.modification.app_layer_folder);

    let target_client = RegistryClient::new(
        &recipe.target.registry,
        &recipe.target.repo,
        &recipe.target.auth,
    );

    let ((base_image, base_config), app_layer, target_client) =
        flatten(tokio::join!(base, app_layer, target_client)).unwrap();

    let mut image = PreparationState::new(base_image, base_config, base_provider);

    image.apply_layer(app_layer);

    recipe
        .modification
        .execution_config
        .inspect(|patch| image.patch_execution_config(patch));

    image.push_to(&target_client, recipe.target.tag).await;
}
