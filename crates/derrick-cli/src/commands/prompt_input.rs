//! Resolving the feature prompt from a string, a file, or stdin.
//!
//! `derrick drill` / `derrick run drill` accept the feature brief three
//! ways so that a large multi-line `/speckit.specify`-style prompt (newlines,
//! quotes, `$`) can be supplied without shell-escaping pain:
//!
//! 1. the positional argument / `--prompt "..."` string,
//! 2. `--prompt-file <path>` (a file to read),
//! 3. stdin (piped, or explicitly via the `-` sentinel).
//!
//! [`resolve_prompt`] folds those three sources into the single
//! `Option<String>` that feeds `state.prompt`.  Exactly one explicit source is
//! allowed; more than one is a usage error.  The "is stdin a terminal" decision
//! and the stdin reader are injected so the resolution rules can be unit-tested
//! without a real TTY.

use std::io::Read;
use std::path::Path;

use crate::CliError;

/// The `-` sentinel that means "read from stdin" when supplied as the prompt
/// string or as the `--prompt-file` value.
const STDIN_SENTINEL: &str = "-";

/// Resolve the feature prompt from the available sources.
///
/// * `prompt` — the positional argument / `--prompt` string (if any).
/// * `prompt_file` — the `--prompt-file` value (a path, or `-` for stdin).
/// * `stdin_is_terminal` — whether stdin is attached to a terminal; inject
///   [`std::io::IsTerminal`] at the call site.
/// * `read_stdin` — a closure that reads stdin to a `String`; injected so tests
///   can supply a fixed reader.
///
/// ## Resolution rules
///
/// * stdin is used when the prompt string is exactly `-`, **or**
///   `--prompt-file -` is given, **or** no prompt string and no `--prompt-file`
///   is given and stdin is not a terminal (piped input).
/// * Supplying more than one explicit source (e.g. a non-`-` positional string
///   *and* `--prompt-file`) is a usage error.
/// * A missing or unreadable `--prompt-file` is an error naming the path.
/// * A single trailing newline is trimmed; a prompt that is empty or
///   whitespace-only after trimming is rejected.
/// * When nothing is supplied and stdin **is** a terminal, returns `Ok(None)`
///   so the caller's interactive no-prompt fallback still fires.
pub(crate) fn resolve_prompt<R>(
    prompt: Option<String>,
    prompt_file: Option<String>,
    stdin_is_terminal: bool,
    read_stdin: R,
) -> Result<Option<String>, CliError>
where
    R: FnOnce() -> std::io::Result<String>,
{
    // Classify the prompt string: a literal `-` is a stdin request, not text.
    let prompt_is_stdin = prompt.as_deref() == Some(STDIN_SENTINEL);
    let prompt_text = if prompt_is_stdin { None } else { prompt };

    // Classify `--prompt-file`: `-` is a stdin request, not a path.
    let file_is_stdin = prompt_file.as_deref() == Some(STDIN_SENTINEL);
    let file_path = if file_is_stdin { None } else { prompt_file };

    let wants_stdin = prompt_is_stdin || file_is_stdin;

    // Count explicit sources; more than one is ambiguous.
    let explicit_sources = usize::from(prompt_text.is_some())
        + usize::from(file_path.is_some())
        + usize::from(wants_stdin);
    if explicit_sources > 1 {
        return Err(crate::message(
            "provide the prompt as a positional argument, --prompt-file, or stdin \
             — not more than one",
        ));
    }

    let raw = if let Some(text) = prompt_text {
        text
    } else if let Some(path) = file_path {
        read_prompt_file(Path::new(&path))?
    } else if wants_stdin {
        read_stdin().map_err(|source| {
            crate::message(format!("failed to read the prompt from stdin: {source}"))
        })?
    } else if stdin_is_terminal {
        // Nothing supplied and stdin is interactive: preserve the caller's
        // no-prompt fallback (e.g. incomplete-run detection).
        return Ok(None);
    } else {
        // Nothing supplied but stdin is piped: read it.
        read_stdin().map_err(|source| {
            crate::message(format!("failed to read the prompt from stdin: {source}"))
        })?
    };

    Ok(Some(normalise(raw)?))
}

/// Read a prompt file, mapping any IO failure to an error naming the path.
fn read_prompt_file(path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Trim a single trailing newline and reject an empty/whitespace-only prompt.
fn normalise(raw: String) -> Result<String, CliError> {
    let trimmed = raw
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(&raw);
    if trimmed.trim().is_empty() {
        return Err(crate::message(
            "the feature prompt is empty — supply some text via the argument, \
             --prompt-file, or stdin",
        ));
    }
    Ok(trimmed.to_owned())
}

/// Convenience wrapper used by command handlers: wires the real stdin reader and
/// the live [`std::io::IsTerminal`] check into [`resolve_prompt`].
pub(crate) fn resolve_prompt_from_env(
    prompt: Option<String>,
    prompt_file: Option<String>,
) -> Result<Option<String>, CliError> {
    use std::io::IsTerminal;
    let stdin_is_terminal = std::io::stdin().is_terminal();
    resolve_prompt(prompt, prompt_file, stdin_is_terminal, || {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdin reader that never panics; tests that don't expect a stdin read
    /// pass this and assert it isn't invoked by checking the resolved source.
    fn no_stdin() -> std::io::Result<String> {
        panic!("stdin should not be read in this case");
    }

    #[test]
    fn positional_only_is_returned_verbatim() {
        let result =
            resolve_prompt(Some("build a webhook".to_owned()), None, true, no_stdin).unwrap();
        assert_eq!(result.as_deref(), Some("build a webhook"));
    }

    #[test]
    fn prompt_file_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("brief.md");
        std::fs::write(&path, "multi\nline\nbrief\n").unwrap();

        let result = resolve_prompt(
            None,
            Some(path.to_string_lossy().into_owned()),
            true,
            no_stdin,
        )
        .unwrap();
        // The single trailing newline is trimmed; interior newlines are kept.
        assert_eq!(result.as_deref(), Some("multi\nline\nbrief"));
    }

    #[test]
    fn stdin_read_via_injected_reader_when_piped() {
        // No prompt, no file, stdin is NOT a terminal -> read stdin.
        let result = resolve_prompt(None, None, false, || Ok("from stdin\n".to_owned())).unwrap();
        assert_eq!(result.as_deref(), Some("from stdin"));
    }

    #[test]
    fn dash_prompt_sentinel_reads_stdin() {
        let result = resolve_prompt(
            Some("-".to_owned()),
            None,
            true, // even with a terminal, an explicit `-` forces a read
            || Ok("piped brief".to_owned()),
        )
        .unwrap();
        assert_eq!(result.as_deref(), Some("piped brief"));
    }

    #[test]
    fn dash_prompt_file_sentinel_reads_stdin() {
        let result = resolve_prompt(None, Some("-".to_owned()), true, || {
            Ok("piped via file flag".to_owned())
        })
        .unwrap();
        assert_eq!(result.as_deref(), Some("piped via file flag"));
    }

    #[test]
    fn conflicting_sources_error() {
        let err = resolve_prompt(
            Some("inline".to_owned()),
            Some("/tmp/brief.md".to_owned()),
            true,
            no_stdin,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not more than one"),
            "expected conflict error, got: {err}"
        );
    }

    #[test]
    fn missing_file_errors_with_path() {
        let err =
            resolve_prompt(None, Some("/no/such/brief.md".to_owned()), true, no_stdin).unwrap_err();
        assert!(
            err.to_string().contains("/no/such/brief.md"),
            "error should name the path, got: {err}"
        );
    }

    #[test]
    fn empty_after_trim_errors() {
        let err = resolve_prompt(None, None, false, || Ok("   \n".to_owned())).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected empty-prompt error, got: {err}"
        );
    }

    #[test]
    fn terminal_and_nothing_returns_none() {
        // Interactive shell, no prompt supplied: caller's fallback should fire.
        let result = resolve_prompt(None, None, true, no_stdin).unwrap();
        assert_eq!(result, None);
    }
}
