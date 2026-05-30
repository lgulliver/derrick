//! Curated, current per-host model catalogue and id normalisation (D64).
//!
//! Single source of truth for the five host CLIs derrick routes inference
//! through (`claude`, `codex`, `copilot`, `opencode`, `aider`): which model
//! ids are known, what each host's default is, and how a configured model id
//! is normalised before it is passed as `--model`.
//!
//! The table is a `const` so both the host adapters (free-fn call sites) and
//! the CLI's `derrick models check` (iterating [`HostCatalogue::hosts`]) can
//! reach it without a cycle. Unknown model ids are never rejected here — the
//! caller WARNs and passes the id through verbatim (the hybrid-validation rule
//! recorded in D64).

/// How a host expects a model id to be shaped on the command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelIdStyle {
    /// A bare model id (e.g. `gpt-5.5`). A single leading `provider/` prefix is
    /// stripped during normalisation.
    BareId,
    /// A `provider/model` id passed through verbatim (e.g. `anthropic/claude-sonnet-4-6`).
    ProviderModel,
}

/// Catalogue entry for one host.
#[derive(Clone, Copy, Debug)]
pub struct HostModels {
    /// Host identifier (matches `HostAdapter::name` and the provider name).
    pub host: &'static str,
    /// The host's default model, if it has one. `None` means the host picks
    /// its own default (opencode/aider).
    pub default_model: Option<&'static str>,
    /// Curated set of known model ids for this host.
    pub known: &'static [&'static str],
    /// How the host expects a model id to be shaped.
    pub id_style: ModelIdStyle,
}

/// The full curated catalogue across all hosts.
#[derive(Clone, Copy, Debug)]
pub struct HostCatalogue {
    /// Per-host entries.
    pub entries: &'static [HostModels],
}

impl HostCatalogue {
    /// Returns the entry for a host, if known.
    pub fn host(&self, host: &str) -> Option<&HostModels> {
        self.entries.iter().find(|entry| entry.host == host)
    }

    /// Lists the known host identifiers.
    pub fn hosts(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().map(|entry| entry.host)
    }

    /// Returns whether `model` is in the curated set for `host`.
    ///
    /// Unknown hosts and unknown models both return `false` — the caller
    /// decides whether that is a WARN or a FAIL.
    pub fn is_known(&self, host: &str, model: &str) -> bool {
        self.host(host)
            .is_some_and(|entry| entry.known.contains(&model))
    }

    /// Normalises a model id for a host just before it is passed as `--model`.
    ///
    /// - [`ModelIdStyle::BareId`]: strips ONE leading `provider/` segment.
    /// - [`ModelIdStyle::ProviderModel`]: returns the id verbatim.
    /// - Unknown host: returns the id verbatim.
    ///
    /// Never translates between dotted and dashed ids.
    pub fn normalize(&self, host: &str, model: &str) -> String {
        match self.host(host).map(|entry| entry.id_style) {
            Some(ModelIdStyle::BareId) => model
                .split_once('/')
                .map(|(_, rest)| rest)
                .unwrap_or(model)
                .to_owned(),
            Some(ModelIdStyle::ProviderModel) | None => model.to_owned(),
        }
    }
}

/// Returns the built-in catalogue (May 2026).
pub const fn builtin() -> HostCatalogue {
    HostCatalogue {
        entries: &[
            HostModels {
                host: "claude",
                default_model: Some("claude-opus-4-8"),
                known: &["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5"],
                id_style: ModelIdStyle::BareId,
            },
            HostModels {
                host: "codex",
                default_model: Some("gpt-5.5"),
                known: &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.2-codex"],
                id_style: ModelIdStyle::BareId,
            },
            HostModels {
                host: "copilot",
                default_model: Some("gpt-5.4"),
                known: &[
                    "gpt-5.4",
                    "gpt-5.3-codex",
                    "claude-sonnet-4.6",
                    "claude-haiku-4.5",
                    "gpt-5.4-mini",
                ],
                id_style: ModelIdStyle::BareId,
            },
            HostModels {
                host: "opencode",
                default_model: None,
                known: &[
                    "anthropic/claude-sonnet-4-6",
                    "anthropic/claude-opus-4-8",
                    "openai/gpt-5.5",
                ],
                id_style: ModelIdStyle::ProviderModel,
            },
            HostModels {
                host: "aider",
                default_model: None,
                known: &["anthropic/claude-sonnet-4-6", "openai/gpt-5.5"],
                id_style: ModelIdStyle::ProviderModel,
            },
        ],
    }
}

/// Normalises `model` for `host` using the built-in catalogue.
pub fn normalize(host: &str, model: &str) -> String {
    builtin().normalize(host, model)
}

/// Returns whether `model` is known for `host` in the built-in catalogue.
pub fn is_known(host: &str, model: &str) -> bool {
    builtin().is_known(host, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_id_strips_one_provider_prefix() {
        assert_eq!(
            normalize("claude", "anthropic/claude-opus-4-8"),
            "claude-opus-4-8"
        );
        assert_eq!(normalize("codex", "openai/gpt-5.5"), "gpt-5.5");
        assert_eq!(
            normalize("copilot", "anything/claude-sonnet-4.6"),
            "claude-sonnet-4.6"
        );
    }

    #[test]
    fn bare_id_keeps_unprefixed_verbatim() {
        assert_eq!(normalize("claude", "claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(
            normalize("copilot", "claude-sonnet-4.6"),
            "claude-sonnet-4.6"
        );
    }

    #[test]
    fn bare_id_strips_only_one_segment() {
        assert_eq!(normalize("codex", "a/b/gpt-5.5"), "b/gpt-5.5");
    }

    #[test]
    fn provider_model_passes_through_verbatim() {
        assert_eq!(
            normalize("opencode", "anthropic/claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(normalize("aider", "openai/gpt-5.5"), "openai/gpt-5.5");
    }

    #[test]
    fn unknown_host_passes_through_verbatim() {
        assert_eq!(normalize("mystery", "anthropic/x"), "anthropic/x");
    }

    #[test]
    fn never_translates_dot_dash() {
        // copilot keeps its dotted ids; normalisation only strips a prefix.
        assert_eq!(
            normalize("copilot", "claude-sonnet-4.6"),
            "claude-sonnet-4.6"
        );
    }

    #[test]
    fn is_known_matches_curated_set() {
        assert!(is_known("claude", "claude-opus-4-8"));
        assert!(is_known("codex", "gpt-5.2-codex"));
        assert!(!is_known("claude", "claude-opus-4-7"));
        assert!(!is_known("nonsuch", "anything"));
    }

    #[test]
    fn opencode_aider_have_no_default() {
        let cat = builtin();
        assert!(cat.host("opencode").unwrap().default_model.is_none());
        assert!(cat.host("aider").unwrap().default_model.is_none());
    }

    #[test]
    fn hosts_lists_all_five() {
        let cat = builtin();
        let hosts: Vec<&str> = cat.hosts().collect();
        assert_eq!(
            hosts,
            vec!["claude", "codex", "copilot", "opencode", "aider"]
        );
    }
}
