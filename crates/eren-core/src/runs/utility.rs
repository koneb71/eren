//! Utility runs: one-shot, in-memory engine invocations that never write to
//! the DB (they only ask the machine's budget), worktrees, or MCP — used for meta-work like AI agent generation. The
//! engine still runs under the user's own CLI login (compliance unchanged).

use eren_engines::{Engine, RunSpec};
use eren_shared::{ErenEvent, ModelTier, PermissionMode, ReasoningEffort};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Everything that could reach outside the reply.
///
/// Deliberately wider than the chat assistant's list: chat is allowed to read
/// the checkout, a utility run has nothing to read. `Read`/`Grep`/`Glob` are in
/// here because the cwd is a scratch directory — a run that went looking would
/// find nothing and waste a turn doing it.
const DENIED: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Bash",
    "Read",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
    "Task",
];

pub async fn utility_run(
    db: &crate::Db,
    engine: Arc<dyn Engine>,
    model_id: String,
    prompt: String,
    effort: Option<ReasoningEffort>,
    timeout: Duration,
) -> anyhow::Result<String> {
    // Not a run on any card or project, but a call to a model all the same,
    // and paid for out of the same plan: a spent machine budget refuses it
    // like any other start. It leaves no row behind, so nothing counts it —
    // the budget can stop it, not see it.
    crate::budgets::check(db, &crate::budgets::Scope::default(), true).await?;
    // Every tool below is denied, and an engine with no mode in which nothing
    // writes would refuse that when it starts — said here instead, as the 409
    // a door gives for an engine that cannot honour the work.
    if !engine.capabilities().read_only_passes {
        return Err(crate::runs::orchestrator::CantHonour(format!(
            "{} has no read-only mode — it runs every tool or none — so it can't draft \
             this. Pick an engine that has one.",
            engine.label()
        ))
        .into());
    }
    let cwd = eren_shared::brand::home().join("tmp");
    tokio::fs::create_dir_all(&cwd).await?;

    let spec = RunSpec {
        cwd,
        prompt,
        model_tier: ModelTier::Complex,
        model_id,
        effort,
        resume_session_id: None,
        permission_mode: PermissionMode::Reviewed,
        allowed_tools: vec![], // no tools: pure generation
        append_system_prompt: None,
        // An empty allow-list pre-approves nothing; it does not forbid. The CLI
        // will still reach for `Bash`, and the only thing that stopped it here
        // was having no way to ask — which is a property of the plumbing, not a
        // rule. A utility run's whole contract is that it produces text and
        // changes nothing, so say so by name.
        denied_tools: DENIED.iter().map(|t| t.to_string()).collect(),
        mcp: Default::default(),
        run_key: "utility".to_string(),
        extra_read_dirs: vec![],
        permission_prompt_tool: false,
        extra_env: HashMap::new(),
    };

    // What a local runtime holds now, not at boot — see `Engine::refresh`.
    engine.refresh().await;
    let mut proc = engine.start(spec)?;
    let collect = async {
        let mut text_parts: Vec<String> = vec![];
        let mut result: Option<String> = None;
        while let Some(event) = proc.events.recv().await {
            match event {
                ErenEvent::AssistantText { text } => text_parts.push(text),
                ErenEvent::RunCompleted { result_text, .. } => {
                    result = Some(result_text);
                    break;
                }
                ErenEvent::RunFailed { reason } => anyhow::bail!("generation failed: {reason}"),
                ErenEvent::RateLimited { message, .. } => {
                    anyhow::bail!("rate limited: {message}")
                }
                _ => {}
            }
        }
        Ok(result
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| text_parts.join("\n")))
    };
    match tokio::time::timeout(timeout, collect).await {
        Ok(res) => res,
        Err(_) => {
            proc.kill();
            anyhow::bail!("generation timed out")
        }
    }
}

/// Extract the first JSON value (array or object) from model output that may
/// be wrapped in markdown fences or prose.
pub fn extract_json(text: &str) -> anyhow::Result<serde_json::Value> {
    let cleaned = text.trim();
    // Fast path: it's already pure JSON.
    if let Ok(v) = serde_json::from_str(cleaned) {
        return Ok(v);
    }
    // Strip ``` fences if present.
    let unfenced: String = if cleaned.contains("```") {
        cleaned
            .split("```")
            .nth(1)
            .map(|block| block.trim_start_matches("json").trim().to_string())
            .unwrap_or_else(|| cleaned.to_string())
    } else {
        cleaned.to_string()
    };
    if let Ok(v) = serde_json::from_str(&unfenced) {
        return Ok(v);
    }
    // Last resort: substring from the first bracket to the last matching one.
    for (open, close) in [('[', ']'), ('{', '}')] {
        if let (Some(start), Some(end)) = (cleaned.find(open), cleaned.rfind(close)) {
            if start < end {
                if let Ok(v) = serde_json::from_str(&cleaned[start..=end]) {
                    return Ok(v);
                }
            }
        }
    }
    anyhow::bail!("no parseable JSON in output")
}

#[cfg(test)]
mod tests {
    use super::extract_json;

    #[test]
    fn parses_plain_json() {
        let v = extract_json(r#"[{"name":"Reviewer"}]"#).unwrap();
        assert_eq!(v[0]["name"], "Reviewer");
    }

    #[test]
    fn parses_fenced_json() {
        let v =
            extract_json("Here you go:\n```json\n[{\"name\":\"Planner\"}]\n```\nEnjoy!").unwrap();
        assert_eq!(v[0]["name"], "Planner");
    }

    #[test]
    fn parses_json_embedded_in_prose() {
        let v = extract_json("Sure! [{\"name\":\"Tester\"}] — hope that helps.").unwrap();
        assert_eq!(v[0]["name"], "Tester");
    }

    #[test]
    fn rejects_garbage() {
        assert!(extract_json("no json here at all").is_err());
    }
}
