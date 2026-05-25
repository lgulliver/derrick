//! GitHub release client for derrick upgrades.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

const GITHUB_API: &str = "https://api.github.com";
const REPO: &str = "lgulliver/derrick";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(super) struct ReleaseAsset {
    pub(super) name: String,
    #[serde(rename = "browser_download_url")]
    pub(super) browser_download_url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(super) struct GithubRelease {
    #[serde(rename = "tag_name")]
    pub(super) tag_name: String,
    pub(super) assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Error)]
pub(super) enum ReleaseClientError {
    #[error("failed to build GitHub release HTTP client: {0}")]
    BuildClient(#[source] reqwest::Error),
    #[error("GitHub release request failed: {0}")]
    Request(#[from] reqwest::Error),
}

#[async_trait]
pub(super) trait ReleaseClient: Send + Sync {
    async fn latest_release(&self) -> Result<GithubRelease, ReleaseClientError>;

    async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Vec<u8>, ReleaseClientError>;
}

#[derive(Clone, Debug)]
pub(super) struct ReqwestReleaseClient {
    client: reqwest::Client,
}

impl ReqwestReleaseClient {
    pub(super) fn new() -> Result<Self, ReleaseClientError> {
        Self::with_timeout(DEFAULT_REQUEST_TIMEOUT)
    }

    pub(super) fn with_timeout(timeout: Duration) -> Result<Self, ReleaseClientError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(timeout)
            .build()
            .map_err(ReleaseClientError::BuildClient)?;
        Ok(Self { client })
    }

    pub(super) fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ReleaseClient for ReqwestReleaseClient {
    async fn latest_release(&self) -> Result<GithubRelease, ReleaseClientError> {
        let release = self
            .client
            .get(latest_release_url())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(release)
    }

    async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Vec<u8>, ReleaseClientError> {
        let bytes = self
            .client
            .get(&asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }
}

fn latest_release_url() -> String {
    format!("{GITHUB_API}/repos/{REPO}/releases/latest")
}

fn user_agent() -> String {
    format!("derrick/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_release_url_uses_repo_constant() {
        assert_eq!(
            latest_release_url(),
            "https://api.github.com/repos/lgulliver/derrick/releases/latest"
        );
    }

    #[test]
    fn user_agent_uses_package_version() {
        assert_eq!(
            user_agent(),
            format!("derrick/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn release_deserializes_from_github_shape() {
        let release: GithubRelease = serde_json::from_str(
            r#"{
                "tag_name": "v1.2.3",
                "assets": [
                    {
                        "name": "derrick-aarch64-apple-darwin.tar.gz",
                        "browser_download_url": "https://github.com/lgulliver/derrick/releases/download/v1.2.3/derrick-aarch64-apple-darwin.tar.gz"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(release.tag_name, "v1.2.3");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(
            release.assets[0].name,
            "derrick-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://github.com/lgulliver/derrick/releases/download/v1.2.3/derrick-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn reqwest_client_builds() {
        ReqwestReleaseClient::new().unwrap();
    }
}
