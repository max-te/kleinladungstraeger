use miette::{IntoDiagnostic, Result};
use oci_spec::image::{Digest, ImageConfiguration, ImageIndex, ImageManifest, MediaType};
use reqwest::{Client, Url};
use secrecy::ExposeSecret;
use std::{borrow::Borrow, fmt::Display, str::FromStr};
use tracing::{debug, info};

use crate::recipe::Authorization;

pub struct RegistryClient<const INSECURE: bool = false> {
    client: reqwest::Client,
    pub registry: String,
    pub repo: String,
}

pub enum ClientScope {
    Push,
    Pull,
}

impl Display for ClientScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientScope::Push => write!(f, "push"),
            ClientScope::Pull => write!(f, "pull"),
        }
    }
}

impl RegistryClient {
    #[tracing::instrument(skip_all)]
    pub async fn new(
        registry: impl ToString,
        repo: impl ToString,
        auth: &Authorization,
        scope: ClientScope,
    ) -> Result<Self> {
        match auth {
            Authorization::UserPassword(user, pass) => {
                RegistryClient::<false>::with_basic_auth(
                    registry,
                    repo,
                    user,
                    pass.expose_secret(),
                    scope,
                )
                .await
            }
            Authorization::Token(token) => {
                RegistryClient::<false>::with_basic_auth(
                    registry,
                    repo,
                    "",
                    token.expose_secret(),
                    scope,
                )
                .await
            }
            Authorization::None => RegistryClient::<false>::anonymous(registry, repo, scope).await,
        }
    }
}

impl<const INSECURE: bool> RegistryClient<INSECURE> {
    fn scheme() -> &'static str {
        if INSECURE {
            "http"
        } else {
            "https"
        }
    }

    #[tracing::instrument(skip_all)]
    async fn probe_for_token_endpoint(registry: impl ToString) -> Result<String> {
        let registry = registry.to_string();
        let url = Url::parse(&format!(
            "{scheme}://{registry}/v2/",
            scheme = Self::scheme()
        ))
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
            debug!("No WWW-Authenticate header but not 401, falling back to token endpoint.",);
            Ok(format!(
                "{scheme}://{registry}/v2/token",
                scheme = Self::scheme()
            ))
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn anonymous(
        registry: impl ToString,
        repo: impl ToString,
        scope: ClientScope,
    ) -> Result<Self> {
        let registry = registry.to_string();
        let repo = repo.to_string();
        let realm = Self::probe_for_token_endpoint(&registry).await?;
        let token_url =
            Url::parse_with_params(&realm, [("scope", format!("repository:{repo}:{scope}"))])
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
        scope: ClientScope,
    ) -> Result<Self> {
        let registry = registry.to_string();
        let repo = repo.to_string();

        let realm = Self::probe_for_token_endpoint(&registry).await?;
        let token_url =
            Url::parse_with_params(&realm, [("scope", format!("repository:{repo}:{scope}"))])
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

    fn repo_url(&self) -> Result<Url> {
        Url::parse(&format!(
            "{}://{}/v2/{}/",
            Self::scheme(),
            self.registry,
            self.repo
        ))
        .into_diagnostic()
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
    pub async fn get_manifest(&self, digest: impl Borrow<Digest>) -> Result<ImageManifest> {
        info!(
            "fetching manifest for {}/{}@{}",
            &self.registry,
            &self.repo,
            digest.borrow()
        );
        self.client
            .get(
                self.repo_url()?
                    .join(&format!("manifests/{}", digest.borrow()))
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
    pub async fn get_config(&self, digest: impl Borrow<Digest>) -> Result<ImageConfiguration> {
        info!(
            "fetching config for {}/{}@{}",
            &self.registry,
            &self.repo,
            digest.borrow()
        );
        self.client
            .get(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest.borrow()))
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
    pub async fn get_binary_blob(&self, digest: impl Borrow<Digest>) -> Result<bytes::Bytes> {
        info!(
            "downloading blob {} from {}/{}",
            digest.borrow(),
            self.registry,
            self.repo
        );
        let blob = self
            .client
            .get(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest.borrow()))
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
    pub async fn upload_blob(&self, digest: impl Borrow<Digest>, contents: Vec<u8>) -> Result<()> {
        info!(
            "uploading blob {} to {}/{}",
            digest.borrow(),
            self.registry,
            self.repo
        );
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
            .append_pair("digest", digest.borrow().as_ref());

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
    pub async fn has_blob(&self, digest: impl Borrow<Digest>) -> Result<bool> {
        let resp = self
            .client
            .head(
                self.repo_url()?
                    .join(&format!("blobs/{}", digest.borrow()))
                    .into_diagnostic()?,
            )
            .send()
            .await
            .into_diagnostic()?;
        Ok(resp.status() == reqwest::StatusCode::OK)
    }

    #[tracing::instrument(skip_all)]
    pub async fn upload_manifest(
        &self,
        manifest: ImageManifest,
        tag: impl Display,
    ) -> Result<Digest> {
        info!(
            "uploading manifest for {}/{}:{}",
            &self.registry, &self.repo, &tag
        );
        let res = self
            .client
            .put(
                self.repo_url()?
                    .join(&format!("manifests/{tag}"))
                    .into_diagnostic()?,
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                manifest.media_type().as_ref().unwrap().to_string(),
            )
            .body(manifest.to_string().into_diagnostic()?)
            .send()
            .await
            .into_diagnostic()?
            .error_for_status()
            .into_diagnostic()?;
        res.headers()
            .get("docker-content-digest")
            .ok_or(miette::miette!(
                "Missing docker-content-digest header in registry response"
            ))
            .and_then(|h| h.to_str().into_diagnostic())
            .and_then(|s| Digest::from_str(s).into_diagnostic())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::image::{Arch, Os};
    use std::str::FromStr;
    use test_log::test;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    const TEST_DIGEST: &str =
        "sha256:9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a";
    const CONFIG_DIGEST: &str =
        "sha256:1010101010101010101010101010101010101010101010101010101010101010";

    #[test(tokio::test)]
    async fn test_anonymous_client_creation() -> Result<()> {
        let mock_server = MockServer::start().await;
        let registry_url = mock_server.uri().replace("http://", "");

        // Mock the probe endpoint
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    "Bearer realm=\"http://{server}/auth\",service=\"{server}\"",
                    server = registry_url
                ),
            ))
            .mount(&mock_server)
            .await;

        // Mock the auth endpoint
        Mock::given(method("GET"))
            .and(path("/auth"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "test-token"
            })))
            .mount(&mock_server)
            .await;

        let client =
            RegistryClient::<true>::anonymous(&registry_url, "test-repo", ClientScope::Pull)
                .await?;

        assert_eq!(client.registry, registry_url);
        assert_eq!(client.repo, "test-repo");
        Ok(())
    }

    #[test(tokio::test)]
    async fn test_basic_auth_client_creation() -> Result<()> {
        let mock_server = MockServer::start().await;
        let registry_url = mock_server.uri().replace("http://", "");

        // Mock the token endpoint
        Mock::given(method("GET"))
            .and(path("/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "test-token"
            })))
            .mount(&mock_server)
            .await;
        let client = RegistryClient::<true>::with_basic_auth(
            &registry_url,
            "test-repo",
            "username",
            "password",
            ClientScope::Push,
        )
        .await?;

        assert_eq!(client.registry, registry_url);
        assert_eq!(client.repo, "test-repo");
        Ok(())
    }

    #[test(tokio::test)]
    async fn test_get_tag_for_target() -> Result<()> {
        let mock_server = MockServer::start().await;
        let registry_url = mock_server.uri().replace("http://", "");

        // Mock the index response
        let index_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": TEST_DIGEST,
                    "size": 4,
                    "platform": {
                        "architecture": "amd64",
                        "os": "linux"
                    }
                }
            ]
        });

        Mock::given(method("GET"))
            .and(path("/v2/test-repo/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&index_json))
            .mount(&mock_server)
            .await;

        // Mock the manifest response
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": CONFIG_DIGEST,
                "size": 4
            },
            "layers": []
        });

        Mock::given(method("GET"))
            .and(path(format!("/v2/test-repo/manifests/{}", TEST_DIGEST)))
            .respond_with(ResponseTemplate::new(200).set_body_json(&manifest_json))
            .mount(&mock_server)
            .await;

        // Mock the config response
        let config_json = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        });

        Mock::given(method("GET"))
            .and(path(format!("/v2/test-repo/blobs/{}", CONFIG_DIGEST)))
            .respond_with(ResponseTemplate::new(200).set_body_json(&config_json))
            .mount(&mock_server)
            .await;

        // Create client and test
        let client = RegistryClient::<true> {
            client: reqwest::Client::new(),
            registry: registry_url,
            repo: "test-repo".to_string(),
        };

        let (manifest, config) = client
            .get_tag_for_target("latest", Arch::Amd64, Os::Linux)
            .await?;
        assert_eq!(manifest.config().digest().to_string(), CONFIG_DIGEST);
        assert_eq!(config.architecture(), &Arch::Amd64);
        assert_eq!(config.os(), &Os::Linux);

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_blob_operations() -> Result<()> {
        let mock_server = MockServer::start().await;
        let registry_url = mock_server.uri().replace("http://", "");

        // Mock blob upload initiation
        Mock::given(method("POST"))
            .and(path("/v2/test-repo/blobs/uploads/"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("Location", "/v2/test-repo/blobs/uploads/test-upload"),
            )
            .mount(&mock_server)
            .await;

        // Mock blob upload
        Mock::given(method("PUT"))
            .and(path("/v2/test-repo/blobs/uploads/test-upload"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&mock_server)
            .await;

        // Mock blob existence check
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/test-repo/blobs/{}", TEST_DIGEST)))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = RegistryClient::<true> {
            client: reqwest::Client::new(),
            registry: registry_url,
            repo: "test-repo".to_string(),
        };

        // Test upload
        let digest = Digest::from_str(TEST_DIGEST).unwrap();
        client.upload_blob(&digest, vec![1, 2, 3, 4]).await?;

        // Mock blob download
        Mock::given(method("GET"))
            .and(path(format!("/v2/test-repo/blobs/{}", TEST_DIGEST)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4]))
            .mount(&mock_server)
            .await;

        // Test existence check
        assert!(client.has_blob(&digest).await?);

        // Test download
        let blob = client.get_binary_blob(&digest).await?;
        assert_eq!(blob.as_ref(), &[1, 2, 3, 4]);

        Ok(())
    }
}
