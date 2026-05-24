//! Queue file rendering. The output is a complete Claude Code prompt: it
//! tells the agent what branch to create, the spec to implement, and how to
//! hand work back to the foreman via `derrick ticket review`.

/// Render the queue file content for a ticket dispatch.
///
/// The rendered markdown is a complete Claude Code prompt that:
/// 1. Creates `branch` based off `parent_branch`
/// 2. Implements the ticket
/// 3. Pushes the branch
/// 4. Runs `derrick ticket review <ticket_id> --branch <branch> --head-sha <sha>`
///
/// The last step is mandatory — it transitions the ticket to `InReview` and
/// triggers the foreman's verifier.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn render_queue_file(
    ticket_id: &str,
    batch: Option<&str>,
    title: &str,
    body: &str,
    branch: &str,
    parent_branch: &str,
    roughneck_enabled: bool,
    roughneck_level: &str,
) -> String {
    let batch_display = batch.unwrap_or("(none)");
    let mut out = String::new();
    out.push_str("# Derrick ticket: ");
    out.push_str(title);
    out.push('\n');
    out.push('\n');
    out.push_str(
        "You are implementing a ticket dispatched by derrick's crew-mode foreman.\n\
         Complete ALL steps below in order. Do not stop until step 5 is done.\n",
    );
    out.push('\n');
    out.push_str("## Ticket metadata\n");
    out.push_str(&format!("- **ID**: {ticket_id}\n"));
    out.push_str(&format!("- **Batch**: {batch_display}\n"));
    out.push_str(&format!("- **Branch**: `{branch}`\n"));
    out.push_str(&format!("- **Base**: `{parent_branch}`\n"));
    out.push('\n');
    out.push_str("## Specification\n");
    out.push('\n');
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str("## Required steps\n");
    out.push('\n');
    out.push_str("1. Create branch and check it out:\n");
    out.push_str("   ```\n");
    out.push_str(&format!("   git checkout -b {branch} {parent_branch}\n"));
    out.push_str("   ```\n");
    out.push_str(
        "2. Implement the specification above. Commit all changes with conventional\n   \
         commit messages.\n",
    );
    out.push_str("3. Push the branch:\n");
    out.push_str("   ```\n");
    out.push_str(&format!("   git push -u origin {branch}\n"));
    out.push_str("   ```\n");
    out.push_str("4. Capture your HEAD SHA:\n");
    out.push_str("   ```\n");
    out.push_str("   git rev-parse HEAD\n");
    out.push_str("   ```\n");
    out.push_str(
        "5. Tell the foreman your work is ready (replace `<HEAD_SHA>` with the output\n   \
         of step 4):\n",
    );
    out.push_str("   ```\n");
    out.push_str(&format!(
        "   derrick ticket review {ticket_id} --branch {branch} --head-sha <HEAD_SHA>\n"
    ));
    out.push_str("   ```\n");
    out.push('\n');
    out.push_str(
        "**Do not open a PR yourself** — the foreman handles PR creation when stacking\n\
         is configured.\n",
    );
    if roughneck_enabled {
        derrick_roughneck::inject_prompt(&out, roughneck_level)
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_branch_parent_and_review_command() {
        let rendered = render_queue_file(
            "drk-099",
            Some("phase-1"),
            "implement widget",
            "Do the widget thing.",
            "derrick/phase-1/drk-099",
            "main",
            false,
            "full",
        );
        assert!(rendered.contains("derrick/phase-1/drk-099"));
        assert!(rendered.contains("main"));
        assert!(rendered.contains("derrick ticket review drk-099"));
        assert!(rendered.contains("--branch derrick/phase-1/drk-099"));
        assert!(rendered.contains("Do the widget thing."));
        assert!(rendered.contains("phase-1"));
    }

    #[test]
    fn renders_batch_none_when_missing() {
        let rendered = render_queue_file(
            "drk-001",
            None,
            "title",
            "body",
            "derrick/ad-hoc/drk-001",
            "main",
            false,
            "full",
        );
        assert!(rendered.contains("**Batch**: (none)"));
    }

    #[test]
    fn renders_with_roughneck_header() {
        let rendered = render_queue_file(
            "drk-200",
            None,
            "title",
            "body",
            "derrick/ad-hoc/drk-200",
            "main",
            true,
            "full",
        );
        assert!(rendered.starts_with("[ROUGHNECK:FULL]"));
        // Body is preserved after the header.
        assert!(rendered.contains("derrick ticket review drk-200"));
    }

    #[test]
    fn omits_roughneck_header_when_disabled() {
        let rendered = render_queue_file(
            "drk-201",
            None,
            "title",
            "body",
            "derrick/ad-hoc/drk-201",
            "main",
            false,
            "full",
        );
        assert!(!rendered.starts_with("[ROUGHNECK:"));
    }
}
