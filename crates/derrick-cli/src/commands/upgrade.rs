//! `derrick upgrade` — binary self-update plumbing.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::{NamedTempFile, TempPath};

use crate::commands::UpgradeArgs;
use crate::exit_code::CliExitCode;
use crate::{message, CliError};

pub(crate) async fn execute(args: UpgradeArgs) -> Result<CliExitCode, CliError> {
    let _ = (args.check, args.force);

    let Some(url) = args.url else {
        println!("upgrade not yet available, re-run the install script");
        return Ok(CliExitCode::Success);
    };

    let mut response = reqwest::get(&url).await.map_err(|source| {
        message(format!(
            "failed to download replacement binary from {url}: {source}"
        ))
    })?;
    if !response.status().is_success() {
        return Err(message(format!(
            "failed to download replacement binary from {url}: HTTP {}",
            response.status()
        )));
    }

    let mut replacement = BinaryReplacement::for_current_exe()?;
    while let Some(chunk) = response.chunk().await.map_err(|source| {
        message(format!(
            "failed while downloading replacement binary from {url}: {source}"
        ))
    })? {
        replacement.write_chunk(&chunk)?;
    }

    let outcome = replacement.commit()?;
    if let Some(note) = outcome.manual_cleanup_note() {
        println!("{note}");
    }
    println!("upgraded {}", outcome.target.display());
    Ok(CliExitCode::Success)
}

struct BinaryReplacement {
    target: PathBuf,
    temp: NamedTempFile,
}

impl BinaryReplacement {
    fn for_current_exe() -> Result<Self, CliError> {
        let target = std::env::current_exe()
            .map_err(|source| message(format!("failed to resolve current executable: {source}")))?;
        Self::for_target(&target)
    }

    fn for_target(target: &Path) -> Result<Self, CliError> {
        let parent = executable_directory(target)?;
        let temp = NamedTempFile::new_in(parent).map_err(|source| CliError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        Ok(Self {
            target: target.to_path_buf(),
            temp,
        })
    }

    #[cfg(test)]
    fn copy_from(&mut self, mut bytes: impl io::Read) -> Result<(), CliError> {
        io::copy(&mut bytes, self.temp.as_file_mut()).map_err(|source| {
            message(format!(
                "failed to write replacement binary for {}: {source}",
                self.target.display()
            ))
        })?;
        Ok(())
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        self.temp.as_file_mut().write_all(chunk).map_err(|source| {
            message(format!(
                "failed to write replacement binary for {}: {source}",
                self.target.display()
            ))
        })
    }

    fn commit(mut self) -> Result<ReplacementOutcome, CliError> {
        self.temp.as_file_mut().flush().map_err(|source| {
            message(format!(
                "failed to flush replacement binary for {}: {source}",
                self.target.display()
            ))
        })?;

        inherit_target_permissions(self.temp.path(), &self.target)?;
        mark_executable(self.temp.path())?;
        let temp_path = self.temp.into_temp_path();
        install_temp_path(temp_path, &self.target)
    }
}

struct ReplacementOutcome {
    target: PathBuf,
    #[cfg(target_os = "windows")]
    backup_path: PathBuf,
}

impl ReplacementOutcome {
    fn manual_cleanup_note(&self) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            return Some(format!(
                "note: previous binary moved to {}; delete it manually after confirming the upgrade",
                self.backup_path.display()
            ));
        }

        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }
}

fn executable_directory(target: &Path) -> Result<&Path, CliError> {
    target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            message(format!(
                "failed to locate executable directory for {}",
                target.display()
            ))
        })
}

fn inherit_target_permissions(temp: &Path, target: &Path) -> Result<(), CliError> {
    let metadata = match fs::metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(CliError::Io {
                path: target.to_path_buf(),
                source,
            });
        }
    };
    fs::set_permissions(temp, metadata.permissions()).map_err(|source| CliError::Io {
        path: temp.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|source| CliError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> Result<(), CliError> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn install_temp_path(temp_path: TempPath, target: &Path) -> Result<ReplacementOutcome, CliError> {
    rename_with_help(temp_path.as_ref(), target)?;
    Ok(ReplacementOutcome {
        target: target.to_path_buf(),
    })
}

#[cfg(target_os = "windows")]
fn install_temp_path(temp_path: TempPath, target: &Path) -> Result<ReplacementOutcome, CliError> {
    let backup_path = windows_backup_path(target);
    if backup_path.exists() {
        fs::remove_file(&backup_path).map_err(|source| CliError::Io {
            path: backup_path.clone(),
            source,
        })?;
    }

    rename_with_help(target, &backup_path)?;
    if let Err(error) = rename_with_help(temp_path.as_ref(), target) {
        let restore_result = fs::rename(&backup_path, target);
        if let Err(restore_error) = restore_result {
            return Err(message(format!(
                "{error}; additionally failed to restore {} from {}: {restore_error}",
                target.display(),
                backup_path.display()
            )));
        }
        return Err(error);
    }

    Ok(ReplacementOutcome {
        target: target.to_path_buf(),
        backup_path,
    })
}

#[cfg(target_os = "windows")]
fn windows_backup_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_os_string();
    path.push(".old");
    PathBuf::from(path)
}

fn rename_with_help(from: &Path, to: &Path) -> Result<(), CliError> {
    fs::rename(from, to).map_err(|source| rename_error(to, source))
}

fn rename_error(target: &Path, source: io::Error) -> CliError {
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

    CliError::Io {
        path: target.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::io::{Cursor, Read};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[test]
    #[cfg(unix)]
    fn upgrade_success_replaces_binary_and_sets_executable_bit() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let target = dir.path().join("derrick");
        fs::write(&target, b"old-binary")?;

        let mut replacement = BinaryReplacement::for_target(&target)?;
        replacement.copy_from(Cursor::new(b"new-binary"))?;
        let outcome = replacement.commit()?;

        assert_eq!(fs::read(&target)?, b"new-binary");
        assert_eq!(outcome.target, target);
        assert!(fs::metadata(&target)?.permissions().mode() & 0o111 != 0);
        assert_eq!(
            directory_entries(dir.path())?,
            vec![OsString::from("derrick")]
        );

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn upgrade_preserves_target_permissions() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let target = dir.path().join("derrick");
        fs::write(&target, b"old-binary")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;

        let mut replacement = BinaryReplacement::for_target(&target)?;
        replacement.copy_from(Cursor::new(b"new-binary"))?;
        replacement.commit()?;

        let mode = fs::metadata(&target)?.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "expected target perms + execute bits, got {mode:o}"
        );

        Ok(())
    }

    #[test]
    fn upgrade_mid_download_failure_leaves_original_untouched() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("derrick");
        fs::write(&target, b"old-binary")?;

        let mut replacement = BinaryReplacement::for_target(&target)?;
        let error = replacement
            .copy_from(FailingReader { emitted: false })
            .err()
            .ok_or_else(|| io::Error::other("expected failing reader error"))?;
        drop(replacement);

        assert!(
            error
                .to_string()
                .contains("failed to write replacement binary"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(&target)?, b"old-binary");
        assert_eq!(
            directory_entries(dir.path())?,
            vec![OsString::from("derrick")]
        );

        Ok(())
    }

    #[test]
    fn upgrade_permission_denied_rename_error_is_helpful() -> TestResult {
        let target = PathBuf::from("/usr/local/bin/derrick");
        let error = rename_error(
            &target,
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        let message = error.to_string();

        assert!(message.contains("permission denied replacing /usr/local/bin/derrick"));
        assert!(message.contains("permission to write /usr/local/bin"));
        assert!(message.contains("install script"));

        Ok(())
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn upgrade_windows_moves_original_to_old_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("derrick.exe");
        fs::write(&target, b"old-binary")?;

        let mut replacement = BinaryReplacement::for_target(&target)?;
        replacement.copy_from(Cursor::new(b"new-binary"))?;
        let outcome = replacement.commit()?;
        let backup = dir.path().join("derrick.exe.old");

        assert_eq!(fs::read(&target)?, b"new-binary");
        assert_eq!(fs::read(&backup)?, b"old-binary");
        assert_eq!(outcome.backup_path, backup);
        assert!(outcome
            .manual_cleanup_note()
            .ok_or_else(|| io::Error::other("missing cleanup note"))?
            .contains("delete it manually"));

        Ok(())
    }

    fn directory_entries(dir: &Path) -> TestResult<Vec<OsString>> {
        let mut entries = fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        Ok(entries)
    }

    struct FailingReader {
        emitted: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.emitted {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "download interrupted",
                ));
            }

            let bytes = b"partial-new-binary";
            let len = bytes.len().min(buf.len());
            buf[..len].copy_from_slice(&bytes[..len]);
            self.emitted = true;
            Ok(len)
        }
    }
}
