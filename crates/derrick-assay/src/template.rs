//! `{{...}}` template variable renderer for pipeline step fields.

use crate::types::RunError;

pub struct TemplateContext {
    pub prompt: String,
    pub site_name: String,
    pub site_prefix: String,
    pub feature_dir: Option<std::path::PathBuf>,
    pub run_id: String,
}

pub fn render_template(template: &str, context: &TemplateContext) -> Result<String, RunError> {
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let (prefix, after_prefix) = rest.split_at(start);
        rendered.push_str(prefix);
        let end = after_prefix
            .find("}}")
            .ok_or_else(|| RunError::Config("unterminated template var".to_owned()))?;
        let name = after_prefix[2..end].trim();
        rendered.push_str(&template_value(name, context)?);
        rest = &after_prefix[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

pub fn template_value(name: &str, context: &TemplateContext) -> Result<String, RunError> {
    match name {
        "prompt" => Ok(context.prompt.clone()),
        "site_name" => Ok(context.site_name.clone()),
        "site_prefix" => Ok(context.site_prefix.clone()),
        "run_id" => Ok(context.run_id.clone()),
        "feature_dir" => context
            .feature_dir
            .as_ref()
            .map(|path| crate::io::path_string(path))
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{feature_dir}} is not available before specify completes"
                        .to_owned(),
                )
            }),
        "tasks_md" => context
            .feature_dir
            .as_ref()
            .map(|path| crate::io::path_string(&path.join("tasks.md")))
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{tasks_md}} is not available before specify completes"
                        .to_owned(),
                )
            }),
        "batch" => context
            .feature_dir
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{batch}} is not available before specify completes".to_owned(),
                )
            }),
        "rig" => Err(RunError::Config(
            "unknown template var: {{rig}}; use {{site_name}}".to_owned(),
        )),
        other => Err(RunError::Config(format!(
            "unknown template var: {{{{{other}}}}}"
        ))),
    }
}

pub fn validate_template(template: &str, feature_available: bool) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after_prefix = &rest[start..];
        let end = after_prefix
            .find("}}")
            .ok_or_else(|| "unterminated template var".to_owned())?;
        let name = after_prefix[2..end].trim();
        match name {
            "prompt" | "site_name" | "site_prefix" | "run_id" => {}
            "feature_dir" | "tasks_md" | "batch" if feature_available => {}
            "feature_dir" | "tasks_md" | "batch" => {
                return Err(format!(
                    "template var {{{{{name}}}}} is not available before specify completes"
                ));
            }
            "rig" => return Err("unknown template var: {{rig}}; use {{site_name}}".to_owned()),
            other => return Err(format!("unknown template var: {{{{{other}}}}}")),
        }
        rest = &after_prefix[end + 2..];
    }
    Ok(())
}

pub fn validate_rounds_template(template: &str, feature_available: bool) -> Result<(), RunError> {
    if template == "{{tools.assay.rounds}}" {
        Ok(())
    } else {
        validate_template(template, feature_available).map_err(RunError::Config)
    }
}
