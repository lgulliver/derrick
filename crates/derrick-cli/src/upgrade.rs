//! Upgrade support.

#[allow(dead_code)]
mod github;

use std::cmp::Ordering;

use semver::Version;

use github::ReleaseAsset;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VersionStatus {
    UpgradeAvailable,
    AlreadyCurrent,
    CurrentIsNewer,
}

#[allow(dead_code)]
fn select_asset(assets: &[ReleaseAsset]) -> Option<&ReleaseAsset> {
    select_asset_for_platform(assets, std::env::consts::OS, std::env::consts::ARCH)
}

fn select_asset_for_platform<'a>(
    assets: &'a [ReleaseAsset],
    os: &str,
    arch: &str,
) -> Option<&'a ReleaseAsset> {
    let expected = asset_name(os, arch)?;
    assets.iter().find(|asset| asset.name == expected)
}

fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("derrick-linux-x86_64"),
        ("macos", "aarch64") => Some("derrick-macos-arm64"),
        ("macos", "x86_64") => Some("derrick-macos-x86_64"),
        _ => None,
    }
}

#[allow(dead_code)]
fn current_version_status(tag_name: &str) -> Result<VersionStatus, semver::Error> {
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
            browser_download_url: format!("https://example.invalid/{name}"),
        }
    }

    #[test]
    fn asset_name_maps_supported_platforms() {
        assert_eq!(asset_name("linux", "x86_64"), Some("derrick-linux-x86_64"));
        assert_eq!(asset_name("macos", "aarch64"), Some("derrick-macos-arm64"));
        assert_eq!(asset_name("macos", "x86_64"), Some("derrick-macos-x86_64"));
    }

    #[test]
    fn asset_name_returns_none_for_unknown_platform() {
        assert_eq!(asset_name("freebsd", "x86_64"), None);
        assert_eq!(asset_name("linux", "riscv64"), None);
    }

    #[test]
    fn select_asset_matches_supported_platforms() {
        let assets = [
            asset("derrick-macos-arm64"),
            asset("derrick-macos-x86_64"),
            asset("derrick-linux-x86_64"),
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
    fn select_asset_ignores_checksum_and_signature_siblings() {
        let assets = [
            asset("derrick-linux-x86_64.sha256"),
            asset("derrick-linux-x86_64.sig"),
            asset("derrick-linux-x86_64"),
        ];
        assert_eq!(
            select_asset_for_platform(&assets, "linux", "x86_64"),
            Some(&assets[2])
        );
    }

    #[test]
    fn select_asset_returns_none_for_unknown_platform() {
        let assets = [asset("derrick-linux-x86_64")];
        assert_eq!(
            select_asset_for_platform(&assets, "windows", "x86_64"),
            None
        );
    }

    #[test]
    fn select_asset_uses_current_platform() {
        let Some(name) = asset_name(std::env::consts::OS, std::env::consts::ARCH) else {
            return;
        };
        let assets = [asset(name)];
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
