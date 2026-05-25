use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::commands::UpgradeArgs;
use crate::exit_code::CliExitCode;
use crate::message;
use crate::upgrade::github::{GithubRelease, ReleaseAsset, ReleaseClient, ReqwestReleaseClient};

pub(crate) async fn execute(args: UpgradeArgs) -> Result<CliExitCode, crate::CliError> {
    let client = ReqwestReleaseClient::new().map_err(|error| message(error.to_string()))?;
    run_upgrade(&args, &client).await
}

async fn run_upgrade(
    args: &UpgradeArgs,
    client: &dyn ReleaseClient,
) -> Result<CliExitCode, crate::CliError> {
    let target = current_exe_path()?;
    let reporter = StdoutUpgradeReporter;
    run_upgrade_with(args, client, &target, &reporter).await
}

async fn run_upgrade_with(
    args: &UpgradeArgs,
    client: &dyn ReleaseClient,
    target: &Path,
    reporter: &dyn UpgradeReporter,
) -> Result<CliExitCode, crate::CliError> {
    let release = client
        .latest_release()
        .await
        .map_err(|error| message(error.to_string()))?;
    let current = current_version()?;
    let latest = release_version(&release)?;
    let upgrade_available = latest > current;

    if args.check {
        if upgrade_available {
            reporter.line(&format!(
                "derrick {latest} is available (current {current})"
            ));
            return Ok(CliExitCode::UpgradeAvailable);
        }
        reporter.line(&format!(
            "derrick is already up to date (current {current}, latest {latest})"
        ));
        return Ok(CliExitCode::Success);
    }

    if !upgrade_available && !args.force {
        reporter.line(&format!(
            "derrick is already up to date (current {current}, latest {latest})"
        ));
        return Ok(CliExitCode::Success);
    }

    if !upgrade_available {
        reporter.line(&format!("forcing upgrade to {latest} (current {current})"));
    } else {
        reporter.line(&format!("upgrading derrick from {current} to {latest}"));
    }

    let asset = select_asset(&release)?;
    reporter.line(&format!("downloading {}", asset.name));
    let bytes = client
        .download_asset(asset)
        .await
        .map_err(|error| message(error.to_string()))?;
    reporter.line(&format!("downloaded {} bytes", bytes.len()));

    replace_binary(target, &bytes)?;
    reporter.line(&format!("upgraded derrick to {latest}"));
    Ok(CliExitCode::Success)
}

fn current_exe_path() -> Result<PathBuf, crate::CliError> {
    let path = std::env::current_exe().map_err(|source| crate::CliError::Io {
        path: PathBuf::from("derrick"),
        source,
    })?;
    fs::canonicalize(&path).map_err(|source| crate::CliError::Io { path, source })
}

fn current_version() -> Result<Version, crate::CliError> {
    parse_version(env!("CARGO_PKG_VERSION"))
}

fn release_version(release: &GithubRelease) -> Result<Version, crate::CliError> {
    parse_version(&release.tag_name)
}

fn parse_version(raw: &str) -> Result<Version, crate::CliError> {
    let trimmed = raw.trim().trim_start_matches('v');
    Version::parse(trimmed)
        .map_err(|error| message(format!("invalid release version {raw:?}: {error}")))
}

fn select_asset(release: &GithubRelease) -> Result<&ReleaseAsset, crate::CliError> {
    let expected = expected_asset_name()?;
    release
        .assets
        .iter()
        .find(|asset| asset.name == expected)
        .ok_or_else(|| {
            message(format!(
                "release {} does not include asset {expected}",
                release.tag_name
            ))
        })
}

fn expected_asset_name() -> Result<&'static str, crate::CliError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("derrick-linux-x86_64"),
        ("macos", "aarch64") => Ok("derrick-macos-arm64"),
        ("macos", "x86_64") => Ok("derrick-macos-x86_64"),
        (os, arch) => Err(message(format!(
            "unsupported upgrade platform {os}-{arch}; reinstall with scripts/install.sh"
        ))),
    }
}

fn replace_binary(target: &Path, bytes: &[u8]) -> Result<(), crate::CliError> {
    let parent = target
        .parent()
        .ok_or_else(|| message(format!("cannot determine parent for {}", target.display())))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| {
            message(format!(
                "cannot determine file name for {}",
                target.display()
            ))
        })?
        .to_string_lossy();
    let tmp = parent.join(format!(".{file_name}.upgrade-{}.tmp", std::process::id()));
    let cleanup = TempFileCleanup::new(tmp.clone());

    fs::write(&tmp, bytes).map_err(|source| crate::CliError::Io {
        path: tmp.clone(),
        source,
    })?;
    make_executable(&tmp)?;
    fs::rename(&tmp, target).map_err(|source| crate::CliError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    cleanup.persist();
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), crate::CliError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut permissions = fs::metadata(path)
        .map_err(|source| crate::CliError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|source| crate::CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), crate::CliError> {
    Ok(())
}

trait UpgradeReporter {
    fn line(&self, text: &str);
}

struct StdoutUpgradeReporter;

impl UpgradeReporter for StdoutUpgradeReporter {
    fn line(&self, text: &str) {
        println!("{text}");
    }
}

struct TempFileCleanup {
    path: PathBuf,
    keep: std::cell::Cell<bool>,
}

impl TempFileCleanup {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            keep: std::cell::Cell::new(false),
        }
    }

    fn persist(&self) {
        self.keep.set(true);
    }
}

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        if !self.keep.get() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::upgrade::github::ReleaseClientError;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Default)]
    struct CaptureReporter {
        lines: Mutex<Vec<String>>,
    }

    impl CaptureReporter {
        fn text(&self) -> String {
            self.lines.lock().unwrap().join("\n")
        }
    }

    impl UpgradeReporter for CaptureReporter {
        fn line(&self, text: &str) {
            self.lines.lock().unwrap().push(text.to_owned());
        }
    }

    struct FakeReleaseClient {
        release: GithubRelease,
        bytes: Vec<u8>,
        latest_calls: Mutex<usize>,
        downloaded_assets: Mutex<Vec<String>>,
    }

    impl FakeReleaseClient {
        fn new(tag_name: &str, bytes: &[u8]) -> TestResult<Self> {
            Ok(Self {
                release: GithubRelease {
                    tag_name: tag_name.to_owned(),
                    assets: vec![ReleaseAsset {
                        name: expected_asset_name()?.to_owned(),
                        browser_download_url: format!(
                            "https://github.com/lgulliver/derrick/releases/download/{tag_name}/{}",
                            expected_asset_name()?
                        ),
                    }],
                },
                bytes: bytes.to_vec(),
                latest_calls: Mutex::new(0),
                downloaded_assets: Mutex::new(Vec::new()),
            })
        }

        fn downloaded_assets(&self) -> Vec<String> {
            self.downloaded_assets.lock().unwrap().clone()
        }

        fn latest_calls(&self) -> usize {
            *self.latest_calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl ReleaseClient for FakeReleaseClient {
        async fn latest_release(&self) -> Result<GithubRelease, ReleaseClientError> {
            *self.latest_calls.lock().unwrap() += 1;
            Ok(self.release.clone())
        }

        async fn download_asset(
            &self,
            asset: &ReleaseAsset,
        ) -> Result<Vec<u8>, ReleaseClientError> {
            self.downloaded_assets
                .lock()
                .unwrap()
                .push(asset.name.clone());
            Ok(self.bytes.clone())
        }
    }

    fn args(check: bool, force: bool) -> UpgradeArgs {
        UpgradeArgs { check, force }
    }

    fn target_file(initial: &[u8]) -> TestResult<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("derrick");
        fs::write(&target, initial)?;
        Ok((dir, target))
    }

    #[tokio::test]
    async fn normal_upgrade_replaces_binary_and_reports_success() -> TestResult {
        let (_dir, target) = target_file(b"old")?;
        let client = FakeReleaseClient::new("v999.0.0", b"new")?;
        let reporter = CaptureReporter::default();

        let code = run_upgrade_with(&args(false, false), &client, &target, &reporter).await?;

        assert_eq!(code, CliExitCode::Success);
        assert_eq!(client.latest_calls(), 1);
        assert_eq!(client.downloaded_assets(), vec![expected_asset_name()?]);
        assert_eq!(fs::read(&target)?, b"new");
        let output = reporter.text();
        assert!(output.contains("upgrading derrick from"));
        assert!(output.contains("downloaded 3 bytes"));
        assert!(output.contains("upgraded derrick to 999.0.0"));
        Ok(())
    }

    #[tokio::test]
    async fn already_current_exits_success_without_download() -> TestResult {
        let (_dir, target) = target_file(b"old")?;
        let client = FakeReleaseClient::new(env!("CARGO_PKG_VERSION"), b"new")?;
        let reporter = CaptureReporter::default();

        let code = run_upgrade_with(&args(false, false), &client, &target, &reporter).await?;

        assert_eq!(code, CliExitCode::Success);
        assert_eq!(client.latest_calls(), 1);
        assert!(client.downloaded_assets().is_empty());
        assert_eq!(fs::read(&target)?, b"old");
        assert!(reporter.text().contains("derrick is already up to date"));
        Ok(())
    }

    #[tokio::test]
    async fn check_reports_available_and_exits_before_download() -> TestResult {
        let (_dir, target) = target_file(b"old")?;
        let client = FakeReleaseClient::new("v999.0.0", b"new")?;
        let reporter = CaptureReporter::default();

        let code = run_upgrade_with(&args(true, false), &client, &target, &reporter).await?;

        assert_eq!(code, CliExitCode::UpgradeAvailable);
        assert_eq!(client.latest_calls(), 1);
        assert!(client.downloaded_assets().is_empty());
        assert_eq!(fs::read(&target)?, b"old");
        assert!(reporter.text().contains("derrick 999.0.0 is available"));
        Ok(())
    }

    #[tokio::test]
    async fn force_bypasses_current_version_and_replaces_binary() -> TestResult {
        let (_dir, target) = target_file(b"old")?;
        let client = FakeReleaseClient::new(env!("CARGO_PKG_VERSION"), b"forced")?;
        let reporter = CaptureReporter::default();

        let code = run_upgrade_with(&args(false, true), &client, &target, &reporter).await?;

        assert_eq!(code, CliExitCode::Success);
        assert_eq!(client.latest_calls(), 1);
        assert_eq!(client.downloaded_assets(), vec![expected_asset_name()?]);
        assert_eq!(fs::read(&target)?, b"forced");
        assert!(reporter.text().contains("forcing upgrade to"));
        Ok(())
    }
}
