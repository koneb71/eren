//! One rule for "is this an auth secret?", shared by everything that can put
//! an environment variable in front of a spawned agent.
//!
//! This exists to keep compliance invariant 3 — *never set authentication
//! environment variables on spawned processes* — true as the number of
//! engines and providers grows. It replaces two hand-maintained copies of an
//! Anthropic-only prefix list (one in the Claude adapter, one in
//! `mcp_servers`), which would have leaked the moment a second provider
//! existed: nothing in `["ANTHROPIC_", "CLAUDE_CODE_OAUTH"]` stops
//! `OPENAI_API_KEY` or `GOOGLE_APPLICATION_CREDENTIALS`.
//!
//! The check is deliberately broad. A false positive costs someone one
//! confusing refusal; a false negative hands a credential to a subprocess,
//! which is the failure this whole project is organised to avoid.

/// Secrets **Eren itself** puts in its own environment.
///
/// A different problem from the one below, and easy to miss: the guard here
/// only ever ran over variables an adapter was asked to *set*. A spawned CLI
/// inherits the server's whole environment, so the moment Eren gained a
/// credential of its own — object storage, for the knowledge base — every
/// agent it launched would have been handed it, having passed no check at all.
///
/// These are stripped from every child. Deliberately narrow: it names only
/// what Eren owns. Stripping the *user's* variables would be overreach and
/// would break real setups, since OpenCode authenticates some providers from
/// the environment on purpose.
///
/// Named without their prefix, because each is read under two names (see
/// [`crate::brand::var`]) and both have to go: [`own_secrets`] spells them out.
pub const OWN_SECRETS: &[&str] = &["S3_ACCESS_KEY", "S3_SECRET_KEY", "ACCESS_TOKEN"];

/// Eren's own, read under exactly one name and stripped under exactly that.
///
/// `DATABASE_URL` is the database Eren itself runs on — any `DATABASE_URL`
/// in its environment is the one `serve` connected to — so a child that
/// inherited it held Eren's whole state, password included, and a project's
/// test suite run as a check would have pointed its fixtures at it. Nothing
/// Eren starts needs it: the managed Postgres is configured by its own
/// arguments, and a preview gets only what its recipe declares. It is still
/// not auth-shaped ([`is_auth_env`] answers whether a *requested* variable
/// may be set, and a person may hand an MCP server a database of its own).
pub const OWN_UNPREFIXED: &[&str] = &["DATABASE_URL"];

/// Every variable name [`OWN_SECRETS`] and [`OWN_UNPREFIXED`] can be set
/// under.
pub fn own_secrets() -> impl Iterator<Item = String> {
    OWN_SECRETS
        .iter()
        .flat_map(|key| crate::brand::env_names(key))
        .chain(OWN_UNPREFIXED.iter().map(|key| key.to_string()))
}

/// A child process, minus the secrets Eren itself holds.
///
/// **The only way anything in this workspace starts a process.** Stripping
/// [`OWN_SECRETS`] used to be something each spawn site remembered, and
/// seven did; the rest — git, whose repository hooks inherit the environment,
/// docker, every engine's `--version` probe, and the MCP "test" button, which
/// runs somebody's `npx` package — handed the object-storage keys to whatever
/// they started. Now there is nothing to remember. `Command::new` is refused by
/// `clippy.toml` and by the test below, which reads the workspace's source.
#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    for key in own_secrets() {
        cmd.env_remove(key);
    }
    cmd
}

/// Fragments that make a name look like a secret regardless of vendor.
const SECRET_SUBSTRINGS: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "_TOKEN",
    "TOKEN_",
    "_SECRET",
    "SECRET_",
    "PASSWORD",
    "PASSWD",
    "OAUTH",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "SESSION_KEY",
    "ACCESS_KEY",
];

/// Vendor namespaces we refuse wholesale.
///
/// `OPENCODE_` earns its place twice over: besides credentials it carries
/// `OPENCODE_CONFIG*` and `OPENCODE_PERMISSION`, which can rewrite the very
/// permission rules an adapter generates. The adapter sets its own config
/// variable deliberately, *after* this check has run over user-supplied
/// values.
const VENDOR_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_CODE_OAUTH",
    "OPENAI_",
    "AZURE_",
    "AWS_",
    "GOOGLE_",
    "GEMINI_",
    "VERTEX_",
    "GROQ_",
    "MISTRAL_",
    "DEEPSEEK_",
    "XAI_",
    "OPENROUTER_",
    "TOGETHER_",
    "FIREWORKS_",
    "CEREBRAS_",
    "PERPLEXITY_",
    "COHERE_",
    "HUGGING",
    "HF_",
    "REPLICATE_",
    "OLLAMA_",
    "OPENCODE_",
    // The CLIs added with Gemini, Cursor, Qwen Code and Amp. Each namespace
    // carries its own key and, for some, settings that would rewrite how the
    // CLI runs — refused wholesale for the reason OPENCODE_ is.
    "CURSOR_",
    "AGENT_CLI_",
    "QWEN_",
    "DASHSCOPE_",
    "AMP_",
];

/// Would setting this variable hand an agent a credential — or let it rewrite
/// the rules we generated for it?
pub fn is_auth_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    VENDOR_PREFIXES.iter().any(|p| upper.starts_with(p))
        || SECRET_SUBSTRINGS.iter().any(|s| upper.contains(s))
}

/// The refusal message, phrased so the reader understands it is a design
/// stance rather than a bug.
pub fn auth_env_refusal(key: &str) -> String {
    format!(
        "refusing to set {key}: Eren runs on your CLI's own login and never \
         handles credentials, so it will not put auth-shaped environment \
         variables in front of a spawned agent"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newer_engines_keys_are_refused_too() {
        for key in [
            "CURSOR_API_KEY",
            "AGENT_CLI_TOKEN",
            "QWEN_API_KEY",
            "DASHSCOPE_API_KEY",
            "AMP_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_CLOUD_PROJECT",
        ] {
            assert!(is_auth_env(key), "{key}");
        }
        // And nothing ordinary is caught by the new prefixes.
        for key in [
            "EREN_RUN_ID",
            "MCP_TOOL_TIMEOUT",
            "PATH",
            "AMPLIFY",
            "CURSORY",
        ] {
            assert!(!is_auth_env(key), "{key}");
        }
    }

    #[test]
    fn the_original_anthropic_names_are_still_caught() {
        assert!(is_auth_env("ANTHROPIC_API_KEY"));
        assert!(is_auth_env("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn a_second_provider_no_longer_walks_straight_through() {
        // The whole reason this module exists: the old prefix list was
        // Anthropic-only, so every one of these was permitted.
        for key in [
            "OPENAI_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "OPENROUTER_API_KEY",
            "GEMINI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "XAI_API_KEY",
            "OLLAMA_HOST",
        ] {
            assert!(is_auth_env(key), "{key} should be refused");
        }
    }

    #[test]
    fn opencode_config_vars_are_refused_because_they_rewrite_our_rules() {
        // Not secrets, but they can override the generated permission config.
        assert!(is_auth_env("OPENCODE_CONFIG_CONTENT"));
        assert!(is_auth_env("OPENCODE_PERMISSION"));
        assert!(is_auth_env("OPENCODE_API_KEY"));
    }

    #[test]
    fn unknown_vendors_are_caught_by_shape() {
        // A provider nobody has added to the list yet still gets stopped when
        // its variable is named like a secret.
        assert!(is_auth_env("ACME_API_KEY"));
        assert!(is_auth_env("some_service_token"));
        assert!(is_auth_env("DB_PASSWORD"));
        assert!(is_auth_env("MY_OAUTH_THING"));
    }

    #[test]
    fn ordinary_variables_still_pass() {
        for key in [
            "EREN_RUN_ID",
            "EREN_STEP",
            "MCP_TOOL_TIMEOUT",
            "MCP_TIMEOUT",
            "PWD",
            "PATH",
            "NODE_ENV",
            "DATABASE_URL",
        ] {
            assert!(!is_auth_env(key), "{key} should be allowed");
        }
    }

    #[test]
    fn the_check_ignores_case() {
        assert!(is_auth_env("anthropic_api_key"));
        assert!(is_auth_env("OpenAI_Api_Key"));
    }
}

#[cfg(test)]
mod own_secret_tests {
    use super::*;

    #[test]
    fn a_command_does_not_inherit_what_eren_owns() {
        let cmd = command("true");
        let names: Vec<String> = own_secrets().collect();
        // Both spellings of each prefixed key, and the unprefixed ones once.
        assert_eq!(
            names.len(),
            OWN_SECRETS.len() * 2 + OWN_UNPREFIXED.len(),
            "{names:?}"
        );
        for key in names {
            let removed = cmd
                .as_std()
                .get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new(&key) && v.is_none());
            assert!(removed, "{key} is inherited by a child");
        }
    }

    /// Eren's own database is stripped under its one name — and only that
    /// name, so nothing that merely contains it is caught.
    #[test]
    fn a_command_does_not_inherit_erens_database() {
        let cmd = command("true");
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(removed.iter().any(|k| k == "DATABASE_URL"), "{removed:?}");
        assert!(!removed
            .iter()
            .any(|k| k.contains("DATABASE_URL") && k != "DATABASE_URL"));
        // A different question, with a different answer: a person may still
        // hand a tool a database of its own.
        assert!(!is_auth_env("DATABASE_URL"));
    }

    /// The rule above only holds if nothing goes around it. This is the check
    /// that runs everywhere `cargo test` does — clippy's `disallowed-methods`
    /// says the same thing in an editor, but CI does not block on clippy yet.
    #[test]
    fn nothing_in_the_workspace_spawns_a_process_any_other_way() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/");
        let mut offenders = vec![];
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") || path.ends_with("env_guard.rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                for (n, line) in source.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    if code.contains("Command::new(") {
                        offenders.push(format!("{}:{}", path.display(), n + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "spawn through eren_shared::env_guard::command instead: {offenders:#?}"
        );
    }

    /// Whatever credential Eren decides to own, it must also recognise as a
    /// secret — otherwise a future addition could be passed through
    /// `extra_env` and sail past the very check that exists to stop it.
    /// [`OWN_UNPREFIXED`] is the exception, and says why.
    #[test]
    fn everything_eren_owns_reads_as_a_secret() {
        for key in OWN_SECRETS.iter().flat_map(|k| crate::brand::env_names(k)) {
            assert!(is_auth_env(&key), "{key} is not recognised as a secret");
        }
    }

    /// The user's own provider credentials are theirs. OpenCode authenticates
    /// some providers from the environment by design, so stripping these would
    /// break working installs to solve a problem Eren didn't create.
    #[test]
    fn the_users_own_credentials_are_left_alone() {
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                !own_secrets().any(|own| own == key),
                "{key} belongs to the user, not to eren"
            );
        }
    }
}
