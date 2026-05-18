use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliExitCode {
    Success,
    Failure,
    DoctorFailures(usize),
}

impl From<CliExitCode> for ExitCode {
    fn from(code: CliExitCode) -> Self {
        match code {
            CliExitCode::Success => Self::SUCCESS,
            CliExitCode::Failure => Self::from(1),
            CliExitCode::DoctorFailures(count) => {
                let capped = u8::try_from(count).unwrap_or(u8::MAX);
                Self::from(capped)
            }
        }
    }
}

pub(crate) fn from_i32(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        Err(_) => ExitCode::from(1),
    }
}
