use flate2::{write::GzEncoder, Compression};
use miette::{IntoDiagnostic, Result};
use oci_spec::image::Descriptor;
use sha2::{Digest, Sha256};
use std::io::Write;

fn tar_folder(src_path: impl AsRef<std::path::Path>) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let mut tar = tar::Builder::new(buf);
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
    pub diff_id: String,
}

impl AppLayer {
    pub async fn build_from_directory(input_folder: &str) -> Result<AppLayer> {
        let input_folder = std::path::Path::new(input_folder).to_owned();
        tokio::task::spawn_blocking(move || {
            let contents_plain = tar_folder(input_folder)?;
            let plain_len = contents_plain.len();
            let plain_digest = base16ct::lower::encode_string(&Sha256::digest(&contents_plain));
            println!("App Layer uncompressed size: {plain_len} bytes");

            let contents = gzip(contents_plain)?;
            let layer_digest = base16ct::lower::encode_string(&Sha256::digest(&contents));
            let layer_size = contents.len();
            println!(
                "App Layer compressed size: {layer_size} bytes ({:.2}%)",
                layer_size as f32 / plain_len as f32 * 100.0
            );
            let descriptor = Descriptor::new(
                oci_spec::image::MediaType::ImageLayerGzip,
                layer_size as i64,
                format!("sha256:{layer_digest}"),
            );

            Ok(AppLayer {
                contents,
                descriptor,
                diff_id: format!("sha256:{plain_digest}"),
            })
        })
        .await
        .into_diagnostic()?
    }
}
