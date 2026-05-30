//! Curated, current per-host model catalogue and id normalisation (D65).
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
//! recorded in D65).

/// How a host expects a model id to be shaped on the command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelIdStyle {
    /// A bare model id (e.g. `gpt-5.5`). A single leading `provider/` prefix is
    /// stripped during normalisation.
    BareId,
    /// A `provider/model` id passed through verbatim (e.g. `anthropic/claude-sonnet-4-6`).
    ProviderModel,
}

/// Complexity tier the foreman resolves a ticket to before picking a model
/// within the configured host (D67). Ordered lightest to heaviest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Smallest, cheapest model for low-complexity tickets.
    Light,
    /// The default tier for ordinary tickets.
    Standard,
    /// The strongest model for heavy tickets.
    Heavy,
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
    /// Per-tier model ids for adaptive selection (D67), ordered light→heavy.
    /// May be empty for hosts that expose no tier mapping.
    pub tiers: &'static [(Tier, &'static str)],
}

impl HostModels {
    /// Returns the model id for `tier` with a fallback that never silently
    /// escalates to a heavier model than Standard.
    ///
    /// - exact tier match → that id;
    /// - else the Standard-tier entry (the safe middle ground);
    /// - else the host's `default_model`;
    /// - else the FIRST (lightest) `tiers` entry;
    /// - else `None`.
    pub fn model_for_tier(&self, tier: Tier) -> Option<&'static str> {
        if let Some((_, id)) = self.tiers.iter().find(|(entry, _)| *entry == tier) {
            return Some(id);
        }
        if let Some((_, id)) = self
            .tiers
            .iter()
            .find(|(entry, _)| *entry == Tier::Standard)
        {
            return Some(id);
        }
        if let Some(default) = self.default_model {
            return Some(default);
        }
        self.tiers.first().map(|(_, id)| *id)
    }
}

/// How a configured executor model id resolves at dispatch time (D67).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelChoice {
    /// An explicit model id pinned in config — always wins.
    Pinned(String),
    /// Foreman-selected per ticket. `bias` overrides the ticket's resolved
    /// tier when set (e.g. `auto:heavy`).
    Auto {
        /// Optional tier override parsed from `auto:<tier>`.
        bias: Option<Tier>,
    },
}

/// Parses a configured executor model id into a [`ModelChoice`].
///
/// The input is trimmed first, so surrounding whitespace never leaks into a
/// pin nor blocks the `auto` sentinels (e.g. `" auto "` parses as Auto, and
/// `"  anthropic/x  "` pins `anthropic/x`).
///
/// Returns [`ModelChoice::Auto`] ONLY for the exact strings `auto`,
/// `auto:light`, `auto:standard`, and `auto:heavy`. Everything else —
/// including `auto-foo` and the empty string — is treated as a pin.
pub fn parse_model_choice(raw: &str) -> ModelChoice {
    match raw.trim() {
        "auto" => ModelChoice::Auto { bias: None },
        "auto:light" => ModelChoice::Auto {
            bias: Some(Tier::Light),
        },
        "auto:standard" => ModelChoice::Auto {
            bias: Some(Tier::Standard),
        },
        "auto:heavy" => ModelChoice::Auto {
            bias: Some(Tier::Heavy),
        },
        other => ModelChoice::Pinned(other.to_owned()),
    }
}

/// Resolves the model id to pass to `host` for `choice` given the ticket's
/// resolved `tier` (D67).
///
/// - [`ModelChoice::Pinned`] with non-blank content → that id;
/// - [`ModelChoice::Pinned`] that is blank → `None` (host picks its default);
/// - [`ModelChoice::Auto`] → the host's model for `bias.unwrap_or(tier)`.
pub fn select_model(host: &str, choice: &ModelChoice, tier: Tier) -> Option<String> {
    match choice {
        ModelChoice::Pinned(id) if !id.trim().is_empty() => Some(id.clone()),
        ModelChoice::Pinned(_) => None,
        ModelChoice::Auto { bias } => builtin()
            .host(host)?
            .model_for_tier(bias.unwrap_or(tier))
            .map(str::to_owned),
    }
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
                tiers: &[
                    (Tier::Light, "claude-haiku-4-5"),
                    (Tier::Standard, "claude-sonnet-4-6"),
                    (Tier::Heavy, "claude-opus-4-8"),
                ],
            },
            HostModels {
                host: "codex",
                default_model: Some("gpt-5.5"),
                known: &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.2-codex"],
                id_style: ModelIdStyle::BareId,
                tiers: &[
                    (Tier::Light, "gpt-5.4-mini"),
                    (Tier::Standard, "gpt-5.4"),
                    (Tier::Heavy, "gpt-5.5"),
                ],
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
                tiers: &[
                    (Tier::Light, "claude-haiku-4.5"),
                    (Tier::Standard, "gpt-5.4"),
                    (Tier::Heavy, "gpt-5.3-codex"),
                ],
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
                tiers: &[
                    (Tier::Light, "anthropic/claude-sonnet-4-6"),
                    (Tier::Standard, "anthropic/claude-sonnet-4-6"),
                    (Tier::Heavy, "anthropic/claude-opus-4-8"),
                ],
            },
            HostModels {
                host: "aider",
                default_model: None,
                known: &["anthropic/claude-sonnet-4-6", "openai/gpt-5.5"],
                id_style: ModelIdStyle::ProviderModel,
                tiers: &[
                    (Tier::Light, "anthropic/claude-sonnet-4-6"),
                    (Tier::Standard, "anthropic/claude-sonnet-4-6"),
                    (Tier::Heavy, "openai/gpt-5.5"),
                ],
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

    #[test]
    fn parse_model_choice_auto_only_for_exact_forms() {
        assert_eq!(parse_model_choice("auto"), ModelChoice::Auto { bias: None });
        assert_eq!(
            parse_model_choice("auto:light"),
            ModelChoice::Auto {
                bias: Some(Tier::Light)
            }
        );
        assert_eq!(
            parse_model_choice("auto:standard"),
            ModelChoice::Auto {
                bias: Some(Tier::Standard)
            }
        );
        assert_eq!(
            parse_model_choice("auto:heavy"),
            ModelChoice::Auto {
                bias: Some(Tier::Heavy)
            }
        );
        // Anything else is a pin, including near-misses and the empty string.
        for raw in ["auto-foo", "", "auto:weird", "AUTO", "claude-opus-4-8"] {
            assert_eq!(
                parse_model_choice(raw),
                ModelChoice::Pinned(raw.to_owned()),
                "{raw:?} should be a pin"
            );
        }
    }

    #[test]
    fn model_for_tier_resolves_per_host() {
        let cat = builtin();
        let claude = cat.host("claude").expect("claude entry");
        assert_eq!(claude.model_for_tier(Tier::Light), Some("claude-haiku-4-5"));
        assert_eq!(
            claude.model_for_tier(Tier::Standard),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(claude.model_for_tier(Tier::Heavy), Some("claude-opus-4-8"));

        // aider now has an explicit Light tier (D67 fix): low-complexity aider
        // tickets must NOT fall through to the heaviest model.
        let aider = cat.host("aider").expect("aider entry");
        assert_eq!(
            aider.model_for_tier(Tier::Light),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            aider.model_for_tier(Tier::Standard),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(aider.model_for_tier(Tier::Heavy), Some("openai/gpt-5.5"));
    }

    #[test]
    fn every_host_resolves_all_three_tiers() {
        // Every built-in host must map Light/Standard/Heavy to a non-empty id —
        // no tier may fall through to nothing (D67).
        let cat = builtin();
        for entry in cat.entries {
            for tier in [Tier::Light, Tier::Standard, Tier::Heavy] {
                let id = entry.model_for_tier(tier);
                assert!(
                    id.is_some_and(|id| !id.is_empty()),
                    "host `{}` resolves no model for {tier:?}",
                    entry.host
                );
            }
        }
    }

    #[test]
    fn missing_tier_never_resolves_heavier_than_standard() {
        // Construct a host with only a Standard tier so Light/Heavy must fall
        // back. The fallback must land on Standard — never on a strictly-heavier
        // entry (the old `tiers.last()` bug picked the heaviest).
        let host = HostModels {
            host: "synthetic",
            default_model: None,
            known: &[],
            id_style: ModelIdStyle::ProviderModel,
            tiers: &[(Tier::Standard, "standard-model")],
        };
        assert_eq!(host.model_for_tier(Tier::Light), Some("standard-model"));
        assert_eq!(host.model_for_tier(Tier::Heavy), Some("standard-model"));
        assert_eq!(host.model_for_tier(Tier::Standard), Some("standard-model"));
    }

    #[test]
    fn parse_model_choice_trims_input() {
        // Surrounding whitespace must not leak into a pin nor block `auto`.
        assert_eq!(
            parse_model_choice(" auto "),
            ModelChoice::Auto { bias: None }
        );
        assert_eq!(
            parse_model_choice("  anthropic/x  "),
            ModelChoice::Pinned("anthropic/x".to_owned())
        );
    }

    #[test]
    fn select_model_pin_wins_and_blank_is_none() {
        assert_eq!(
            select_model(
                "claude",
                &ModelChoice::Pinned("claude-sonnet-4-6".to_owned()),
                Tier::Light
            ),
            Some("claude-sonnet-4-6".to_owned())
        );
        assert_eq!(
            select_model(
                "claude",
                &ModelChoice::Pinned("   ".to_owned()),
                Tier::Heavy
            ),
            None
        );
        assert_eq!(
            select_model("claude", &ModelChoice::Pinned(String::new()), Tier::Heavy),
            None
        );
    }

    #[test]
    fn select_model_auto_per_host_and_tier() {
        let auto = ModelChoice::Auto { bias: None };
        assert_eq!(
            select_model("claude", &auto, Tier::Heavy),
            Some("claude-opus-4-8".to_owned())
        );
        assert_eq!(
            select_model("claude", &auto, Tier::Light),
            Some("claude-haiku-4-5".to_owned())
        );
        assert_eq!(
            select_model("copilot", &auto, Tier::Light),
            Some("claude-haiku-4.5".to_owned())
        );
        assert_eq!(
            select_model("codex", &auto, Tier::Standard),
            Some("gpt-5.4".to_owned())
        );
        // Unknown host -> None under Auto.
        assert_eq!(select_model("mystery", &auto, Tier::Heavy), None);
    }

    #[test]
    fn select_model_bias_overrides_ticket_tier() {
        let biased = ModelChoice::Auto {
            bias: Some(Tier::Light),
        };
        // Ticket tier is Heavy, but the explicit bias forces the light model.
        assert_eq!(
            select_model("claude", &biased, Tier::Heavy),
            Some("claude-haiku-4-5".to_owned())
        );
    }

    #[test]
    fn select_model_never_returns_literal_auto() {
        for host in ["claude", "codex", "copilot", "opencode", "aider"] {
            for tier in [Tier::Light, Tier::Standard, Tier::Heavy] {
                let resolved = select_model(host, &ModelChoice::Auto { bias: None }, tier);
                assert_ne!(
                    resolved.as_deref(),
                    Some("auto"),
                    "select_model must never return the literal auto for {host}"
                );
            }
        }
    }
}
