//! GitHub release client for derrick upgrades.

use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

const GITHUB_API: &str = "https://api.github.com";
const REPO: &str = "lgulliver/derrick";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: String,
    #[serde(rename = "browser_download_url")]
    pub(crate) browser_download_url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct GithubRelease {
    #[serde(rename = "tag_name")]
    pub(crate) tag_name: String,
    pub(crate) assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Error)]
pub(crate) enum ReleaseClientError {
    #[error("failed to build GitHub release HTTP client: {0}")]
    BuildClient(#[source] reqwest::Error),
    #[error("GitHub release request failed: {0}")]
    Request(#[from] reqwest::Error),
}

#[async_trait::async_trait]
pub(crate) trait ReleaseClient: Send + Sync {
    async fn latest_release(&self) -> Result<GithubRelease, ReleaseClientError>;

    async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Vec<u8>, ReleaseClientError>;
}

#[derive(Clone, Debug)]
pub(crate) struct ReqwestReleaseClient {
    client: reqwest::Client,
}

impl ReqwestReleaseClient {
    pub(crate) fn new() -> Result<Self, ReleaseClientError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()
            .map_err(ReleaseClientError::BuildClient)?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
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
                        "name": "derrick-macos-arm64",
                        "browser_download_url": "https://github.com/lgulliver/derrick/releases/download/v1.2.3/derrick-macos-arm64"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(release.tag_name, "v1.2.3");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "derrick-macos-arm64");
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://github.com/lgulliver/derrick/releases/download/v1.2.3/derrick-macos-arm64"
        );
    }

    #[test]
    fn reqwest_client_builds() {
        ReqwestReleaseClient::new().unwrap();
    }
}
