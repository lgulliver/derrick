use derrick_scrub::Scrubber;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Case {
    tool: String,
    input: PathBuf,
    expected: PathBuf,
}

fn corpus_cases() -> std::io::Result<Vec<Case>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut cases = Vec::new();
    for tool_entry in fs::read_dir(root)? {
        let tool_entry = tool_entry?;
        if !tool_entry.file_type()?.is_dir() {
            continue;
        }
        let tool = tool_entry.file_name().to_string_lossy().into_owned();
        for case_entry in fs::read_dir(tool_entry.path())? {
            let case_entry = case_entry?;
            let input = case_entry.path();
            if input.extension().and_then(|ext| ext.to_str()) != Some("in") {
                continue;
            }
            let expected = input.with_extension("out");
            cases.push(Case {
                tool: tool.clone(),
                input,
                expected,
            });
        }
    }
    cases.sort_by(|left, right| left.input.cmp(&right.input));
    Ok(cases)
}

fn scrub(tool: &str, input: &[u8]) -> Vec<u8> {
    Scrubber::with_defaults().scrub(tool, input).0
}

#[test]
fn corpus_round_trips() -> std::io::Result<()> {
    let scrubber = Scrubber::with_defaults();
    for case in corpus_cases()? {
        let input = fs::read(&case.input)?;
        let expected = fs::read(&case.expected)?;
        let (actual, _) = scrubber.scrub(&case.tool, &input);
        assert_eq!(actual, expected, "case {case:?} mismatch");
    }
    Ok(())
}

#[test]
fn stream_variant_matches_buffered_for_each_corpus_case() -> std::io::Result<()> {
    let scrubber = Scrubber::with_defaults();
    for case in corpus_cases()? {
        let input = fs::read(&case.input)?;
        let (buffered, _) = scrubber.scrub(&case.tool, &input);
        let mut stream = scrubber.scrub_stream(&case.tool, &input[..]);
        let mut streamed = Vec::new();
        stream.read_to_end(&mut streamed)?;
        assert_eq!(streamed, buffered, "case {case:?} stream mismatch");
    }
    Ok(())
}

#[test]
fn gt_ansi_controls_strip_escape_sequences() {
    assert_eq!(
        scrub("gt", b"\x1b[2Kticket ready\x1b[0m\n"),
        b"ticket ready\n"
    );
}

#[test]
fn gt_spinner_dropped_mid_progress() {
    assert_eq!(
        scrub("gt", "⠋ Loading site\nready\n".as_bytes()),
        b"ready\n"
    );
}

#[test]
fn gt_repeated_header_keeps_first_consecutive_header() {
    assert_eq!(
        scrub(
            "gt",
            b"=== derrick activity ===\n=== derrick activity ===\nticket\n"
        ),
        b"=== derrick activity ===\nticket\n"
    );
}

#[test]
fn bd_header_drops_table_header() {
    assert_eq!(
        scrub("bd", b"ID  Title  Status\nT-1  Build  open\n"),
        b"T-1: Build\n"
    );
}

#[test]
fn bd_separator_drops_table_rule() {
    assert_eq!(
        scrub("bd", b"----  -----  ------\nT-1  Build  open\n"),
        b"T-1: Build\n"
    );
}

#[test]
fn bd_list_row_folds_to_identifier_and_title() {
    assert_eq!(scrub("bd", b"T-1  Build API  open\n"), b"T-1: Build API\n");
}

#[test]
fn git_remote_progress_dropped() {
    assert_eq!(scrub("git", b"remote: Counting objects: 4\nok\n"), b"ok\n");
}

#[test]
fn git_transfer_progress_dropped() {
    assert_eq!(
        scrub("git", b"Receiving objects: 100% (4/4), done.\ndone\n"),
        b"done\n"
    );
}

#[test]
fn git_repeated_warning_collapses_consecutive_duplicates() {
    assert_eq!(
        scrub("git", b"warning: retry\nwarning: retry\nnext\n"),
        b"warning: retry\nnext\n"
    );
}

#[test]
fn gh_spinner_dropped() {
    assert_eq!(
        scrub("gh", "⠙ Fetching pull request\nPR #3\n".as_bytes()),
        b"PR #3\n"
    );
}

#[test]
fn gh_success_prefix_removed_but_text_kept() {
    assert_eq!(
        scrub("gh", "✓ Checks passed\n".as_bytes()),
        b"Checks passed\n"
    );
}

#[test]
fn gh_ansi_controls_removed() {
    assert_eq!(scrub("gh", b"\x1b[32mready\x1b[0m\n"), b"ready\n");
}

#[test]
fn claude_info_dropped() {
    assert_eq!(
        scrub("claude", b"[INFO] loading context\nanswer\n"),
        b"answer\n"
    );
}

#[test]
fn claude_tool_use_decoration_folded() {
    assert_eq!(
        scrub("claude", b"Tool use: Bash(cargo test)\n"),
        b"Bash: cargo test\n"
    );
}

#[test]
fn claude_thinking_marker_dropped() {
    assert_eq!(scrub("claude", "Thinking…\nready\n".as_bytes()), b"ready\n");
}

#[test]
fn codex_tokens_used_dropped() {
    assert_eq!(scrub("codex", b"tokens used: 12\nanswer\n"), b"answer\n");
}

#[test]
fn codex_exec_recap_dropped() {
    assert_eq!(scrub("codex", b"exec: cargo test\nok\n"), b"ok\n");
}

#[test]
fn codex_succeeded_footer_collapsed() {
    assert_eq!(
        scrub("codex", b"succeeded in 10ms\nsucceeded in 12ms\n"),
        b"succeeded (2 steps)\n"
    );
}

#[test]
fn copilot_file_decoration_dropped() {
    assert_eq!(
        scrub("copilot", "● Read src/lib.rs\ncontent\n".as_bytes()),
        b"content\n"
    );
}

#[test]
fn copilot_premium_telemetry_collapsed() {
    assert_eq!(
        scrub(
            "copilot",
            b"Premium request 1 of 5\nPremium request 2 of 5\n"
        ),
        b"Premium request telemetry (2 lines)\n"
    );
}

#[test]
fn copilot_thinking_dropped() {
    assert_eq!(scrub("copilot", b"Thinking...\nready\n"), b"ready\n");
}

#[test]
fn cargo_compile_progress_keeps_first_kind() {
    assert_eq!(
        scrub(
            "cargo",
            b"   Compiling alpha v0.1.0\n   Compiling beta v0.1.0\nok\n"
        ),
        b"   Compiling alpha v0.1.0\nok\n"
    );
}

#[test]
fn cargo_fresh_progress_dropped() {
    assert_eq!(scrub("cargo", b"   Fresh alpha v0.1.0\nok\n"), b"ok\n");
}

#[test]
fn cargo_finished_footer_dropped() {
    assert_eq!(
        scrub("cargo", b"   Finished test target(s) in 0.10s\nok\n"),
        b"ok\n"
    );
}
