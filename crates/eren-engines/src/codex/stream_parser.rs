//! `codex exec --json` emits JSON Lines; this turns them into `ErenEvent`.
//!
//! ## Now checked against a real transcript
//!
//! This file was first written from OpenAI's documentation, without the binary
//! — the exception to the repository's rule. It has since been run against
//! `codex-cli 0.147.0`, and the fixtures in the tests below are those runs,
//! copied verbatim. The envelope names it guessed turned out to be right; the
//! two things it got wrong are fixed here and worth knowing:
//!
//! - **An `error` is an item, not only an event.** A run whose model id is
//!   unknown emits
//!   `{"type":"item.completed","item":{"type":"error","message":"…"}}` and
//!   then *completes successfully*. Treating that as a failure would fail a
//!   run that worked; dropping it, which is what happened before, threw away
//!   the only explanation of one that did not. So it is remembered and used as
//!   the reason **only when the process also exits non-zero**.
//! - **A turn can contain several `agent_message` items.** Text is
//!   accumulated, never replaced, or the card's summary shows the last
//!   sentence of the answer instead of the answer.
//!
//! Two properties are kept from the documentation-only era, because they cost
//! nothing and the stream is still not fully specified:
//!
//! - **Unknown lines are ignored, never fatal.** `parse_line` returns an empty
//!   vector for anything it cannot read, including lines that are not JSON.
//!   The failure mode is a thinner event stream, not a dead run.
//! - **Both event spellings are accepted.** Observed output uses
//!   `thread.started`/`item.completed`; the docs also describe a JSON-RPC-ish
//!   `{"method":"turn/started"}`. The type is read from `type` or `method` and
//!   `.`/`/` are treated as the same separator.

use eren_shared::{ErenEvent, Usage};
use serde_json::Value;

/// What has to survive between lines.
pub struct StreamState {
    /// From `thread.started`, and what a later run resumes with.
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// The assistant's text, accumulated so the terminal event can carry the
    /// whole answer the way `RunCompleted.result_text` expects.
    text: String,
    usage: Usage,
    /// Set by an explicit failure event, so `finish` can report the reason
    /// rather than only the exit code.
    failure: Option<String>,
    /// The last `error` *item*, which is not the same thing.
    ///
    /// Codex emits one for a merely degraded turn — an unknown model id, say —
    /// and then finishes successfully. Promoting that to a failure would fail
    /// working runs, so it is only ever used to explain a process that also
    /// exited non-zero.
    last_error: Option<String>,
}

impl StreamState {
    pub fn new(model: Option<String>) -> Self {
        Self {
            session_id: None,
            model,
            text: String::new(),
            usage: Usage::default(),
            failure: None,
            last_error: None,
        }
    }

    /// The terminal event, decided by the process's exit status.
    ///
    /// Like OpenCode and unlike Claude Code: there is no single line that
    /// says how the whole run went, so the exit code is the authority. A
    /// failure event seen mid-stream supplies the *reason* when there is one,
    /// because "exited non-zero" is not something a person can act on.
    pub fn finish(self, exit_ok: bool) -> ErenEvent {
        if exit_ok && self.failure.is_none() {
            ErenEvent::RunCompleted {
                session_id: self.session_id.unwrap_or_default(),
                // Codex reports tokens, not money. Left None rather than
                // guessed from a price table that would go stale — the same
                // choice the OpenCode adapter makes, and the reason the
                // activity view counts unpriced runs separately instead of
                // folding them in as zero.
                cost_usd: None,
                usage: self.usage,
                result_text: self.text,
            }
        } else {
            // An error item is only an explanation once the process has
            // actually failed — see `last_error`.
            let reason = self
                .failure
                .or(self.last_error)
                .unwrap_or_else(|| "codex exited without completing the turn".to_string());
            // Codex reports a spent plan as an ordinary failed turn ("You've
            // hit your usage limit", a 429). Read as a failure, the card went
            // to review for a person to retry by hand; read as a limit, the
            // queue waits it out and tries again.
            if eren_shared::rate_limit_signal(&reason) {
                ErenEvent::RateLimited {
                    reset_at: None,
                    message: reason,
                }
            } else {
                ErenEvent::RunFailed { reason }
            }
        }
    }
}

/// Read the event type from either spelling, normalised to dots.
fn kind(v: &Value) -> Option<String> {
    let raw = v
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| v.get("method").and_then(Value::as_str))?;
    Some(raw.replace('/', "."))
}

/// The payload, which is at the top level in one spelling and under `params`
/// in the other.
fn payload<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
        .or_else(|| v.get("params").and_then(|p| p.get(key)))
}

/// Text out of an item, whichever field it used.
fn item_text(item: &Value) -> Option<String> {
    for key in ["text", "content", "message", "delta"] {
        if let Some(s) = item.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn usage_from(v: &Value) -> Option<Usage> {
    let u = payload(v, "usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input_tokens: n("input_tokens") + n("inputTokens"),
        output_tokens: n("output_tokens") + n("outputTokens"),
        cache_read_tokens: n("cached_input_tokens") + n("cachedInputTokens"),
        cache_creation_tokens: 0,
    })
}

/// One line in, zero or more events out.
///
/// Zero is the normal answer for a line this does not recognise. That is the
/// whole robustness story for an adapter written without the binary: a stream
/// full of unknown shapes degrades to "the run produced no detail", which is
/// survivable, rather than to an error, which is not.
pub fn parse_line(line: &str, state: &mut StreamState) -> Vec<ErenEvent> {
    let line = line.trim();
    if line.is_empty() {
        return vec![];
    }
    let Ok(v): Result<Value, _> = serde_json::from_str(line) else {
        // Not JSON. `codex exec` documents progress on stderr and JSON on
        // stdout, but a banner or a warning on the wrong stream must not kill
        // a run.
        return vec![];
    };
    let Some(kind) = kind(&v) else { return vec![] };
    let mut out = Vec::new();

    if let Some(usage) = usage_from(&v) {
        if usage.input_tokens + usage.output_tokens > 0 {
            state.usage = usage.clone();
            out.push(ErenEvent::UsageUpdated { usage });
        }
    }

    match kind.as_str() {
        "thread.started" => {
            let id = payload(&v, "thread_id")
                .or_else(|| payload(&v, "threadId"))
                .or_else(|| payload(&v, "thread").and_then(|t| t.get("id")))
                .and_then(Value::as_str)
                .map(str::to_string);
            state.session_id = id.clone();
            out.push(ErenEvent::RunStarted {
                session_id: id,
                model: state.model.clone(),
            });
        }
        // A turn starting is not the run starting — a resumed thread has many.
        // Nothing to emit; the run already announced itself.
        "turn.started" => {}
        "item.started" | "item.completed" | "item.updated" => {
            let Some(item) = payload(&v, "item") else {
                return out;
            };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match item_type {
                // Only on completion: an `item.started` for a message carries
                // no text yet, and emitting on both would duplicate it.
                "agentMessage" | "agent_message" | "assistantMessage" => {
                    if kind.ends_with(".completed") {
                        if let Some(text) = item_text(item) {
                            // Separate messages stay separate paragraphs, or
                            // "…sandbox mode." and "ls output…" run together.
                            if !state.text.is_empty() {
                                state.text.push_str("\n\n");
                            }
                            state.text.push_str(&text);
                            out.push(ErenEvent::AssistantText { text });
                        }
                    }
                }
                // An error item, which is a diagnostic and not necessarily
                // the end: a run naming an unknown model emits one and then
                // completes. Remembered rather than emitted, so `finish` can
                // explain a non-zero exit without inventing a failure.
                "error" => {
                    if let Some(msg) = item_text(item) {
                        tracing::warn!(codex_error = %msg, "codex reported an error item");
                        state.last_error = Some(msg);
                    }
                }
                "commandExecution" | "command_execution" | "fileChange" | "file_change"
                | "toolCall" | "tool_call" | "mcpToolCall" | "mcp_tool_call" => {
                    let tool_name = item
                        .get("name")
                        .or_else(|| item.get("tool"))
                        .and_then(Value::as_str)
                        .unwrap_or(item_type)
                        .to_string();
                    if kind.ends_with(".started") {
                        out.push(ErenEvent::ToolCall {
                            tool_name,
                            tool_use_id: id,
                            input: item
                                .get("input")
                                .or_else(|| item.get("command"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        });
                    } else if kind.ends_with(".completed") {
                        let failed = item
                            .get("status")
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.eq_ignore_ascii_case("failed"));
                        out.push(ErenEvent::ToolResult {
                            tool_use_id: id,
                            is_error: failed,
                            summary: item_text(item).unwrap_or_else(|| tool_name.clone()),
                        });
                    }
                }
                // A reasoning item is not shown as the answer: it is not what
                // the user asked for, and folding it into result_text would
                // put the thinking in the card's summary.
                _ => {}
            }
        }
        "turn.failed" | "error" => {
            let reason = payload(&v, "error")
                .and_then(|e| e.get("message").and_then(Value::as_str))
                .or_else(|| payload(&v, "message").and_then(Value::as_str))
                .unwrap_or("codex reported an error")
                .to_string();
            // Recorded rather than emitted: `finish` owns the terminal event,
            // so a run has exactly one, decided by the exit status.
            state.failure = Some(reason);
        }
        "turn.completed" => {}
        _ => {}
    }
    out
}

#[cfg(test)]
mod recorded {
    //! Verbatim lines from `codex-cli 0.147.0` on 2026-08-17, trimmed only
    //! where a value was long. These are the transcript this adapter was
    //! missing for its whole existence.
    use super::*;

    /// `codex exec --json --skip-git-repo-check -s read-only "Reply with
    /// exactly the word pineapple..."`
    const SIMPLE: &[&str] = &[
        r#"{"type":"thread.started","thread_id":"01a00ea5-3f26-7842-9bc8-0796b79dca5b"}"#,
        r#"{"type":"turn.started"}"#,
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"pineapple"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":13397,"cached_input_tokens":4480,"cache_write_input_tokens":0,"output_tokens":6,"reasoning_output_tokens":0}}"#,
    ];

    /// The same, with a shell tool call — note two `agent_message` items in
    /// one turn, which is why text accumulates.
    const WITH_TOOL: &[&str] = &[
        r#"{"type":"thread.started","thread_id":"01a00eae-1fa3-75a0-8d96-e71440cdf600"}"#,
        r#"{"type":"turn.started"}"#,
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I am running under read-only sandbox mode."}}"#,
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"run1.jsonl\n","exit_code":0,"status":"completed"}}"#,
        r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"ls output: run1.jsonl"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":26160,"cached_input_tokens":22272,"cache_write_input_tokens":0,"output_tokens":102,"reasoning_output_tokens":0}}"#,
    ];

    /// `-m definitely-not-a-model-xyz`. The turn continued and the run
    /// completed successfully after this line.
    const UNKNOWN_MODEL: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `definitely-not-a-model-xyz` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}}"#;

    fn drive(lines: &[&str]) -> (Vec<ErenEvent>, StreamState) {
        let mut state = StreamState::new(Some("gpt-5.5".into()));
        let mut out = vec![];
        for l in lines {
            out.extend(parse_line(l, &mut state));
        }
        (out, state)
    }

    #[test]
    fn a_real_run_yields_a_session_the_text_and_the_tokens() {
        let (events, state) = drive(SIMPLE);
        assert_eq!(
            state.session_id.as_deref(),
            Some("01a00ea5-3f26-7842-9bc8-0796b79dca5b"),
            "the thread id is what a follow-up turn resumes with"
        );
        assert!(matches!(events[0], ErenEvent::RunStarted { .. }));
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::AssistantText { text } if text == "pineapple")));
        match state.finish(true) {
            ErenEvent::RunCompleted {
                usage,
                result_text,
                session_id,
                ..
            } => {
                assert_eq!(result_text, "pineapple");
                assert_eq!(usage.input_tokens, 13397);
                assert_eq!(usage.output_tokens, 6);
                assert_eq!(usage.cache_read_tokens, 4480);
                assert!(!session_id.is_empty());
            }
            other => panic!("expected a completed run, got {other:?}"),
        }
    }

    #[test]
    fn a_turn_with_several_messages_keeps_all_of_them() {
        // Codex answers, runs a command, then answers again. Replacing rather
        // than accumulating would put only the last sentence in the card.
        let (events, state) = drive(WITH_TOOL);
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::ToolCall { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            ErenEvent::ToolResult {
                is_error: false,
                ..
            }
        )));
        match state.finish(true) {
            ErenEvent::RunCompleted { result_text, .. } => {
                assert!(result_text.contains("read-only sandbox mode"));
                assert!(result_text.contains("ls output"));
            }
            other => panic!("expected a completed run, got {other:?}"),
        }
    }

    #[test]
    fn several_messages_are_kept_as_separate_paragraphs() {
        let (_, state) = drive(WITH_TOOL);
        let ErenEvent::RunCompleted { result_text, .. } = state.finish(true) else {
            panic!("expected a completed run")
        };
        assert_eq!(
            result_text,
            "I am running under read-only sandbox mode.\n\nls output: run1.jsonl"
        );
    }

    #[test]
    fn an_error_item_does_not_fail_a_run_that_succeeded() {
        // The regression this fixture exists for. Codex emits an error item
        // for a merely degraded turn and then finishes; promoting it to a
        // failure would fail working runs.
        let (_, state) = drive(&[SIMPLE[0], UNKNOWN_MODEL, SIMPLE[2], SIMPLE[3]]);
        assert!(matches!(state.finish(true), ErenEvent::RunCompleted { .. }));
    }

    #[test]
    fn but_it_explains_one_that_did_fail() {
        // Dropping it, which is what used to happen, left "codex exited
        // without completing the turn" as the only thing a person was told.
        let (_, state) = drive(&[SIMPLE[0], UNKNOWN_MODEL]);
        match state.finish(false) {
            ErenEvent::RunFailed { reason } => {
                assert!(reason.contains("Model metadata"), "got: {reason}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(lines: &[&str]) -> (Vec<ErenEvent>, StreamState) {
        let mut state = StreamState::new(Some("gpt-5-codex".into()));
        let mut out = vec![];
        for l in lines {
            out.extend(parse_line(l, &mut state));
        }
        (out, state)
    }

    #[test]
    fn reads_the_documented_dotted_spelling() {
        let (events, state) = drive(&[
            r#"{"type":"thread.started","thread_id":"th_1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"type":"agentMessage","id":"m1","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":4}}"#,
        ]);
        assert_eq!(state.session_id.as_deref(), Some("th_1"));
        assert!(matches!(events[0], ErenEvent::RunStarted { .. }));
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::AssistantText { text } if text == "done")));
        let ErenEvent::RunCompleted {
            result_text,
            usage,
            cost_usd,
            ..
        } = state.finish(true)
        else {
            panic!("expected completion")
        };
        assert_eq!(result_text, "done");
        assert_eq!(usage.output_tokens, 4);
        // Codex reports tokens, not money. Guessing a price would be worse
        // than saying nothing.
        assert_eq!(cost_usd, None);
    }

    #[test]
    fn reads_the_json_rpc_spelling_too() {
        // The docs show both; betting on one and being wrong would produce a
        // run with no events at all.
        let (events, state) = drive(&[
            r#"{"method":"thread/started","params":{"thread":{"id":"th_2"}}}"#,
            r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"m","text":"hi"}}}"#,
        ]);
        assert_eq!(state.session_id.as_deref(), Some("th_2"));
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::AssistantText { text } if text == "hi")));
    }

    #[test]
    fn a_tool_call_becomes_a_call_and_a_result() {
        let (events, _) = drive(&[
            r#"{"type":"item.started","item":{"type":"commandExecution","id":"c1","name":"bash","command":"ls"}}"#,
            r#"{"type":"item.completed","item":{"type":"commandExecution","id":"c1","status":"failed","text":"no such file"}}"#,
        ]);
        assert!(
            matches!(&events[0], ErenEvent::ToolCall { tool_use_id, .. } if tool_use_id == "c1")
        );
        assert!(
            matches!(&events[1], ErenEvent::ToolResult { is_error, tool_use_id, .. }
                if *is_error && tool_use_id == "c1")
        );
    }

    #[test]
    fn a_started_message_does_not_duplicate_its_completed_text() {
        let (events, state) = drive(&[
            r#"{"type":"item.started","item":{"type":"agentMessage","id":"m","text":"partial"}}"#,
            r#"{"type":"item.completed","item":{"type":"agentMessage","id":"m","text":"partial and final"}}"#,
        ]);
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ErenEvent::AssistantText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["partial and final"]);
        let ErenEvent::RunCompleted { result_text, .. } = state.finish(true) else {
            panic!()
        };
        assert_eq!(result_text, "partial and final");
    }

    #[test]
    fn an_error_event_supplies_the_reason_and_finish_owns_the_verdict() {
        let (events, state) = drive(&[
            r#"{"type":"thread.started","thread_id":"t"}"#,
            r#"{"type":"turn.failed","error":{"message":"model refused"}}"#,
        ]);
        // Not emitted mid-stream: a run gets exactly one terminal event.
        assert!(!events
            .iter()
            .any(|e| matches!(e, ErenEvent::RunFailed { .. })));
        let ErenEvent::RunFailed { reason } = state.finish(true) else {
            panic!("a failure seen in the stream must beat a zero exit code")
        };
        assert_eq!(reason, "model refused");
    }

    #[test]
    fn a_spent_plan_is_a_rate_limit_not_a_failure() {
        for line in [
            r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit. Try again later."}}"#,
            r#"{"type":"error","message":"stream error: unexpected status 429 Too Many Requests"}"#,
        ] {
            let (_, state) = drive(&[r#"{"type":"thread.started","thread_id":"t"}"#, line]);
            match state.finish(false) {
                ErenEvent::RateLimited { reset_at, message } => {
                    assert_eq!(reset_at, None);
                    assert!(!message.is_empty());
                }
                other => panic!("expected a rate limit, got {other:?}"),
            }
        }
        // An error item that says so explains a failed exit the same way.
        let (_, state) = drive(&[
            r#"{"type":"item.completed","item":{"id":"e","type":"error","message":"429 Too Many Requests"}}"#,
        ]);
        assert!(matches!(state.finish(false), ErenEvent::RateLimited { .. }));
    }

    #[test]
    fn a_nonzero_exit_fails_even_with_no_error_line() {
        let (_, state) = drive(&[r#"{"type":"thread.started","thread_id":"t"}"#]);
        assert!(matches!(state.finish(false), ErenEvent::RunFailed { .. }));
    }

    #[test]
    fn unknown_and_malformed_lines_are_survivable() {
        // The whole robustness story for an adapter written without the
        // binary: a shape it has never seen must thin the stream, not kill it.
        let (events, state) = drive(&[
            "not json at all",
            "",
            r#"{"type":"some.future.event","payload":{"whatever":1}}"#,
            r#"{"no_type_field":true}"#,
            r#"{"type":"item.completed","item":{"type":"agentMessage","id":"m","text":"still here"}}"#,
        ]);
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::AssistantText { text } if text == "still here")));
        assert!(matches!(state.finish(true), ErenEvent::RunCompleted { .. }));
    }
}
