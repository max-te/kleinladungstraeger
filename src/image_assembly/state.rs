use std::collections::HashMap;
use std::{future::Future, pin::Pin};

use futures::TryStreamExt;
use futures::stream::FuturesUnordered;
use miette::Result;
use oci_spec::image::HistoryBuilder;
use oci_spec::image::ImageManifest;
use oci_spec::image::{Config as ExecConfig, Digest};
use oci_spec::image::{Descriptor, ImageConfiguration};
use tracing::info;

use crate::app_layer::{self, AppLayer};
use crate::recipe::TagName;
use crate::registry_client::RegistryClient;

/// Ensure the layer with the given digest is known at the target registry,
/// copying it from the provider if necessary.
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

pub(crate) struct PreparationState {
    manifest: ImageManifest,
    configuration: ImageConfiguration,
    base_layers: Vec<Descriptor>,
    own_layers: Vec<AppLayer>,
    base_provider: RegistryClient,
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
        self.configuration
            .history_mut()
            .get_or_insert_default()
            .push(
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
        self.configuration
            .history_mut()
            .get_or_insert_default()
            .push(
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
        tags: Vec<TagName>,
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

        let tasks: FuturesUnordered<Pin<Box<dyn Future<Output = Result<Digest>> + Send>>> =
            FuturesUnordered::new();
        for tag in tags {
            tasks.push(Box::pin(target.upload_manifest(self.manifest.clone(), tag)));
        }
        let mut digests = tasks.try_collect::<Vec<Digest>>().await?;
        Ok(digests.pop().unwrap())
    }

    /// Read-only access to the assembled manifest (for debug logging).
    pub(crate) fn manifest(&self) -> &ImageManifest {
        &self.manifest
    }
    pub fn set_annotations(&mut self, annotations: HashMap<String, String>) {
        info!("setting manifest annotations to {annotations:?}");
        self.manifest.set_annotations(Some(annotations));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_layer::AppLayer;
    use oci_spec::image::{
        ConfigBuilder, Descriptor, Digest, ImageConfigurationBuilder, ImageManifestBuilder,
        MediaType, RootFsBuilder,
    };
    use std::str::FromStr;

    const LAYER_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CONFIG_DIGEST: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn dummy_layer_descriptor() -> Descriptor {
        Descriptor::new(
            MediaType::ImageLayerGzip,
            100,
            Digest::from_str(LAYER_DIGEST).unwrap(),
        )
    }

    fn dummy_config() -> ImageConfiguration {
        ImageConfigurationBuilder::default()
            .architecture(oci_spec::image::Arch::Amd64)
            .os(oci_spec::image::Os::Linux)
            .rootfs(
                RootFsBuilder::default()
                    .diff_ids(Vec::new())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    fn dummy_manifest(layers: Vec<Descriptor>) -> ImageManifest {
        let config_desc = Descriptor::new(
            MediaType::ImageConfig,
            200,
            Digest::from_str(CONFIG_DIGEST).unwrap(),
        );
        ImageManifestBuilder::default()
            .schema_version(2u32)
            .config(config_desc)
            .layers(layers)
            .build()
            .unwrap()
    }

    fn dummy_app_layer() -> AppLayer {
        AppLayer {
            contents: vec![1, 2, 3],
            descriptor: Descriptor::new(
                MediaType::ImageLayerGzip,
                3,
                Digest::from_str(
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                )
                .unwrap(),
            ),
            diff_id: Digest::from_str(
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            )
            .unwrap(),
            created_by: "KLT COPY test/* /".to_string(),
        }
    }

    fn dummy_client() -> RegistryClient {
        RegistryClient::test_dummy("test-registry", "test-repo")
    }

    #[test]
    fn test_new_extracts_base_layers_and_fixes_media_types() {
        let legacy_media =
            MediaType::Other("application/vnd.docker.image.rootfs.diff.tar.gzip".into());
        let layer_desc =
            Descriptor::new(legacy_media, 100, Digest::from_str(LAYER_DIGEST).unwrap());
        let manifest = dummy_manifest(vec![layer_desc]);
        let config = dummy_config();

        let state = PreparationState::new(manifest, config, dummy_client());

        // base_layers is the source snapshot — preserves original media types
        assert_eq!(state.base_layers.len(), 1);
        assert_eq!(
            state.base_layers[0].media_type(),
            &MediaType::Other("application/vnd.docker.image.rootfs.diff.tar.gzip".into())
        );
        // manifest is the target — legacy media types are fixed up
        assert_eq!(
            state.manifest.layers()[0].media_type(),
            &MediaType::ImageLayerGzip
        );
        assert_eq!(
            state.manifest.media_type().as_ref().unwrap(),
            &MediaType::ImageManifest
        );
        assert!(state.own_layers.is_empty());
    }

    #[test]
    fn test_apply_layer_adds_diff_id_and_history() {
        let manifest = dummy_manifest(vec![dummy_layer_descriptor()]);
        let config = dummy_config();
        let mut state = PreparationState::new(manifest, config, dummy_client());
        let layer = dummy_app_layer();
        let expected_diff_id = layer.diff_id.to_string();
        state.apply_layer(layer);
        assert_eq!(state.own_layers.len(), 1);
        // manifest layers: 1 base + 1 new
        assert_eq!(state.manifest.layers().len(), 2);
        // diff_ids should have the new layer's diff_id
        let diff_ids = state.configuration.rootfs().diff_ids();
        assert_eq!(diff_ids.len(), 1);
        assert_eq!(&diff_ids[0], &expected_diff_id);
        // history should have one entry
        let history = state.configuration.history().as_ref().unwrap();
        assert_eq!(history.len(), 1);
        assert!(
            history[0]
                .created_by()
                .as_ref()
                .unwrap()
                .contains("KLT COPY")
        );
    }
    #[test]
    fn test_patch_execution_config_sets_all_fields() {
        let manifest = dummy_manifest(vec![]);
        let config = dummy_config();
        let mut state = PreparationState::new(manifest, config, dummy_client());
        let patch = ConfigBuilder::default()
            .user("testuser")
            .working_dir("/app")
            .cmd(vec!["sh".to_string(), "-c".to_string()])
            .stop_signal("SIGTERM")
            .env(vec!["KEY=value".to_string()])
            .labels([("label1".to_string(), "val1".to_string())])
            .build()
            .unwrap();
        state.patch_execution_config(&patch);
        let result = state.configuration.config().as_ref().unwrap();
        assert_eq!(result.user().as_ref().unwrap(), "testuser");
        assert_eq!(result.working_dir().as_ref().unwrap(), "/app");
        assert_eq!(
            result.cmd().as_ref().unwrap().as_slice(),
            &["sh".to_string(), "-c".to_string()]
        );
        assert_eq!(result.stop_signal().as_ref().unwrap(), "SIGTERM");
        assert!(
            result
                .env()
                .as_ref()
                .unwrap()
                .contains(&"KEY=value".to_string())
        );
        assert_eq!(
            result.labels().as_ref().unwrap().get("label1").unwrap(),
            "val1"
        );
    }
    #[test]
    fn test_patch_execution_config_merges_existing_fields() {
        let manifest = dummy_manifest(vec![]);
        let config = ImageConfigurationBuilder::default()
            .architecture(oci_spec::image::Arch::Amd64)
            .os(oci_spec::image::Os::Linux)
            .config(
                ConfigBuilder::default()
                    .user("olduser")
                    .env(vec!["OLD=val".to_string()])
                    .build()
                    .unwrap(),
            )
            .rootfs(
                RootFsBuilder::default()
                    .diff_ids(Vec::new())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let mut state = PreparationState::new(manifest, config, dummy_client());
        let patch = ConfigBuilder::default()
            .env(vec!["NEW=val".to_string()])
            .build()
            .unwrap();
        state.patch_execution_config(&patch);
        let result = state.configuration.config().as_ref().unwrap();
        assert_eq!(result.user().as_ref().unwrap(), "olduser"); // preserved
        let env = result.env().as_ref().unwrap();
        assert!(env.contains(&"OLD=val".to_string())); // preserved
        assert!(env.contains(&"NEW=val".to_string())); // added
        assert_eq!(env.len(), 2);
    }
    #[test]
    fn test_set_annotations() {
        let manifest = dummy_manifest(vec![]);
        let config = dummy_config();
        let mut state = PreparationState::new(manifest, config, dummy_client());
        let mut annotations = HashMap::new();
        annotations.insert("key".to_string(), "value".to_string());
        state.set_annotations(annotations);
        assert_eq!(
            state
                .manifest
                .annotations()
                .as_ref()
                .unwrap()
                .get("key")
                .unwrap(),
            "value"
        );
    }
    #[test]
    fn test_image_configuration_to_blob() {
        let config = dummy_config();
        let (bytes, descriptor) = image_configuration_to_blob(&config);
        assert!(!bytes.is_empty());
        assert_eq!(descriptor.media_type(), &MediaType::ImageConfig);
        assert_eq!(descriptor.size(), bytes.len() as u64);
        assert!(descriptor.digest().to_string().starts_with("sha256:"));
    }
}
