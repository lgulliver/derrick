/// How the wizard wants specs produced. Mirrors
/// [`derrick_config::SpecProviderKind`] but lives in the CLI so the wizard can
/// own the prompt ordering and labels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SpecProviderChoice {
    #[default]
    Native,
    Speckit,
    /// Import an externally-authored spec.
    Import,
}

impl SpecProviderChoice {
    /// A one-line label for the preview/summary screens.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Speckit => "speckit",
            Self::Native => "native",
            Self::Import => "import",
        }
    }
}

const BARE_SPEC_STEPS: [&str; 3] = ["specify", "plan", "tasks"];

/// A commented stub the user must replace with their imported spec path. Kept
/// as a YAML comment so the generated config still parses with an unset source
/// (the import provider then errors clearly until a source is supplied or
/// `--spec` is passed).
const IMPORT_SOURCE_HINT: &str =
    "set this to your spec file path, or pass --spec <path> on the command line";

pub(crate) fn apply_spec_provider(
    rendered: &str,
    choice: SpecProviderChoice,
) -> Result<String, crate::CliError> {
    if choice == SpecProviderChoice::Native
        && rendered.contains("provider: native")
        && !rendered.contains("/speckit.")
    {
        return Ok(rendered.to_owned());
    }

    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(rendered).map_err(|error| crate::message(error.to_string()))?;
    let root = yaml
        .as_mapping_mut()
        .ok_or_else(|| crate::message("rendered config is not a mapping"))?;

    let provider = match choice {
        SpecProviderChoice::Native => "native",
        SpecProviderChoice::Speckit => "speckit",
        SpecProviderChoice::Import => "import",
    };

    write_specify_block(root, choice, provider);
    match choice {
        SpecProviderChoice::Native | SpecProviderChoice::Import => bare_spec_steps(root)?,
        SpecProviderChoice::Speckit => pin_speckit_steps(root)?,
    }

    let mut out =
        serde_yaml::to_string(&yaml).map_err(|error| crate::message(error.to_string()))?;

    // serde_yaml cannot emit the commented `source:` stub, so splice the hint in
    // textually under the `import:` block for the import provider.
    if choice == SpecProviderChoice::Import {
        out = annotate_import_source(&out);
    }
    Ok(out)
}

/// Writes/overwrites the `tools.specify` block with the chosen provider and,
/// for import, the `import.{plan,tasks}: native` downstream defaults.
fn write_specify_block(root: &mut serde_yaml::Mapping, choice: SpecProviderChoice, provider: &str) {
    let key = |name: &str| serde_yaml::Value::String(name.to_owned());
    let tools = root
        .entry(key("tools"))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let Some(tools) = tools.as_mapping_mut() else {
        return;
    };

    let mut specify = serde_yaml::Mapping::new();
    specify.insert(
        key("provider"),
        serde_yaml::Value::String(provider.to_owned()),
    );
    if choice == SpecProviderChoice::Import {
        let mut import = serde_yaml::Mapping::new();
        // `source` is intentionally left out of the structured value; it is added
        // back as a YAML comment by `annotate_import_source` so the user fills it
        // in. `plan`/`tasks` carry the documented native downstream defaults.
        import.insert(key("plan"), serde_yaml::Value::String("native".to_owned()));
        import.insert(key("tasks"), serde_yaml::Value::String("native".to_owned()));
        specify.insert(key("import"), serde_yaml::Value::Mapping(import));
    }
    tools.insert(key("specify"), serde_yaml::Value::Mapping(specify));
}

/// Strips `role`/`host`/`command`/`runner` from the `specify`/`plan`/`tasks`
/// pipeline steps so they become *bare* and route through the seam. A bare spec
/// step carries no role/host/command/runner; the native generator resolves its
/// own drafter/proposer tiers against `config.roles()` (it never reads the
/// step's `role:`). Leaving a `role:` on the step would fail the runtime
/// `is_bare` test and silently bypass the seam.
fn bare_spec_steps(root: &mut serde_yaml::Mapping) -> Result<(), crate::CliError> {
    let key = serde_yaml::Value::String("pipeline".to_owned());
    let Some(pipeline) = root.get_mut(&key) else {
        return Ok(());
    };
    let steps = pipeline
        .as_sequence_mut()
        .ok_or_else(|| crate::message("pipeline is not a sequence"))?;
    for step in steps {
        let Some(mapping) = step.as_mapping_mut() else {
            continue;
        };
        let id = mapping
            .get(serde_yaml::Value::String("id".to_owned()))
            .and_then(serde_yaml::Value::as_str);
        if id.is_some_and(|id| BARE_SPEC_STEPS.contains(&id)) {
            for field in ["role", "host", "command", "runner"] {
                mapping.remove(serde_yaml::Value::String(field.to_owned()));
            }
        }
    }
    Ok(())
}

fn pin_speckit_steps(root: &mut serde_yaml::Mapping) -> Result<(), crate::CliError> {
    let key = serde_yaml::Value::String("pipeline".to_owned());
    let Some(pipeline) = root.get_mut(&key) else {
        return Ok(());
    };
    let steps = pipeline
        .as_sequence_mut()
        .ok_or_else(|| crate::message("pipeline is not a sequence"))?;

    for step in steps.iter_mut() {
        let Some(mapping) = step.as_mapping_mut() else {
            continue;
        };
        let id = mapping
            .get(serde_yaml::Value::String("id".to_owned()))
            .and_then(serde_yaml::Value::as_str);
        match id {
            Some("specify") => pin_step(mapping, "drafter", "/speckit.specify {{prompt}}"),
            Some("plan") => pin_step(mapping, "proposer", "/speckit.plan"),
            Some("tasks") => pin_step(mapping, "drafter", "/speckit.tasks"),
            Some("analyze") => pin_step(mapping, "proposer", "/speckit.analyze"),
            _ => {}
        }
    }

    if !steps.iter().any(|step| {
        step.get("id")
            .and_then(serde_yaml::Value::as_str)
            .is_some_and(|id| id == "analyze")
    }) {
        let insert_at = steps
            .iter()
            .position(|step| {
                step.get("id")
                    .and_then(serde_yaml::Value::as_str)
                    .is_some_and(|id| id == "tasks")
            })
            .map_or(steps.len(), |index| index + 1);
        let mut analyze = serde_yaml::Mapping::new();
        analyze.insert(
            serde_yaml::Value::String("id".to_owned()),
            serde_yaml::Value::String("analyze".to_owned()),
        );
        pin_step(&mut analyze, "proposer", "/speckit.analyze");
        steps.insert(insert_at, serde_yaml::Value::Mapping(analyze));
    }

    Ok(())
}

fn pin_step(mapping: &mut serde_yaml::Mapping, role: &str, command: &str) {
    let key = |name: &str| serde_yaml::Value::String(name.to_owned());
    mapping.remove(key("runner"));
    mapping.insert(key("role"), serde_yaml::Value::String(role.to_owned()));
    mapping.insert(key("host"), serde_yaml::Value::String("claude".to_owned()));
    mapping.insert(
        key("command"),
        serde_yaml::Value::String(command.to_owned()),
    );
}

/// Inserts a commented `source:` stub directly under the emitted `import:`
/// mapping so the user has an obvious slot to fill in. The child indentation is
/// derived from the `import:` line's own indent (serde_yaml's two-space step is
/// not hard-coded here).
fn annotate_import_source(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + IMPORT_SOURCE_HINT.len() + 64);
    for line in yaml.lines() {
        out.push_str(line);
        out.push('\n');
        if line.trim_end().trim_start() == "import:" {
            let indent: String = line.chars().take_while(|ch| ch.is_whitespace()).collect();
            out.push_str(&format!(
                "{indent}  # source: <path>  # {IMPORT_SOURCE_HINT}\n"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use derrick_config::{Config, Host, SpecProviderKind};

    const TEMPLATE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../templates/derrick.yaml.in"
    ));

    /// Renders the bundled template with concrete site/prefix/mode so it parses
    /// as a real config.
    fn rendered_template() -> String {
        derrick_config::render_init_template(
            TEMPLATE,
            derrick_config::InitTemplateVars {
                site_name: "t",
                prefix: "tst",
                mode: "solo",
            },
        )
    }

    fn load(yaml: &str) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        std::fs::write(&path, yaml).expect("write");
        Config::load_from_path(&path).expect("generated config should load")
    }

    #[test]
    fn speckit_choice_writes_provider_and_pins_commands() {
        let rendered = rendered_template();
        let out = apply_spec_provider(&rendered, SpecProviderChoice::Speckit).expect("apply");
        let config = load(&out);
        assert_eq!(
            config.tools().specify().provider(),
            SpecProviderKind::Speckit
        );
        for id in ["specify", "plan", "tasks", "analyze"] {
            let step = config
                .pipeline()
                .iter()
                .find(|step| step.id() == id)
                .unwrap_or_else(|| panic!("{id} step"));
            assert_eq!(step.host(), Some(Host::Claude), "{id} should pin claude");
            assert!(
                step.command()
                    .is_some_and(|command| command.contains("/speckit.")),
                "{id} should pin speckit"
            );
        }
    }

    #[test]
    fn speckit_choice_pins_existing_analyze_step() {
        let rendered =
            rendered_template().replace("  - id: tasks\n", "  - id: tasks\n  - id: analyze\n");
        let out = apply_spec_provider(&rendered, SpecProviderChoice::Speckit).expect("apply");
        let config = load(&out);
        let analyze = config
            .pipeline()
            .iter()
            .find(|step| step.id() == "analyze")
            .expect("analyze step");
        assert_eq!(analyze.host(), Some(Host::Claude));
        assert_eq!(analyze.command(), Some("/speckit.analyze"));
        assert_eq!(
            config
                .pipeline()
                .iter()
                .filter(|step| step.id() == "analyze")
                .count(),
            1
        );
    }

    #[test]
    fn native_choice_writes_provider_and_bares_steps() {
        let rendered = rendered_template();
        let out = apply_spec_provider(&rendered, SpecProviderChoice::Native).expect("apply");
        let config = load(&out);
        assert_eq!(
            config.tools().specify().provider(),
            SpecProviderKind::Native
        );
        for id in ["specify", "plan", "tasks"] {
            let step = config
                .pipeline()
                .iter()
                .find(|step| step.id() == id)
                .unwrap_or_else(|| panic!("{id} step"));
            assert!(step.host().is_none(), "{id} should be bare (no host)");
            assert!(step.command().is_none(), "{id} should be bare (no command)");
            // A bare spec step carries no role; the native generator resolves
            // its own drafter/proposer tiers.
            assert!(step.role().is_none(), "{id} should be bare (no role)");
        }
        assert!(config.pipeline().iter().all(|step| step.id() != "analyze"));
    }

    #[test]
    fn import_choice_writes_provider_source_stub_and_native_downstream() {
        let rendered = rendered_template();
        let out = apply_spec_provider(&rendered, SpecProviderChoice::Import).expect("apply");
        // The commented source stub is present for the user to fill in.
        assert!(
            out.contains("# source:"),
            "expected a commented source stub, got:\n{out}"
        );
        let config = load(&out);
        let specify = config.tools().specify();
        assert_eq!(specify.provider(), SpecProviderKind::Import);
        // Unset source: the provider errors at run time until set or --spec given.
        assert_eq!(specify.import().source(), None);
        assert_eq!(
            specify.import().plan(),
            derrick_config::DownstreamMode::Native
        );
        assert_eq!(
            specify.import().tasks(),
            derrick_config::DownstreamMode::Native
        );
        // Steps are bared so the seam routes specify (then native plan/tasks).
        let specify_step = config
            .pipeline()
            .iter()
            .find(|step| step.id() == "specify")
            .expect("specify step");
        assert!(specify_step.host().is_none());
        assert!(specify_step.command().is_none());
    }

    #[test]
    fn label_is_stable() {
        assert_eq!(SpecProviderChoice::Speckit.label(), "speckit");
        assert_eq!(SpecProviderChoice::Native.label(), "native");
        assert_eq!(SpecProviderChoice::Import.label(), "import");
    }
}
