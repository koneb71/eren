use crate::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Task-complexity tier that routes a run to a concrete model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Mechanical, well-specified work.
    Easy,
    /// Typical feature/bugfix work.
    #[default]
    Medium,
    /// Architecture, judging, gnarly debugging.
    Complex,
}

/// What a person picked, which is not the same as what a run gets.
///
/// [`ModelTier`] answers "which model"; this answers "who decides". They are
/// kept apart for a concrete reason: [`TierMapping::model_for`] falls back to
/// `claude-opus-5` for a tier it has no entry for, so an `Auto` variant added
/// to `ModelTier` would resolve to the *most expensive* model every time it
/// reached a mapping — the exact outcome automatic routing exists to prevent.
/// Keeping `ModelTier` closed at three real tiers means `auto` cannot reach
/// `model_for` at all; it has to be resolved first.
///
/// Stored as plain text in the same columns as a tier (`tasks.model_tier`,
/// `agents.model_tier`, `runs.tier_override`), so nothing needed a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TierChoice {
    /// Let Eren pick per run, from what it can see about the work.
    Auto,
    Easy,
    Medium,
    Complex,
}

impl TierChoice {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "easy" => Some(Self::Easy),
            "medium" => Some(Self::Medium),
            "complex" => Some(Self::Complex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Easy => "easy",
            Self::Medium => "medium",
            Self::Complex => "complex",
        }
    }

    /// The tier this choice pins, or `None` when Eren decides.
    pub fn fixed(self) -> Option<ModelTier> {
        match self {
            Self::Auto => None,
            Self::Easy => Some(ModelTier::Easy),
            Self::Medium => Some(ModelTier::Medium),
            Self::Complex => Some(ModelTier::Complex),
        }
    }
}

impl From<ModelTier> for TierChoice {
    fn from(t: ModelTier) -> Self {
        match t {
            ModelTier::Easy => Self::Easy,
            ModelTier::Medium => Self::Medium,
            ModelTier::Complex => Self::Complex,
        }
    }
}

/// Tier → model-ID mapping. Stored in settings so users on plans without
/// access to a given model can remap (e.g. Complex → Opus).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierMapping(pub BTreeMap<ModelTier, String>);

impl Default for TierMapping {
    fn default() -> Self {
        // Complex maps to Opus, not Fable. The most capable model is not the
        // right default for anyone: it is the one most likely to be outside a
        // given plan, and the one that turns an ordinary task into a bill you
        // didn't ask for. Fable is offered in settings for people who want it.
        //
        // Aliases, not ids: the installed CLI resolves `opus` to the newest
        // Opus it knows, so the defaults follow Anthropic's releases with no
        // edit here — only an update of the CLI.
        Self(BTreeMap::from([
            (ModelTier::Easy, "sonnet".to_string()),
            (ModelTier::Medium, "opus".to_string()),
            (ModelTier::Complex, "opus".to_string()),
        ]))
    }
}

/// A model the user may route a tier to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelChoice {
    pub id: &'static str,
    pub label: &'static str,
    /// One line on when it earns its keep, shown beside the picker.
    pub blurb: &'static str,
}

/// What the settings picker offers.
///
/// A fixed list rather than a free-text field: a typo'd model id fails at
/// the point a run starts, minutes later and far from where it was entered.
///
/// The aliases come first and are what the defaults use: Claude Code's
/// `--model` takes `haiku`, `sonnet`, `opus` and `fable` and resolves each to
/// the newest model of that family the installed CLI knows. Nothing asks
/// Anthropic for a list — that would mean an API call with a credential,
/// which Eren never makes — so "latest" is whatever the CLI on this machine
/// says it is, and updating the CLI is how new models arrive. The ids below
/// them pin one release, for whoever wants a run to stay put.
pub const MODEL_CHOICES: &[ModelChoice] = &[
    ModelChoice {
        id: "haiku",
        label: "Haiku (latest)",
        blurb: "Fastest and cheapest. Good for mechanical edits.",
    },
    ModelChoice {
        id: "sonnet",
        label: "Sonnet (latest)",
        blurb: "Balanced. The usual choice for well-specified work.",
    },
    ModelChoice {
        id: "opus",
        label: "Opus (latest)",
        blurb: "Strong general coding. The default for real feature work.",
    },
    ModelChoice {
        id: "fable",
        label: "Fable (latest)",
        blurb: "Most capable, and the most expensive. Opt in deliberately.",
    },
    ModelChoice {
        id: "claude-haiku-4-5-20251001",
        label: "Haiku 4.5",
        blurb: "Pinned: stays on this release.",
    },
    ModelChoice {
        id: "claude-sonnet-5-5",
        label: "Sonnet 5.5",
        blurb: "Pinned: stays on this release.",
    },
    ModelChoice {
        id: "claude-opus-5-5",
        label: "Opus 5.5",
        blurb: "Pinned: stays on this release.",
    },
    ModelChoice {
        id: "claude-fable-5-1",
        label: "Fable 5.1",
        blurb: "Pinned: stays on this release. The most expensive.",
    },
];

/// Ids the picker no longer offers but a saved setting, an agent or a run's
/// history may still name. Still accepted — refusing them would fail a save
/// that only re-sends what was already there — and still Claude's, so the
/// other engines keep recognising them as foreign.
pub const RETIRED_MODELS: &[&str] = &["claude-sonnet-5", "claude-opus-5", "claude-fable-5"];

/// Is this a Claude Code model we accept? Guards the settings endpoint.
pub fn is_known_model(id: &str) -> bool {
    MODEL_CHOICES.iter().any(|m| m.id == id) || RETIRED_MODELS.contains(&id)
}

impl TierMapping {
    pub fn model_for(&self, tier: ModelTier) -> &str {
        self.0.get(&tier).map(String::as_str).unwrap_or("opus")
    }
}

impl std::cmp::Ord for ModelTier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

impl std::cmp::PartialOrd for ModelTier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Tier routing per engine.
///
/// `claude-opus-5` is not something OpenCode can be asked for — it wants
/// `provider/model`. So "medium" cannot mean one model globally; it means one
/// model *per engine*, which is also what makes a cross-engine bake-off
/// meaningful rather than a category error.
#[derive(Debug, Clone, Serialize)]
pub struct EngineTierMapping(pub BTreeMap<String, TierMapping>);

impl EngineTierMapping {
    pub fn model_for(&self, engine: &str, tier: ModelTier) -> String {
        self.0
            .get(engine)
            .map(|m| m.model_for(tier).to_string())
            .unwrap_or_else(|| Self::defaults_for(engine).model_for(tier).to_string())
    }

    /// The mapping for one engine, falling back to that engine's defaults.
    pub fn for_engine(&self, engine: &str) -> TierMapping {
        self.0
            .get(engine)
            .cloned()
            .unwrap_or_else(|| Self::defaults_for(engine))
    }

    pub fn defaults_for(engine: &str) -> TierMapping {
        match engine {
            "opencode" => TierMapping(BTreeMap::from([
                (ModelTier::Easy, "anthropic/claude-haiku-4-5".to_string()),
                (ModelTier::Medium, "anthropic/claude-sonnet-4-5".to_string()),
                (
                    ModelTier::Complex,
                    "anthropic/claude-sonnet-4-5".to_string(),
                ),
            ])),
            // Three engines whose models are a fact about somebody's machine
            // rather than about Eren, so this states none and
            // `Orchestrator::derived_defaults` fills them in from what each
            // install actually reported. That is also what the settings page
            // shows as "default", so Reset goes somewhere real.
            //
            // For the local runtimes it is which models have been pulled or
            // loaded. For Codex it is a correction: this used to name
            // `gpt-5-codex` for every tier, and the CLI answers that with
            // "Model metadata not found. Defaulting to fallback metadata; this
            // can degrade performance" — the same warning it gives an id
            // nobody ever published.
            "codex" | "ollama" | "lmstudio" => TierMapping(BTreeMap::new()),
            // Gemini's documented aliases follow Google's releases, so they
            // stay right without editing when a model is retired.
            "gemini" => TierMapping(BTreeMap::from([
                (ModelTier::Easy, "flash-lite".to_string()),
                (ModelTier::Medium, "flash".to_string()),
                (ModelTier::Complex, "pro".to_string()),
            ])),
            // `auto` is the one id every Cursor account has; which models a
            // plan can use is the account's business.
            "cursor" => TierMapping(BTreeMap::from([
                (ModelTier::Easy, "auto".to_string()),
                (ModelTier::Medium, "auto".to_string()),
                (ModelTier::Complex, "auto".to_string()),
            ])),
            // Qwen's ids depend on which provider the person pointed it at,
            // so it states none and runs the one Qwen is configured for.
            "qwen" => TierMapping(BTreeMap::new()),
            // Amp has no model flag, only modes, and a tier climbs them.
            "amp" => TierMapping(BTreeMap::from([
                (ModelTier::Easy, "low".to_string()),
                (ModelTier::Medium, "medium".to_string()),
                (ModelTier::Complex, "high".to_string()),
            ])),
            // Claude Code and the mock engine both speak Claude model ids.
            _ => TierMapping::default(),
        }
    }
}

impl Default for EngineTierMapping {
    fn default() -> Self {
        Self(BTreeMap::from([
            ("claude-code".to_string(), Self::defaults_for("claude-code")),
            ("opencode".to_string(), Self::defaults_for("opencode")),
            ("codex".to_string(), Self::defaults_for("codex")),
        ]))
    }
}

/// How hard each tier thinks, per engine.
///
/// Deliberately a separate map from `EngineTierMapping` rather than a second
/// field on it. A tier answers two questions — which model, and how long it
/// gets to use it — but they are set at different times and by different
/// people, and folding them together would have changed the stored shape of a
/// setting every install already has.
///
/// A tier with no entry inherits: the machine-wide default if one is set,
/// otherwise whatever the CLI does on its own. So an empty map is the shipped
/// state and means "nothing pinned anywhere", not "nothing configured yet".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineTierEffort(pub BTreeMap<String, BTreeMap<ModelTier, ReasoningEffort>>);

impl EngineTierEffort {
    pub fn effort_for(&self, engine: &str, tier: ModelTier) -> Option<ReasoningEffort> {
        self.0.get(engine).and_then(|t| t.get(&tier)).copied()
    }

    /// One engine's row, empty when it has none.
    pub fn for_engine(&self, engine: &str) -> BTreeMap<ModelTier, ReasoningEffort> {
        self.0.get(engine).cloned().unwrap_or_default()
    }
}

/// Accepts both shapes, because installs predating per-engine routing stored
/// a flat `{easy,medium,complex}` and must keep working without a migration.
impl<'de> Deserialize<'de> for EngineTierMapping {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(de)?;
        // Nested: values are objects keyed by tier.
        if let Ok(nested) = serde_json::from_value::<BTreeMap<String, TierMapping>>(raw.clone()) {
            if !nested.is_empty() {
                return Ok(Self(nested));
            }
        }
        // Flat: the legacy shape belonged to Claude Code.
        let flat: TierMapping = serde_json::from_value(raw).map_err(serde::de::Error::custom)?;
        let mut map = BTreeMap::new();
        map.insert("claude-code".to_string(), flat);
        map.insert("opencode".to_string(), Self::defaults_for("opencode"));
        map.insert("codex".to_string(), Self::defaults_for("codex"));
        Ok(Self(map))
    }
}

/// Is `id` a model this engine can actually be asked for?
///
/// Claude Code has a fixed catalog worth validating against — a typo there
/// fails minutes later at run start. OpenCode fronts 75+ providers plus local
/// models, so any fixed list would be wrong within a week and would block
/// `ollama/…` outright. Validating the *shape* still catches the realistic
/// mistake, which is a Claude id pasted into the OpenCode field.
pub fn is_known_model_for(engine: &str, id: &str) -> bool {
    match engine {
        // The local runtimes go through OpenCode and speak its id shape, one
        // step narrower: the provider half is the runtime's own name, so
        // `ollama/deepseek-r1:latest`. Validating only the shape is still
        // right — what a machine has pulled changes without Eren hearing
        // about it. The adapter re-reads what the runtime holds before every
        // run: a named id it has runs, one it lacks is refused by name, and a
        // bare one is resolved against that catalog rather than failing the
        // save.
        "opencode" | "ollama" | "lmstudio" => is_provider_model_shape(id),
        // Same reasoning as OpenCode's, one step further: OpenAI's ids are
        // bare names with no provider prefix and the catalog moves, so any
        // fixed list here would reject a model that works. Anything non-empty
        // is accepted and the CLI reports what it cannot reach.
        "codex" => !id.trim().is_empty(),
        // Bare names that move with each vendor's releases: Gemini's aliases
        // and ids, Cursor's per-account catalog, Qwen's per-provider ids
        // (which may be `org/model` on a routing provider). A typo is the
        // CLI's to report; whitespace never is an id.
        "gemini" | "cursor" => {
            let id = id.trim();
            !id.is_empty() && !id.contains(char::is_whitespace) && !id.contains('/')
        }
        "qwen" => {
            let id = id.trim();
            !id.is_empty() && !id.contains(char::is_whitespace)
        }
        // Amp chooses the model; what a tier picks is a mode.
        "amp" => matches!(id, "low" | "medium" | "high" | "ultra"),
        _ => is_known_model(id),
    }
}

/// `provider/model`: at least one slash, every segment non-empty, no
/// whitespace.
///
/// **Two segments is a minimum, not a maximum**, and this used to demand
/// exactly two — which quietly rejected ids that are entirely ordinary. A
/// model can be namespaced by whoever published it, so LM Studio serves
/// `google/gemma-4-e4b`, and routing providers put their own name in front of
/// a full id, giving `openrouter/anthropic/claude-3.5-sonnet`. Both are three
/// segments and both were refused, so the model could not be saved and, once
/// discovery started offering them, could not be picked either.
pub fn is_provider_model_shape(id: &str) -> bool {
    let mut parts = id.split('/');
    let Some(provider) = parts.next() else {
        return false;
    };
    // `split_once` semantics: everything after the first slash is the model,
    // and it may contain more of them.
    let Some(rest) = id.split_once('/').map(|(_, r)| r) else {
        return false;
    };
    !provider.is_empty()
        && !rest.is_empty()
        && !rest.split('/').any(str::is_empty)
        && !id.chars().any(char::is_whitespace)
}

/// Choose a tier mapping from the models an install can actually reach.
///
/// The built-in OpenCode defaults name `anthropic/…`, which is only right for
/// someone authenticated with Anthropic. A user whose one provider is Google
/// would get three tiers pointing at models they cannot run — and would find
/// out when their first task failed, not when they installed it.
///
/// So: rank by substring against known coding models, best available wins,
/// and fall back to whatever *is* there rather than to nothing. Returns
/// `None` only when the list is empty, which means "we couldn't ask".
pub fn pick_defaults(available: &[String]) -> Option<TierMapping> {
    if available.is_empty() {
        return None;
    }
    // Family keywords rather than exact ids, so this doesn't need editing
    // every time a provider ships a point release. Roughly descending
    // capability within each tier.
    const STRONG: &[&str] = &["claude-opus", "claude-sonnet", "gpt-5", "coder", "-pro"];
    const FAST: &[&str] = &["claude-haiku", "gpt-5-mini", "-flash", "-mini"];

    // Not coding models. Handing one to a coding agent fails in a way that
    // reads as an Eren bug rather than a configuration one. Previews are
    // excluded too — a default that names one rots when it's withdrawn.
    const NOT_FOR_CODING: &[&str] = &[
        "image",
        "tts",
        "embed",
        "veo",
        "lyria",
        "live",
        "robotics",
        "translate",
        "computer-use",
        "deep-research",
        "preview",
    ];

    let usable = |id: &&String| !NOT_FOR_CODING.iter().any(|b| id.contains(b));
    let best = |prefs: &[&str]| -> Option<String> {
        prefs.iter().find_map(|p| {
            available
                .iter()
                .filter(usable)
                .filter(|id| id.contains(p))
                // Shortest match wins, so `gemini-3.6-flash` beats
                // `gemini-3.1-flash-lite`; ties go to the id that sorts last,
                // which for version-numbered families is the newer one.
                .min_by_key(|id| (id.len(), std::cmp::Reverse(id.as_str())))
                .cloned()
        })
    };

    let anything = available
        .iter()
        .filter(usable)
        .min_by_key(|id| (id.len(), std::cmp::Reverse(id.as_str())))
        .or_else(|| available.first())?;
    let strong = best(STRONG).unwrap_or_else(|| anything.clone());
    let fast = best(FAST).unwrap_or_else(|| strong.clone());
    Some(TierMapping(BTreeMap::from([
        (ModelTier::Easy, fast),
        (ModelTier::Medium, strong.clone()),
        (ModelTier::Complex, strong),
    ])))
}

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The case that motivated this: a real Google-only install, taken
    /// verbatim from `opencode models` on 2026-07-29.
    #[test]
    fn a_google_only_install_gets_models_it_can_actually_run() {
        let available = ids(&[
            "opencode/big-pickle",
            "google/gemini-2.5-flash",
            "google/gemini-2.5-pro",
            "google/gemini-3-pro-image",
            "google/gemini-3.1-pro-preview",
            "google/gemini-3.6-flash",
            "google/gemini-embedding-001",
            "google/veo-3.1-generate-preview",
        ]);
        let m = pick_defaults(&available).unwrap();
        // The only non-preview "pro" in the list.
        assert_eq!(m.model_for(ModelTier::Medium), "google/gemini-2.5-pro");
        assert_eq!(m.model_for(ModelTier::Easy), "google/gemini-3.6-flash");
    }

    #[test]
    fn image_tts_and_preview_variants_are_never_chosen() {
        let available = ids(&[
            "google/gemini-3-pro-image",
            "google/gemini-3.1-pro-preview",
            "google/lyria-3-pro-preview",
        ]);
        let m = pick_defaults(&available).unwrap();
        // Nothing usable, so it falls back rather than returning None: a
        // model the user can see and change beats no mapping at all.
        assert!(m.model_for(ModelTier::Medium).starts_with("google/"));
    }

    #[test]
    fn anthropic_still_wins_when_it_is_there() {
        let available = ids(&[
            "google/gemini-2.5-pro",
            "anthropic/claude-sonnet-4-5",
            "anthropic/claude-haiku-4-5",
        ]);
        let m = pick_defaults(&available).unwrap();
        assert_eq!(
            m.model_for(ModelTier::Complex),
            "anthropic/claude-sonnet-4-5"
        );
        assert_eq!(m.model_for(ModelTier::Easy), "anthropic/claude-haiku-4-5");
    }

    #[test]
    fn easy_falls_back_to_the_strong_model_rather_than_to_nothing() {
        let available = ids(&["anthropic/claude-opus-4-5"]);
        let m = pick_defaults(&available).unwrap();
        assert_eq!(m.model_for(ModelTier::Easy), "anthropic/claude-opus-4-5");
    }

    #[test]
    fn no_catalog_means_no_opinion() {
        assert!(pick_defaults(&[]).is_none());
    }
}

#[cfg(test)]
mod per_engine_tests {
    use super::*;

    #[test]
    fn each_engine_gets_ids_it_can_actually_use() {
        let m = EngineTierMapping::default();
        assert_eq!(m.model_for("claude-code", ModelTier::Medium), "opus");
        // Not a Claude id — OpenCode would reject that outright.
        assert!(m.model_for("opencode", ModelTier::Medium).contains('/'));
    }

    #[test]
    fn an_unknown_engine_falls_back_rather_than_returning_nothing() {
        let m = EngineTierMapping::default();
        assert!(!m
            .model_for("some-future-engine", ModelTier::Easy)
            .is_empty());
    }

    #[test]
    fn the_legacy_flat_setting_still_loads_and_belongs_to_claude() {
        // Installs predating per-engine routing stored this shape. Reading it
        // as anything other than Claude's would silently repoint every run.
        let flat = serde_json::json!({
            "easy": "claude-sonnet-5", "medium": "claude-opus-5", "complex": "claude-opus-5"
        });
        let m: EngineTierMapping = serde_json::from_value(flat).unwrap();
        assert_eq!(
            m.model_for("claude-code", ModelTier::Easy),
            "claude-sonnet-5"
        );
        // And OpenCode still gets something usable rather than a Claude id.
        assert!(m.model_for("opencode", ModelTier::Easy).contains('/'));
    }

    #[test]
    fn the_nested_setting_round_trips() {
        let m = EngineTierMapping::default();
        let json = serde_json::to_value(&m).unwrap();
        let back: EngineTierMapping = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.model_for("opencode", ModelTier::Complex),
            m.model_for("opencode", ModelTier::Complex)
        );
    }

    #[test]
    fn opencode_validates_shape_not_membership() {
        // The realistic mistake: a Claude id pasted into the OpenCode field.
        assert!(!is_known_model_for("opencode", "claude-opus-5"));
        assert!(is_known_model_for(
            "opencode",
            "anthropic/claude-sonnet-4-5"
        ));
        // A local model no catalog would ever list.
        assert!(is_known_model_for("opencode", "ollama/qwen3-coder"));
        // Three segments used to be refused here, and that was the bug: a
        // model can be namespaced by whoever published it, and a routing
        // provider puts its own name in front of a whole id. Both shapes are
        // real and both were unsaveable.
        assert!(is_known_model_for(
            "opencode",
            "lmstudio/google/gemma-4-e4b"
        ));
        assert!(is_known_model_for(
            "opencode",
            "openrouter/anthropic/claude-3.5-sonnet"
        ));
        // What must still be refused: an empty segment anywhere, or
        // whitespace, neither of which addresses anything.
        assert!(!is_known_model_for("opencode", "has space/model"));
        assert!(!is_known_model_for("opencode", "/leading"));
        assert!(!is_known_model_for("opencode", "trailing/"));
        assert!(!is_known_model_for("opencode", "double//slash"));
        assert!(!is_known_model_for("opencode", "noslash"));
    }

    #[test]
    fn claude_still_validates_against_its_catalog() {
        assert!(is_known_model_for("claude-code", "claude-opus-5-5"));
        assert!(is_known_model_for("claude-code", "opus"));
        assert!(!is_known_model_for("claude-code", "gpt-5"));
        assert!(!is_known_model_for("claude-code", "Opus"));
    }

    #[test]
    fn a_retired_id_a_setting_may_still_hold_is_accepted_but_not_offered() {
        for id in RETIRED_MODELS {
            assert!(is_known_model_for("claude-code", id), "{id}");
            assert!(!MODEL_CHOICES.iter().any(|m| m.id == *id), "{id}");
        }
    }

    #[test]
    fn the_defaults_follow_the_cli_rather_than_a_release() {
        // An alias, so a new Opus arrives with the CLI, not with an edit here.
        for tier in [ModelTier::Easy, ModelTier::Medium, ModelTier::Complex] {
            let id = TierMapping::default().model_for(tier).to_string();
            assert!(!id.starts_with("claude-"), "{tier:?} is pinned to {id}");
        }
    }

    #[test]
    fn a_tier_with_no_effort_inherits_rather_than_defaulting() {
        // The shipped state: nothing pinned anywhere. `None` here has to mean
        // "ask the next place", not "low" — a silent floor would be the one
        // outcome nobody chose.
        let empty = EngineTierEffort::default();
        assert_eq!(empty.effort_for("claude-code", ModelTier::Complex), None);

        let mut e = EngineTierEffort::default();
        e.0.insert(
            "claude-code".to_string(),
            BTreeMap::from([(ModelTier::Complex, ReasoningEffort::Max)]),
        );
        assert_eq!(
            e.effort_for("claude-code", ModelTier::Complex),
            Some(ReasoningEffort::Max)
        );
        // Set on one tier, silent on its siblings and on other engines.
        assert_eq!(e.effort_for("claude-code", ModelTier::Easy), None);
        assert_eq!(e.effort_for("opencode", ModelTier::Complex), None);
    }

    #[test]
    fn tier_efforts_survive_a_round_trip_through_settings() {
        let mut e = EngineTierEffort::default();
        e.0.insert(
            "opencode".to_string(),
            BTreeMap::from([
                (ModelTier::Easy, ReasoningEffort::Low),
                (ModelTier::Complex, ReasoningEffort::XHigh),
            ]),
        );
        let back: EngineTierEffort =
            serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        assert_eq!(
            back.effort_for("opencode", ModelTier::Complex),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(back.effort_for("opencode", ModelTier::Medium), None);
    }

    #[test]
    fn model_tier_refuses_auto_rather_than_defaulting_it() {
        // The trap this guards: several call sites read a tier with
        // `from_value(...).unwrap_or_default()`. If `ModelTier` accepted
        // "auto" it would land on Medium → Opus; if it silently *defaulted*
        // one, the same thing happens. Either way an automatic router that
        // exists to save money would route every auto card to the dearest
        // model, and nothing would say so.
        //
        // So `ModelTier` must reject the string outright, and `TierChoice` is
        // the only type allowed to understand it.
        assert!(serde_json::from_value::<ModelTier>(serde_json::json!("auto")).is_err());
        assert_eq!(TierChoice::parse("auto"), Some(TierChoice::Auto));
    }

    #[test]
    fn a_tier_choice_round_trips_through_a_text_column() {
        // These live in TEXT columns beside real tiers, so every value has to
        // survive the trip out and back without a migration.
        for c in [
            TierChoice::Auto,
            TierChoice::Easy,
            TierChoice::Medium,
            TierChoice::Complex,
        ] {
            assert_eq!(TierChoice::parse(c.as_str()), Some(c));
        }
        assert_eq!(TierChoice::parse("nonsense"), None);
    }

    #[test]
    fn only_auto_leaves_the_tier_undecided() {
        assert_eq!(TierChoice::Auto.fixed(), None);
        assert_eq!(TierChoice::Easy.fixed(), Some(ModelTier::Easy));
        assert_eq!(TierChoice::Complex.fixed(), Some(ModelTier::Complex));
        // And a real tier converts in without inventing a choice.
        assert_eq!(
            TierChoice::from(ModelTier::Medium).fixed(),
            Some(ModelTier::Medium)
        );
    }

    #[test]
    fn the_newer_engines_default_to_ids_they_can_run_and_validate_their_own() {
        let d = EngineTierMapping::defaults_for;
        assert_eq!(d("gemini").model_for(ModelTier::Easy), "flash-lite");
        assert_eq!(d("gemini").model_for(ModelTier::Complex), "pro");
        assert_eq!(d("cursor").model_for(ModelTier::Medium), "auto");
        assert_eq!(d("amp").model_for(ModelTier::Complex), "high");
        // Qwen's ids are its provider's; it names none.
        assert!(d("qwen").0.is_empty());

        assert!(is_known_model_for("amp", "ultra"));
        assert!(!is_known_model_for("amp", "gpt-5"));
        assert!(is_known_model_for("gemini", "gemini-2.5-pro"));
        assert!(!is_known_model_for("gemini", "google/gemini-2.5-pro"));
        assert!(!is_known_model_for("cursor", " "));
        assert!(is_known_model_for("qwen", "qwen/qwen3-coder"));
        assert!(!is_known_model_for("qwen", "qwen3 coder"));
    }
}
