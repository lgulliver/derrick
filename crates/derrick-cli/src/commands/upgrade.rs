use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use tempfile::NamedTempFile;

use crate::commands::UpgradeArgs;
use crate::exit_code::CliExitCode;
use crate::message;
use crate::upgrade::github::{GithubRelease, ReleaseAsset, ReleaseClient, ReqwestReleaseClient};

/// Executes the `derrick upgrade` subcommand (downloads and installs a new derrick release).
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
    let written = download_and_replace(target, client, asset).await?;
    reporter.line(&format!("downloaded {written} bytes"));
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

async fn download_and_replace(
    target: &Path,
    client: &dyn ReleaseClient,
    asset: &ReleaseAsset,
) -> Result<u64, crate::CliError> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| message(format!("cannot determine parent for {}", target.display())))?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| crate::CliError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let written = {
        use std::io::Write as _;
        let file = tmp.as_file_mut();
        let total = client
            .download_asset(asset, file)
            .await
            .map_err(|error| message(error.to_string()))?;
        file.flush().map_err(|source| crate::CliError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;
        total
    };

    apply_executable_permissions(tmp.path(), target)?;
    let temp_path = tmp.into_temp_path();
    let kept = temp_path.keep().map_err(|error| crate::CliError::Io {
        path: error.path.to_path_buf(),
        source: error.error,
    })?;
    if let Err(error) = rename_with_help(&kept, target) {
        let _ = fs::remove_file(&kept);
        return Err(error);
    }
    Ok(written)
}

#[cfg(unix)]
fn apply_executable_permissions(temp: &Path, target: &Path) -> Result<(), crate::CliError> {
    use std::os::unix::fs::PermissionsExt as _;

    // Inherit the target's existing mode (preserving any deliberate
    // restrictions like 0o700) and ensure the execute bits are set so the
    // replacement is invocable.
    let mode = match fs::metadata(target) {
        Ok(metadata) => metadata.permissions().mode(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0o755,
        Err(source) => {
            return Err(crate::CliError::Io {
                path: target.to_path_buf(),
                source,
            });
        }
    };
    let mut permissions = fs::metadata(temp)
        .map_err(|source| crate::CliError::Io {
            path: temp.to_path_buf(),
            source,
        })?
        .permissions();
    permissions.set_mode((mode & 0o777) | 0o111);
    fs::set_permissions(temp, permissions).map_err(|source| crate::CliError::Io {
        path: temp.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn apply_executable_permissions(_temp: &Path, _target: &Path) -> Result<(), crate::CliError> {
    Ok(())
}

fn rename_with_help(from: &Path, to: &Path) -> Result<(), crate::CliError> {
    fs::rename(from, to).map_err(|source| rename_error(to, source))
}

fn rename_error(target: &Path, source: io::Error) -> crate::CliError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        let location = target.parent().map_or_else(
            || target.display().to_string(),
            |parent| parent.display().to_string(),
        );
        return message(format!(
            "permission denied replacing {}; rerun with permission to write {location} or reinstall with the install script",
            target.display()
        ));
    }
    crate::CliError::Io {
        path: target.to_path_buf(),
        source,
    }
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
            writer: &mut (dyn std::io::Write + Send),
        ) -> Result<u64, ReleaseClientError> {
            self.downloaded_assets
                .lock()
                .unwrap()
                .push(asset.name.clone());
            writer
                .write_all(&self.bytes)
                .map_err(ReleaseClientError::Write)?;
            Ok(self.bytes.len() as u64)
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

    #[tokio::test]
    #[cfg(unix)]
    async fn upgrade_preserves_target_permissions_and_adds_execute_bits() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, target) = target_file(b"old")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        let client = FakeReleaseClient::new("v999.0.0", b"new")?;
        let reporter = CaptureReporter::default();

        run_upgrade_with(&args(false, false), &client, &target, &reporter).await?;

        let mode = fs::metadata(&target)?.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o711,
            "expected inherited 0o600 plus execute bits, got {mode:o}"
        );
        Ok(())
    }

    #[test]
    fn rename_error_permission_denied_is_actionable() {
        let target = PathBuf::from("/usr/local/bin/derrick");
        let error = rename_error(
            &target,
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        let text = error.to_string();
        assert!(text.contains("permission denied replacing /usr/local/bin/derrick"));
        assert!(text.contains("permission to write /usr/local/bin"));
        assert!(text.contains("install script"));
    }

    #[test]
    fn upgrade_available_exit_code_is_distinct_from_failure() {
        use std::process::ExitCode;
        // Just confirm the typed enum maps to a non-1 process exit code so
        // callers can disambiguate "upgrade available" from a generic failure.
        let upgrade: ExitCode = CliExitCode::UpgradeAvailable.into();
        let failure: ExitCode = CliExitCode::Failure.into();
        // ExitCode lacks PartialEq, compare via Debug formatting.
        assert_ne!(format!("{upgrade:?}"), format!("{failure:?}"));
    }
}
