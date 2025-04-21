use flate2::{write::GzEncoder, Compression};
use miette::{Context, IntoDiagnostic, Result};
use oci_spec::image::{Descriptor, Digest};
use sha2::{Digest as _, Sha256};
use std::io::Write;
use tracing::info;

fn tar_folder(src_path: impl AsRef<std::path::Path>) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let mut tar = tar::Builder::new(buf);
    tar.follow_symlinks(false);
    tar.mode(tar::HeaderMode::Deterministic);
    tar.append_dir_all("", src_path).into_diagnostic()?;
    tar.into_inner().into_diagnostic()
}

fn gzip(input: Vec<u8>) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let mut encoder = GzEncoder::new(buf, Compression::fast());
    encoder.write_all(&input).into_diagnostic()?;
    encoder.finish().into_diagnostic()
}

pub struct AppLayer {
    pub contents: Vec<u8>,
    pub descriptor: Descriptor,
    pub diff_id: Digest,
    pub created_by: String,
}

impl AppLayer {
    #[tracing::instrument(skip_all)]
    pub async fn build_from_directory(input_folder: &str) -> Result<AppLayer> {
        let input_folder = std::path::Path::new(input_folder).to_owned();

        let thread_span = tracing::debug_span!("thread").or_current();
        tokio::task::spawn_blocking(move || {
            let _entered = thread_span.entered();

            info!("building app layer from {input_folder:?}");
            let contents_plain = tar_folder(&input_folder).with_context(|| format!("tarring {input_folder:?}"))?;
            let plain_len = contents_plain.len();
            let plain_digest = sha256_digest(&contents_plain);
            info!("App Layer uncompressed size: {plain_len} bytes");

            let contents = gzip(contents_plain).with_context(|| "gzipping tarred contents")?;
            let layer_digest = sha256_digest(&contents);
            let layer_size = contents.len();
            info!(
                "App Layer compressed size: {layer_size} bytes ({:.2}%)",
                layer_size as f32 / plain_len as f32 * 100.0
            );
            let descriptor = Descriptor::new(
                oci_spec::image::MediaType::ImageLayerGzip,
                layer_size as u64,
                layer_digest,
            );

            Ok(AppLayer {
                contents,
                descriptor,
                diff_id: plain_digest,
                created_by: format!("KLT COPY {}/* /", input_folder.to_str().unwrap()),
            })
        })
        .await
        .into_diagnostic().with_context(|| "building app layer")?
    }
}

pub fn sha256_digest(bytes: &[u8]) -> Digest {
    let digest = Sha256::digest(bytes);
    let digest_str = base16ct::lower::encode_string(&digest);
    Digest::try_from(format!("sha256:{digest_str}"))
        .expect("should be valid sha256 digest")
}
