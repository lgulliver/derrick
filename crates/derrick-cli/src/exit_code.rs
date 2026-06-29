use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliExitCode {
    Success,
    Failure,
    UpgradeAvailable,
    /// Refused for a user-facing policy reason (e.g. mode guard); the
    /// command printed an explanatory message to stderr.
    Refused,
    /// Code review found issues; the calling agent should remediate and retry.
    ReviewIssues,
    DoctorFailures(usize),
}

impl From<CliExitCode> for ExitCode {
    fn from(code: CliExitCode) -> Self {
        match code {
            CliExitCode::Success => Self::SUCCESS,
            CliExitCode::Failure => Self::from(1),
            CliExitCode::Refused => Self::from(2),
            CliExitCode::ReviewIssues => Self::from(3),
            // Dedicated code so `upgrade --check` callers can distinguish
            // "upgrade available" from generic command failure (1).
            CliExitCode::UpgradeAvailable => Self::from(4),
            CliExitCode::DoctorFailures(count) => {
                let capped = u8::try_from(count).unwrap_or(u8::MAX);
                Self::from(capped)
            }
        }
    }
}

/// Converts an i32 exit code to [`ExitCode`], clamping out-of-range values to 1.
pub(crate) fn from_i32(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        Err(_) => ExitCode::from(1),
    }
}
