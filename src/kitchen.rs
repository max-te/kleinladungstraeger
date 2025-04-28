use std::collections::HashMap;
use std::{future::Future, pin::Pin};

use futures::stream::FuturesUnordered;
use futures::TryStreamExt;
use miette::Result;
use oci_spec::image::HistoryBuilder;
use oci_spec::image::ImageManifest;
use oci_spec::image::{Config as ExecConfig, Digest};
use oci_spec::image::{Descriptor, ImageConfiguration};
use tracing::info;

use crate::app_layer::{self, AppLayer};
use crate::registry_client::RegistryClient;

pub(crate) async fn ensure_base_layer(
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

pub(crate) struct PreparationState {
    pub(crate) manifest: ImageManifest,
    pub(crate) configuration: ImageConfiguration,
    pub(crate) base_layers: Vec<Descriptor>,
    pub(crate) own_layers: Vec<AppLayer>,
    pub(crate) base_provider: RegistryClient,
}

impl PreparationState {
    pub(crate) fn new(
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

    pub(crate) fn apply_layer(&mut self, layer: AppLayer) {
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

    pub(crate) fn patch_execution_config(&mut self, patch: &ExecConfig) {
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
                .created_by(format!(
                    "KLT CONFIG {}",
                    serde_json::to_string(&exec_config).unwrap()
                ))
                .build()
                .unwrap(),
        );
        self.configuration.set_config(Some(exec_config));
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn push_to(
        mut self,
        target: &RegistryClient,
        tags: Vec<String>,
    ) -> Result<Digest> {
        info!(
            "pushing image to {}/{}:{tags:?}",
            target.registry, target.repo
        );
        let tasks: FuturesUnordered<Pin<Box<dyn Future<Output = Result<()>> + Send>>> =
            FuturesUnordered::new();

        for layer in self.base_layers.iter() {
            tasks.push(Box::pin(ensure_base_layer(
                &self.base_provider,
                target,
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

        tasks.try_collect::<Vec<()>>().await?;

        let tasks: FuturesUnordered<Pin<Box<dyn Future<Output = Result<()>> + Send>>> =
            FuturesUnordered::new();
        for tag in tags {
            tasks.push(Box::pin(target.upload_manifest(self.manifest.clone(), tag)));
        }
        tasks.try_collect::<Vec<()>>().await?;
        Ok(self.manifest.config().digest().clone())
    }
}

pub(crate) fn image_configuration_to_blob(config: &ImageConfiguration) -> (Vec<u8>, Descriptor) {
    let config_bytes = config.to_string_pretty().unwrap().as_bytes().to_vec();
    let config_digest = app_layer::sha256_digest(&config_bytes);
    let config_descriptor = Descriptor::new(
        oci_spec::image::MediaType::ImageConfig,
        config_bytes.len() as u64,
        config_digest,
    );

    (config_bytes, config_descriptor)
}
