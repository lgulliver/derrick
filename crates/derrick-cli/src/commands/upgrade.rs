#![allow(dead_code)]

use std::cmp::Ordering;

use semver::Version;

use crate::commands::UpgradeArgs;
use crate::exit_code::CliExitCode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VersionStatus {
    UpgradeAvailable,
    AlreadyCurrent,
    CurrentIsNewer,
}

pub(crate) async fn execute(args: UpgradeArgs) -> Result<CliExitCode, crate::CliError> {
    let _ = (args.check, args.force);
    println!("upgrade not yet available, re-run the install script");
    Ok(CliExitCode::Success)
}

#[allow(clippy::needless_lifetimes)]
pub(crate) fn select_asset<'a>(assets: &'a [ReleaseAsset]) -> Option<&'a ReleaseAsset> {
    select_asset_for_platform(assets, std::env::consts::OS, std::env::consts::ARCH)
}

fn select_asset_for_platform<'a>(
    assets: &'a [ReleaseAsset],
    os: &str,
    arch: &str,
) -> Option<&'a ReleaseAsset> {
    let target = target_triple(os, arch)?;
    assets.iter().find(|asset| asset.name.contains(target))
}

fn target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

pub(crate) fn current_version_status(tag_name: &str) -> Result<VersionStatus, semver::Error> {
    compare_versions(env!("CARGO_PKG_VERSION"), tag_name)
}

fn compare_versions(
    current_version: &str,
    release_tag_name: &str,
) -> Result<VersionStatus, semver::Error> {
    let current = Version::parse(current_version)?;
    let release = Version::parse(
        release_tag_name
            .strip_prefix('v')
            .unwrap_or(release_tag_name),
    )?;

    Ok(match release.cmp(&current) {
        Ordering::Greater => VersionStatus::UpgradeAvailable,
        Ordering::Equal => VersionStatus::AlreadyCurrent,
        Ordering::Less => VersionStatus::CurrentIsNewer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
        }
    }

    #[test]
    fn target_triple_maps_supported_platforms() {
        assert_eq!(
            target_triple("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            target_triple("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            target_triple("macos", "x86_64"),
            Some("x86_64-apple-darwin")
        );
    }

    #[test]
    fn target_triple_returns_none_for_unknown_platform() {
        assert_eq!(target_triple("freebsd", "x86_64"), None);
        assert_eq!(target_triple("linux", "riscv64"), None);
    }

    #[test]
    fn select_asset_matches_supported_platforms() {
        let assets = [
            asset("derrick-aarch64-apple-darwin.tar.gz"),
            asset("derrick-x86_64-apple-darwin.tar.gz"),
            asset("derrick-x86_64-unknown-linux-gnu.tar.gz"),
        ];

        assert_eq!(
            select_asset_for_platform(&assets, "linux", "x86_64"),
            Some(&assets[2])
        );
        assert_eq!(
            select_asset_for_platform(&assets, "macos", "aarch64"),
            Some(&assets[0])
        );
        assert_eq!(
            select_asset_for_platform(&assets, "macos", "x86_64"),
            Some(&assets[1])
        );
    }

    #[test]
    fn select_asset_returns_none_for_unknown_platform() {
        let assets = [asset("derrick-x86_64-unknown-linux-gnu.tar.gz")];
        assert_eq!(
            select_asset_for_platform(&assets, "windows", "x86_64"),
            None
        );
    }

    #[test]
    fn select_asset_uses_current_platform() {
        let Some(target) = target_triple(std::env::consts::OS, std::env::consts::ARCH) else {
            return;
        };
        let assets = [asset(&format!("derrick-{target}.tar.gz"))];
        assert_eq!(select_asset(&assets), Some(&assets[0]));
    }

    #[test]
    fn version_status_reports_upgrade_available_for_newer_release() {
        assert_eq!(
            compare_versions("1.0.0", "v1.0.1").expect("valid semver"),
            VersionStatus::UpgradeAvailable
        );
    }

    #[test]
    fn version_status_reports_already_current_for_equal_release() {
        assert_eq!(
            compare_versions("1.0.0", "v1.0.0").expect("valid semver"),
            VersionStatus::AlreadyCurrent
        );
    }

    #[test]
    fn version_status_reports_current_newer_for_older_release() {
        assert_eq!(
            compare_versions("1.0.1", "v1.0.0").expect("valid semver"),
            VersionStatus::CurrentIsNewer
        );
    }

    #[test]
    fn version_status_treats_prerelease_as_less_than_stable() {
        assert_eq!(
            compare_versions("1.0.0-alpha.1", "v1.0.0").expect("valid semver"),
            VersionStatus::UpgradeAvailable
        );
        assert_eq!(
            compare_versions("1.0.0", "v1.0.0-alpha.1").expect("valid semver"),
            VersionStatus::CurrentIsNewer
        );
    }

    #[test]
    fn current_version_status_parses_cargo_package_version() {
        assert_eq!(
            current_version_status(env!("CARGO_PKG_VERSION")).expect("valid package version"),
            VersionStatus::AlreadyCurrent
        );
    }
}
