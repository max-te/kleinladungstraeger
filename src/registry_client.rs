use miette::{IntoDiagnostic, Result};
use oci_spec::image::{ImageConfiguration, ImageIndex, ImageManifest, MediaType};
use reqwest::{header::CONTENT_TYPE, Client, Url};
use secrecy::ExposeSecret;
use std::fmt::Display;
use tracing::{debug, info};

use crate::recipe::Authorization;

pub struct RegistryClient {
    client: reqwest::Client,
    registry: String,
    repo: String,
}

// pub enum ClientScope {
//     Push,
//     Pull,
// }

// impl Display for ClientScope {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         match self {
//             ClientScope::Push => write!(f, "push"),
//             ClientScope::Pull => write!(f, "pull"),
//         }
//     }
// }

impl RegistryClient {
    #[tracing::instrument(skip_all)]
    async fn probe_for_token_endpoint(
        registry: impl ToString,
        repo: impl ToString,
    ) -> Result<String> {
        let registry = registry.to_string();
        let repo = repo.to_string();
        let url = Url::parse(&format!("https://{registry}/v2/{repo}/manifests/latest"))
            .into_diagnostic()?;

        let resp = reqwest::get(url).await.into_diagnostic()?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(wwwauth) = resp.headers().get("WWW-Authenticate") {
                let captures = regex::Regex::new(r#"Bearer realm="([^"]+)",service="([^"]+)""#)
                    .unwrap()
                    .captures(wwwauth.to_str().unwrap())
                    .unwrap();
                let realm = captures.get(1).unwrap().as_str();
                let service = captures.get(2).unwrap().as_str();
                debug!("Found WWW-Authenticate realm: {realm} in {wwwauth:?}");
                Ok(Url::parse_with_params(realm, [("service", service)])
                    .unwrap()
                    .to_string())
            } else {
                Err(miette::miette!("No WWW-Authenticate header"))
            }
        } else {
            Ok(format!("https://{registry}/v2/token"))
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn anonymous(registry: impl ToString, repo: impl ToString) -> Result<Self> {
        let registry = registry.to_string();
        let repo = repo.to_string();
        let realm = Self::probe_for_token_endpoint(&registry, &repo).await?;
        let token_url =
            Url::parse_with_params(&realm, [("scope", format!("repository:{repo}:pull"))])
                .into_diagnostic()?;

        let token_resp = Client::default()
            .get(token_url)
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .json::<serde_json::Value>()
            .await
            .into_diagnostic()?;
        let token = token_resp.get("token").unwrap().as_str().unwrap();
        debug!("Anonymous token: {token}");

        let client = Client::builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            )]))
            .build()
            .into_diagnostic()?;

        Ok(Self {
            client,
            registry,
            repo,
        })
    }

    #[tracing::instrument(skip_all)]
    pub async fn with_basic_auth(
        registry: impl ToString,
        repo: impl ToString,
        username: impl Display,
        password: impl Display,
    ) -> Result<Self> {
        let registry = registry.to_string();
        let repo = repo.to_string();
        let token_url = Url::parse_with_params(
            &format!("https://{registry}/v2/token"),
            [("scope", format!("repository:{repo}:push"))],
        )
        .into_diagnostic()?;
        let token_resp = Client::default()
            .get(token_url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .json::<serde_json::Value>()
            .await
            .into_diagnostic()?;
        let token = token_resp.get("token").unwrap().as_str().unwrap();

        let client = Client::builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            )]))
            .build()
            .into_diagnostic()?;

        Ok(Self {
            client,
            registry,
            repo,
        })
    }

    #[tracing::instrument(skip_all)]
    pub async fn new(
        registry: impl ToString,
        repo: impl ToString,
        auth: &Authorization,
    ) -> Result<Self> {
        match auth {
            Authorization::UserPassword(user, pass) => {
                RegistryClient::with_basic_auth(registry, repo, user, pass.expose_secret()).await
            }
            Authorization::Token(token) => {
                RegistryClient::with_basic_auth(registry, repo, "", token.expose_secret()).await
            }
            Authorization::None => RegistryClient::anonymous(registry, repo).await,
        }
    }

    fn repo_url(&self) -> Result<Url> {
        Url::parse(&format!("https://{}/v2/{}/", self.registry, self.repo)).into_diagnostic()
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_index_or_manifest(&self, tag: impl Display) -> Result<ImageIndex> {
        info!(
            "fetching index for {}/{}:{}",
            &self.registry, &self.repo, tag
        );
        self
            .client
            .get(
                self.repo_url()?
                    .join(&format!("manifests/{}", tag))
                    .into_diagnostic()?,
            )
            .header("Accept", "application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.index.v1+json")
            .send()
            .await
            .inspect(|resp| debug!("get_index: {resp:?}"))
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .json()
            .await
            .into_diagnostic()
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_manifest(&self, digest: impl Display) -> Result<ImageManifest> {
        info!(
            "fetching manifest for {}/{}@{digest}",
            &self.registry, &self.repo
        );
        self.client
            .get(
                self.repo_url()?
                    .join(&format!("manifests/{}", digest))
                    .into_diagnostic()?,
            )
            .header("Accept", String::from(MediaType::ImageManifest))
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .json::<ImageManifest>()
            .await
            .into_diagnostic()
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_config(&self, digest: impl Display) -> Result<ImageConfiguration> {
        self.client
            .get(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest))
                    .into_diagnostic()?,
            )
            .header("Accept", String::from(MediaType::ImageConfig))
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .json::<ImageConfiguration>()
            .await
            .into_diagnostic()
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_tag_for_target(
        &self,
        tag: impl Display,
        arch: oci_spec::image::Arch,
        os: oci_spec::image::Os,
    ) -> Result<(ImageManifest, ImageConfiguration)> {
        let index = self.get_index_or_manifest(tag).await?;
        let manifest_descriptor = index
            .manifests()
            .iter()
            .find(|m| {
                m.platform()
                    .as_ref()
                    .is_some_and(|p| *p.architecture() == arch && *p.os() == os)
            })
            .ok_or_else(|| miette::miette!("could not find manifest for {arch}/{os}"))?;
        let manifest = self.get_manifest(manifest_descriptor.digest()).await?;
        let config = self.get_config(manifest.config().digest()).await?;
        Ok((manifest, config))
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_binary_blob(&self, digest: impl Display) -> Result<bytes::Bytes> {
        info!(
            "downloading blob {digest} from {}/{}",
            self.registry, self.repo
        );
        let blob = self
            .client
            .get(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest))
                    .into_diagnostic()?,
            )
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?
            .bytes()
            .await
            .into_diagnostic()?;

        Ok(blob)
    }

    #[tracing::instrument(skip_all)]
    pub async fn upload_blob(&self, digest: impl Display, contents: Vec<u8>) -> Result<()> {
        info!("uploading blob {digest} to {}/{}", self.registry, self.repo);
        let upload_location_response = self
            .client
            .post(self.repo_url()?.join("blobs/uploads/").into_diagnostic()?)
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?;

        let location_header = upload_location_response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| miette::miette!("no location header in upload location response"))?
            .to_str()
            .into_diagnostic()?;
        let mut upload_location = upload_location_response
            .url()
            .join(location_header)
            .unwrap();
        upload_location
            .query_pairs_mut()
            .append_pair("digest", &format!("{digest}"));

        self.client
            .put(upload_location)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(contents)
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    pub async fn has_blob(&self, digest: impl Display) -> Result<bool> {
        let resp = self
            .client
            .head(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest))
                    .into_diagnostic()?,
            )
            .send()
            .await
            .into_diagnostic()?;
        Ok(resp.status() == reqwest::StatusCode::OK)
    }

    #[tracing::instrument(skip_all)]
    pub async fn upload_manifest(&self, manifest: ImageManifest, tag: impl Display) -> Result<()> {
        info!(
            "uploading manifest for {}/{}:{}",
            &self.registry, &self.repo, &tag
        );
        self.client
            .put(
                self.repo_url()?
                    .join(&format!("manifests/{tag}"))
                    .into_diagnostic()?,
            )
            .body(manifest.to_string().into_diagnostic()?)
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?;
        Ok(())
    }
}
