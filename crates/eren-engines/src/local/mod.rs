//! Ollama and LM Studio, as engines you can actually pick.
//!
//! # What this is, and what it is not
//!
//! A local runtime is not an agent. Ollama and LM Studio serve a model over
//! an HTTP API: they hold no tools, edit no files and run no commands, so a
//! board card handed straight to one would produce prose *about* editing a
//! file and no edited file. All of that is still true, and it is why
//! [`crate::opencode`] rather than this module does the work.
//!
//! What changed is the framing, not the fact. OpenCode *is* an agent, and it
//! can front any OpenAI-compatible endpoint — which is what both of these
//! are. So "run this card on the model in LM Studio" has a complete and
//! honest answer: **the agent is OpenCode, the model is local**. This adapter
//! is that pairing, named after the half a person cares about. Picking
//! "LM Studio" spawns the `opencode` binary from `PATH` with the provider
//! declared and the model resolved from what LM Studio actually has loaded.
//!
//! It exists because the same person asked twice, in the same words — "no
//! lmstudio or ollama" in the engine picker — after being told, correctly and
//! uselessly, that those are providers rather than engines. A taxonomy
//! somebody has to learn before they can find a feature is a design failure,
//! not a teaching opportunity. The distinction is real, so it is kept where
//! it costs nothing: in `capabilities`, which is delegated rather than
//! restated, and in the label, which says where the model is served.
//!
//! # The compliance invariants are unchanged
//!
//! 1. **Spawns official binaries from `PATH` and reads their stdout.**
//!    `opencode` to run, and each runtime's own CLI — `ollama list`,
//!    `lms ls --json` — to find out what it has. Deliberately *not* the
//!    `GET /api/tags` probe that [`eren_core::local_models`] uses to fill
//!    the settings page: that is a discovery convenience and this is an
//!    adapter, and adapters are held to the stricter rule.
//! 2. **No credentials.** There are none to read: a server on loopback has no
//!    account, which is also why `authenticated` is unconditionally true here
//!    rather than reporting a login that does not exist.
//! 3. **No auth environment variables.** The one variable it sets is the
//!    runtime's address — a URL out of a settings box — and the OpenCode
//!    adapter runs it past `is_auth_env` like anything else.
//! 4. **No proxying.** The conversation goes straight from OpenCode to the
//!    loopback port. Eren is not in it and never sees a token of it.

pub mod probe;

use crate::opencode::{config, OpenCodeEngine};
use crate::{Capabilities, Engine, EngineInfo, EngineProcess, RunSpec};
use async_trait::async_trait;
use eren_shared::env_guard;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Ollama,
    LmStudio,
}

impl Runtime {
    pub const ALL: &'static [Runtime] = &[Runtime::Ollama, Runtime::LmStudio];

    /// The engine id, which is also the provider half of every model id it
    /// serves — `ollama/deepseek-r1:latest`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
        }
    }

    /// Where it listens when nobody has said otherwise.
    ///
    /// Looked up in [`config::LOCAL`] rather than written down a second time.
    /// That table is what declares the provider inside the generated OpenCode
    /// config, so a private copy here could drift and point the *probe* at a
    /// different port than the *run* — which would present as "discovery
    /// works, runs don't", one of the harder failures to read backwards.
    fn default_host(self) -> &'static str {
        config::LOCAL
            .iter()
            .find(|(id, ..)| *id == self.id())
            .map(|(_, base, _)| *base)
            .expect("every runtime here is declared in config::LOCAL")
    }

    /// The variable [`config::local_provider`] reads the address out of.
    fn host_env(self) -> String {
        eren_shared::brand::env_name(&format!("{}_HOST", self.id().to_uppercase()))
    }

    /// Where to look for the runtime's own CLI.
    ///
    /// `lms` is the awkward one. LM Studio ships it inside the app bundle and
    /// only puts it on `PATH` if you have run `lms bootstrap`, so a perfectly
    /// ordinary install has the binary at a fixed, vendor-documented place and
    /// nowhere else. Looking there is still "the official binary" — it is the
    /// same file the bootstrap would have symlinked — and not looking would
    /// mean telling most LM Studio users their install is missing.
    fn candidates(self) -> Vec<PathBuf> {
        match self {
            Self::Ollama => vec![PathBuf::from("ollama")],
            Self::LmStudio => {
                let mut v = vec![PathBuf::from("lms")];
                if let Some(home) = std::env::var_os("HOME") {
                    v.push(
                        PathBuf::from(home)
                            .join(".lmstudio")
                            .join("bin")
                            .join("lms"),
                    );
                }
                v
            }
        }
    }
}

/// A local runtime paired with the agent that drives it.
pub struct LocalEngine {
    runtime: Runtime,
    /// The address a person configured, if they configured one. `None` means
    /// "work it out", which is not the same as "use the default" — for
    /// LM Studio the runtime itself reports the port it is on.
    configured_host: Option<String>,
    /// What actually runs the agent loop.
    runner: OpenCodeEngine,
    /// Full model ids, as of the last `detect` or `refresh`. `start` is
    /// synchronous and cannot go and ask, so this is how a run resolves a
    /// model against what the machine really has rather than against a tier
    /// mapping that may predate the runtime being installed.
    catalog: Mutex<Vec<String>>,
    /// Whether `catalog` is what the runtime said the last time it was asked.
    /// `false` after a re-read that failed: then it may be stale, and an id
    /// it lacks may be one pulled since.
    catalog_current: AtomicBool,
    /// The runtime's CLI, where `detect` found it, so a re-read does not go
    /// looking again.
    bin: Mutex<Option<PathBuf>>,
    /// The address `detect` saw the runtime on, when its CLI said.
    observed_host: Mutex<Option<String>>,
}

impl LocalEngine {
    pub fn new(runtime: Runtime, configured_host: Option<String>) -> Self {
        Self {
            runtime,
            configured_host: configured_host.map(|h| h.trim_end_matches('/').to_string()),
            runner: OpenCodeEngine::default(),
            catalog: Mutex::new(vec![]),
            catalog_current: AtomicBool::new(false),
            bin: Mutex::new(None),
            observed_host: Mutex::new(None),
        }
    }

    pub fn ollama(host: Option<String>) -> Self {
        Self::new(Runtime::Ollama, host)
    }

    pub fn lmstudio(host: Option<String>) -> Self {
        Self::new(Runtime::LmStudio, host)
    }

    /// What somebody set, then what the runtime said about itself, then the
    /// stock port. In that order: an explicit setting is the only one of the
    /// three that can express "the runtime is on another machine".
    fn host(&self) -> String {
        self.configured_host
            .clone()
            .or_else(|| self.observed_host.lock().unwrap().clone())
            .unwrap_or_else(|| self.runtime.default_host().to_string())
    }

    async fn locate(&self) -> Option<PathBuf> {
        for path in self.runtime.candidates() {
            // Cheapest thing that proves the file is there and runnable. Both
            // CLIs answer `--help` without touching their server.
            if env_guard::command(&path)
                .arg("--help")
                .output()
                .await
                .is_ok_and(|o| o.status.success())
            {
                return Some(path);
            }
        }
        None
    }

    /// The models the runtime holds right now, as full ids. `None` when it
    /// could not be asked.
    async fn list_models(&self, bin: &std::path::Path) -> Option<Vec<String>> {
        let args: &[&str] = match self.runtime {
            // Through the daemon, so this failing also says it is down.
            Runtime::Ollama => &["list"],
            Runtime::LmStudio => &["ls", "--json"],
        };
        let out = env_guard::command(bin).args(args).output().await.ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let names = match self.runtime {
            Runtime::Ollama => probe::ollama_models(&stdout),
            Runtime::LmStudio => probe::lmstudio_models(&stdout),
        };
        Some(
            names
                .into_iter()
                .map(|n| format!("{}/{n}", self.runtime.id()))
                .collect(),
        )
    }

    /// The model this run should actually ask for.
    ///
    /// Three cases, in order: an id already naming this runtime passes
    /// through if the runtime has it — or if the catalog could not be
    /// re-read, when it may simply be newer than what was last seen; a bare
    /// name — what somebody copies out of the runtime's own UI — gets its
    /// prefix if the runtime really has it; anything else is a tier still
    /// pointing where it pointed before this engine existed.
    ///
    /// A named id the runtime has just said it does not hold is refused, not
    /// substituted: unlike a tier left over from before, it is a choice
    /// somebody made, and running another model in its place would answer a
    /// question they did not ask.
    ///
    /// That last case **substitutes rather than refuses**, which is the
    /// opposite of what [`crate::vet`] does and is deliberate. `vet` refuses
    /// because downgrading a permission mode would be a privilege escalation
    /// performed on the user's behalf. There is nothing to escalate here: the
    /// alternative to substituting is a run that never starts because of a
    /// setting nobody made, and the model that did run is named in the run's
    /// own events either way.
    fn resolve_model(&self, requested: &str) -> anyhow::Result<String> {
        let catalog = self.catalog.lock().unwrap().clone();
        let prefix = format!("{}/", self.runtime.id());
        if requested.starts_with(&prefix) {
            if catalog.iter().any(|m| m == requested)
                || !self.catalog_current.load(Ordering::Relaxed)
            {
                return Ok(requested.to_string());
            }
            anyhow::bail!(
                "{} does not have {requested}. Pull or load it, or pick one it has: {}.",
                self.runtime.label(),
                if catalog.is_empty() {
                    "it has none".to_string()
                } else {
                    catalog.join(", ")
                }
            );
        }
        let prefixed = format!("{prefix}{requested}");
        if catalog.contains(&prefixed) {
            return Ok(prefixed);
        }
        let first = catalog.first().cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no models Eren can see. Pull or load one, then try again.",
                self.runtime.label()
            )
        })?;
        if !requested.trim().is_empty() {
            tracing::warn!(
                runtime = self.runtime.id(),
                requested,
                using = %first,
                "the configured model is not one this runtime serves"
            );
        }
        Ok(first)
    }
}

#[async_trait]
impl Engine for LocalEngine {
    fn id(&self) -> &'static str {
        self.runtime.id()
    }

    fn label(&self) -> &'static str {
        self.runtime.label()
    }

    /// OpenCode's, because OpenCode is what runs.
    ///
    /// Restating them here would let the two drift, and every one of these
    /// answers is a property of the agent rather than of the model behind it:
    /// whether a run can pause for approval is decided by the process Eren
    /// spawned, not by which endpoint served the tokens.
    fn capabilities(&self) -> Capabilities {
        self.runner.capabilities()
    }

    async fn detect(&self) -> Option<EngineInfo> {
        // Without OpenCode there is nothing to offer, however many models are
        // loaded — the runtime alone cannot edit a file.
        env_guard::command(&self.runner.binary)
            .arg("--version")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())?;

        let bin = self.locate().await?;
        *self.bin.lock().unwrap() = Some(bin.clone());
        let run = |args: Vec<&'static str>| {
            let bin = bin.clone();
            async move {
                let out = env_guard::command(&bin).args(&args).output().await.ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
            }
        };

        let (version, observed) = match self.runtime {
            Runtime::Ollama => {
                let version = probe::ollama_version(&run(vec!["--version"]).await?)?;
                (version, None)
            }
            Runtime::LmStudio => {
                let status = probe::lms_status(&run(vec!["status"]).await?);
                // Offered only while the server is on, for the same reason an
                // engine that isn't installed isn't offered: the alternative
                // is accepting the choice and dying at spawn, minutes later
                // and nowhere near where it was made. `doctor` tells the
                // difference between this and "not installed" — see [`hints`].
                if !status.on {
                    return None;
                }
                let version = probe::lms_version(&run(vec!["version"]).await?)?;
                let observed = status.port.map(|p| format!("http://127.0.0.1:{p}"));
                (version, observed)
            }
        };
        // Doubles as the liveness check: `ollama list` goes through the
        // daemon, so it succeeding is what says the runtime can serve
        // anything at all.
        let ids = self.list_models(&bin).await?;
        // A runtime that is running but holds nothing it could answer with is
        // not something to put in a picker: every run started on it would
        // fail, and the failure would be about a model rather than about the
        // choice the person actually made.
        if ids.is_empty() {
            return None;
        }

        *self.catalog.lock().unwrap() = ids.clone();
        self.catalog_current.store(true, Ordering::Relaxed);
        *self.observed_host.lock().unwrap() = observed;

        Some(EngineInfo {
            version,
            // There is nothing to sign into. Reporting `false` would make
            // every picker imply a login that does not exist, and reporting a
            // provider would put a made-up account name in the settings page.
            authenticated: true,
            providers: vec![],
            models: ids,
        })
    }

    /// What the runtime holds now, not at boot: a model pulled since is
    /// there to run, and one removed since is said to be gone by name
    /// rather than by OpenCode failing to reach it.
    async fn refresh(&self) {
        let Some(bin) = self.bin.lock().unwrap().clone() else {
            return;
        };
        match self.list_models(&bin).await {
            Some(ids) => {
                *self.catalog.lock().unwrap() = ids;
                self.catalog_current.store(true, Ordering::Relaxed);
            }
            None => self.catalog_current.store(false, Ordering::Relaxed),
        }
    }

    fn start(&self, mut spec: RunSpec) -> anyhow::Result<EngineProcess> {
        spec.model_id = self.resolve_model(&spec.model_id)?;
        // Where `config::local_provider` should point the baseURL. Not a
        // credential, and the OpenCode adapter runs it past the env guard
        // like anything else a caller supplies.
        spec.extra_env.insert(self.runtime.host_env(), self.host());
        self.runner.start(spec)
    }

    /// The session belongs to the OpenCode runner, which offers none yet.
    fn interactive_resume_argv(&self, session_id: &str) -> Option<Vec<String>> {
        self.runner.interactive_resume_argv(session_id)
    }
}

/// What to tell somebody whose local runtime is installed but not offered.
///
/// [`Engine::detect`] can only say yes or no, and `doctor` prints "no" as
/// *not installed* — which is a lie when LM Studio is sitting right there
/// with its server switched off, and an unhelpful one when the missing piece
/// is OpenCode rather than the runtime. This is the difference between those
/// cases, and it is the whole reason a person can tell an Eren problem from
/// a "you have not started the server" problem without reading the source.
pub async fn hints(opencode_binary: &str) -> Vec<String> {
    let mut out = vec![];
    let have_opencode = env_guard::command(opencode_binary)
        .arg("--version")
        .output()
        .await
        .is_ok_and(|o| o.status.success());

    for &runtime in Runtime::ALL {
        let engine = LocalEngine::new(runtime, None);
        let Some(bin) = engine.locate().await else {
            continue; // genuinely not installed; "not installed" was true
        };
        if !have_opencode {
            out.push(format!(
                "{} is installed, but Eren drives a local model through OpenCode \
                 and that isn't here: https://opencode.ai",
                runtime.label()
            ));
            continue;
        }
        if engine.detect().await.is_some() {
            continue; // offered; nothing to explain
        }
        out.push(match runtime {
            Runtime::LmStudio => format!(
                "{} is installed but Eren can't use it — its local server is off, or no \
                 model is loaded. Turn the server on (Developer → Start Server, or \
                 `{} server start`), load a model, then restart Eren.",
                runtime.label(),
                bin.display()
            ),
            Runtime::Ollama => format!(
                "{} is installed but Eren can't use it — it isn't running, or nothing \
                 is pulled. Try `ollama serve` and `ollama pull qwen2.5-coder:7b`, then \
                 restart Eren.",
                runtime.label()
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_and_the_run_agree_on_where_a_runtime_lives() {
        // The failure this prevents is subtle and would read as a bug in
        // discovery: `detect` finds a model on one port, the generated
        // OpenCode config points at another, and the run dies having been
        // offered a model that plainly exists.
        for &r in Runtime::ALL {
            let declared = config::LOCAL
                .iter()
                .find(|(id, ..)| *id == r.id())
                .unwrap_or_else(|| panic!("{} is not declared in config::LOCAL", r.id()));
            assert_eq!(r.default_host(), declared.1);
        }
    }

    #[test]
    fn the_host_variable_is_the_one_the_config_reads() {
        // `local_provider` builds this name itself; if the two spellings ever
        // diverged the address would be silently ignored and every run would
        // quietly go to the default port.
        assert_eq!(Runtime::Ollama.host_env(), "EREN_OLLAMA_HOST");
        assert_eq!(Runtime::LmStudio.host_env(), "EREN_LMSTUDIO_HOST");
        for &r in Runtime::ALL {
            assert!(
                !eren_shared::is_auth_env(&r.host_env()),
                "the address must not look like a credential, or start() refuses it"
            );
        }
    }

    #[test]
    fn a_configured_address_beats_what_the_runtime_reported() {
        // Only an explicit setting can express "the runtime is on another
        // machine", so it has to win over a port the local CLI volunteered.
        let e = LocalEngine::lmstudio(Some("http://box.local:9999/".into()));
        *e.observed_host.lock().unwrap() = Some("http://127.0.0.1:1234".into());
        assert_eq!(e.host(), "http://box.local:9999");

        let e = LocalEngine::lmstudio(None);
        assert_eq!(e.host(), Runtime::LmStudio.default_host());
        *e.observed_host.lock().unwrap() = Some("http://127.0.0.1:4444".into());
        assert_eq!(e.host(), "http://127.0.0.1:4444");
    }

    fn stocked(runtime: Runtime, models: &[&str]) -> LocalEngine {
        let e = LocalEngine::new(runtime, None);
        *e.catalog.lock().unwrap() = models.iter().map(|m| m.to_string()).collect();
        e.catalog_current.store(true, Ordering::Relaxed);
        e
    }

    #[test]
    fn an_id_that_already_names_this_runtime_passes_through_if_it_is_there() {
        let e = stocked(Runtime::Ollama, &["ollama/deepseek-r1:latest"]);
        assert_eq!(
            e.resolve_model("ollama/deepseek-r1:latest").unwrap(),
            "ollama/deepseek-r1:latest"
        );
        // One the runtime has just said it does not hold is refused by name,
        // with what it does hold — not swapped for a model nobody chose.
        let err = e
            .resolve_model("ollama/llama3:latest")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ollama/llama3:latest"), "{err}");
        assert!(err.contains("ollama/deepseek-r1:latest"), "{err}");
        // But a catalog that could not be re-read may be older than the
        // model: a pull since then is real, so the id goes through.
        e.catalog_current.store(false, Ordering::Relaxed);
        assert_eq!(
            e.resolve_model("ollama/llama3:latest").unwrap(),
            "ollama/llama3:latest"
        );
    }

    /// A model pulled after boot is runnable without a restart: the catalog
    /// is re-read before the run starts.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_model_pulled_since_boot_is_found_before_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("list.txt");
        std::fs::write(
            &list,
            "NAME ID SIZE MODIFIED\nqwen3:8b abc 5 GB 1 day ago\n",
        )
        .unwrap();
        let bin = crate::stand_in(dir.path(), &format!("cat '{}'", list.display()));
        let e = stocked(Runtime::Ollama, &["ollama/qwen3:8b"]);
        *e.bin.lock().unwrap() = Some(bin.into());
        assert!(e.resolve_model("ollama/llama3:latest").is_err());

        std::fs::write(
            &list,
            "NAME ID SIZE MODIFIED\nqwen3:8b abc 5 GB 1 day ago\nllama3:latest def 4 GB now\n",
        )
        .unwrap();
        e.refresh().await;
        assert_eq!(
            e.resolve_model("ollama/llama3:latest").unwrap(),
            "ollama/llama3:latest"
        );
    }

    #[test]
    fn a_bare_name_the_runtime_has_gets_its_prefix() {
        // What somebody copies out of LM Studio's own window.
        let e = stocked(Runtime::LmStudio, &["lmstudio/google/gemma-4-e4b"]);
        assert_eq!(
            e.resolve_model("google/gemma-4-e4b").unwrap(),
            "lmstudio/google/gemma-4-e4b"
        );
    }

    #[test]
    fn a_tier_still_pointing_at_a_hosted_model_runs_locally_anyway() {
        // The state every install starts in: the tier mapping says
        // `claude-opus-5` because that is the fallback, and this engine did
        // not exist when it was written. Refusing would mean the first run on
        // a freshly installed runtime fails over a setting nobody made.
        let e = stocked(Runtime::Ollama, &["ollama/deepseek-r1:latest"]);
        assert_eq!(
            e.resolve_model("claude-opus-5").unwrap(),
            "ollama/deepseek-r1:latest"
        );
        assert_eq!(e.resolve_model("").unwrap(), "ollama/deepseek-r1:latest");
    }

    #[test]
    fn an_empty_catalog_says_what_to_do_rather_than_naming_a_model() {
        let err = stocked(Runtime::LmStudio, &[])
            .resolve_model("anything")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("LM Studio"),
            "the message must name the runtime"
        );
        assert!(err.contains("load"), "and say what to do about it");
    }

    #[test]
    fn every_resolved_model_is_a_shape_opencode_would_accept() {
        // These ids go straight into `-m` and into the generated provider
        // block; a shape the validator rejects could not be saved as a tier
        // either, so the two have to agree.
        let e = stocked(
            Runtime::LmStudio,
            &["lmstudio/google/gemma-4-e4b", "lmstudio/qwen3.5-9b"],
        );
        for requested in [
            "",
            "claude-opus-5",
            "google/gemma-4-e4b",
            "lmstudio/qwen3.5-9b",
        ] {
            let id = e.resolve_model(requested).unwrap();
            assert!(
                eren_shared::is_provider_model_shape(&id),
                "{requested} resolved to {id}"
            );
        }
    }

    #[test]
    fn a_local_engine_makes_the_same_promises_as_the_thing_that_runs_it() {
        // Capabilities are delegated rather than restated; this pins that,
        // because a copy would eventually disagree and the disagreement would
        // show up as `vet` allowing a mode the process cannot honour.
        let e = LocalEngine::ollama(None);
        assert_eq!(e.capabilities(), OpenCodeEngine::default().capabilities());
        // The one that matters: neither can stop to ask, so Reviewed is
        // refused at the click rather than silently widened.
        assert!(!e.capabilities().interactive_permissions);
        assert!(crate::vet(&e, eren_shared::PermissionMode::Reviewed, false).is_err());
    }

    #[test]
    fn the_two_runtimes_are_distinct_engines_all_the_way_down() {
        let (o, l) = (LocalEngine::ollama(None), LocalEngine::lmstudio(None));
        assert_ne!(o.id(), l.id());
        assert_ne!(o.label(), l.label());
        assert_ne!(
            Runtime::Ollama.default_host(),
            Runtime::LmStudio.default_host()
        );
        // And neither collides with the agent that actually runs them.
        assert_ne!(o.id(), OpenCodeEngine::default().id());
    }
}
