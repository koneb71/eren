//! Pure parser: one line of `claude --output-format stream-json` output →
//! zero or more normalized [`ErenEvent`]s. Pure so it is trivially
//! fixture-testable; all process I/O lives in the adapter.

use eren_shared::{ErenEvent, Usage};
use serde_json::Value;

pub fn parse_line(line: &str) -> Vec<ErenEvent> {
    let line = line.trim();
    if line.is_empty() {
        return vec![];
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        // Non-JSON noise on stdout (progress spinners, warnings) is ignored;
        // the adapter separately watches stderr for fatal errors.
        return vec![];
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") => parse_system(&v),
        Some("assistant") => parse_assistant(&v),
        Some("user") => parse_user(&v),
        Some("result") => parse_result(&v),
        Some("rate_limit_event") => parse_rate_limit_event(&v),
        _ => vec![],
    }
}

/// The CLI emits structured rate-limit telemetry (observed in v2.1.x):
/// `{"type":"rate_limit_event","rate_limit_info":{"status":"allowed",
///   "resetsAt":1785183600,"rateLimitType":"five_hour",...}}`.
/// `status:"allowed"` is routine telemetry; anything else means the run is
/// being throttled and the queue should back off until `resetsAt`.
fn parse_rate_limit_event(v: &Value) -> Vec<ErenEvent> {
    let info = v.get("rate_limit_info").cloned().unwrap_or(Value::Null);
    let raw = info
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("allowed");
    let status = eren_shared::LimitStatus::parse(raw);
    let reset_at = info
        .get("resetsAt")
        .and_then(Value::as_i64)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    let limit_type = info
        .get("rateLimitType")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let using_overage = info
        .get("isUsingOverage")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Always reported, whatever the status. This is the only place the user's
    // plan position is visible at all, and discarding the healthy case is what
    // made a failed run the first news anyone had of it.
    let mut out = vec![ErenEvent::UsageStatus {
        limit_type: limit_type.to_string(),
        status: status.as_str().to_string(),
        resets_at: reset_at,
        using_overage,
    }];

    // Only an actual refusal stops the run. `allowed_warning` is the CLI
    // saying "still serving you, but not for much longer" — abandoning work
    // on that is throwing away budget the user still has.
    if status.blocks() {
        out.push(ErenEvent::RateLimited {
            reset_at,
            message: format!("rate limit ({limit_type}) status: {raw}"),
        });
    }
    out
}

fn parse_system(v: &Value) -> Vec<ErenEvent> {
    if v.get("subtype").and_then(Value::as_str) == Some("init") {
        vec![ErenEvent::RunStarted {
            session_id: v
                .get("session_id")
                .and_then(Value::as_str)
                .map(String::from),
            model: v.get("model").and_then(Value::as_str).map(String::from),
        }]
    } else {
        vec![]
    }
}

fn parse_assistant(v: &Value) -> Vec<ErenEvent> {
    let mut events = vec![];
    let content = v
        .pointer("/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for block in &content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(ErenEvent::AssistantText {
                            text: text.to_string(),
                        });
                    }
                }
            }
            Some("tool_use") => {
                events.push(ErenEvent::ToolCall {
                    tool_name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    tool_use_id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = v.pointer("/message/usage") {
        events.push(ErenEvent::UsageUpdated {
            usage: parse_usage(usage),
        });
    }
    events
}

fn parse_user(v: &Value) -> Vec<ErenEvent> {
    let mut events = vec![];
    let content = v
        .pointer("/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for block in &content {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            events.push(ErenEvent::ToolResult {
                tool_use_id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                summary: summarize_tool_result(block),
            });
        }
    }
    events
}

/// What Claude Code says when asked to resume a session it does not have —
/// one deleted, made under another home, or lost with a container that kept
/// no `~/.claude`.
const SESSION_NOT_FOUND: &str = "No conversation found with session ID";

/// Did a run fail because the session it resumed no longer exists? So a
/// caller holding that id can let go of it instead of failing on it forever.
pub fn session_not_found(reason: &str) -> bool {
    reason.contains(SESSION_NOT_FOUND)
}

fn parse_result(v: &Value) -> Vec<ErenEvent> {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let result_text = v
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // A run that failed before the model said anything leaves `result` empty
    // and puts why in `errors` — a missing session, a bad flag. Without this
    // every one of them read as "error_during_execution" and nothing else.
    let errors = v
        .get("errors")
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();

    // The text is only a rate-limit signal when the run failed. On success it
    // is the model's last message — "returns 429 after five attempts" is a
    // summary of rate-limiting work, and reading it as a limit held a
    // finished run and ran it again, and again.
    let failed = is_error || subtype.starts_with("error");
    let result_text = if failed && result_text.is_empty() {
        errors
    } else {
        result_text
    };
    if rate_limit_signal(subtype) || (failed && rate_limit_signal(&result_text)) {
        return vec![ErenEvent::RateLimited {
            reset_at: None,
            message: result_text,
        }];
    }
    if failed {
        return vec![ErenEvent::RunFailed {
            reason: if result_text.is_empty() {
                format!("engine reported error ({subtype})")
            } else {
                result_text
            },
        }];
    }
    vec![ErenEvent::RunCompleted {
        session_id: v
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        usage: v.get("usage").map(parse_usage).unwrap_or_default(),
        result_text,
    }]
}

fn parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_creation_tokens: g("cache_creation_input_tokens"),
    }
}

fn summarize_tool_result(block: &Value) -> String {
    const MAX: usize = 400;
    let text = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    let mut out: String = text.chars().take(MAX).collect();
    if text.chars().count() > MAX {
        out.push('…');
    }
    out
}

pub use eren_shared::rate_limit_signal;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_line_yields_run_started() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-opus-5","tools":["Bash"]}"#;
        let events = parse_line(line);
        assert_eq!(
            events,
            vec![ErenEvent::RunStarted {
                session_id: Some("abc-123".into()),
                model: Some("claude-opus-5".into()),
            }]
        );
    }

    #[test]
    fn assistant_text_and_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let events = parse_line(line);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ErenEvent::AssistantText { text } if text == "On it."));
        assert!(matches!(&events[1], ErenEvent::ToolCall { tool_name, .. } if tool_name == "Bash"));
        assert!(matches!(
            &events[2],
            ErenEvent::UsageUpdated { usage } if usage.input_tokens == 10 && usage.output_tokens == 5
        ));
    }

    #[test]
    fn tool_result_is_summarized() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"file1\nfile2"}]}}"#;
        let events = parse_line(line);
        assert_eq!(
            events,
            vec![ErenEvent::ToolResult {
                tool_use_id: "toolu_1".into(),
                is_error: false,
                summary: "file1\nfile2".into(),
            }]
        );
    }

    #[test]
    fn success_result_yields_run_completed() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","session_id":"abc-123","total_cost_usd":0.042,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let events = parse_line(line);
        match &events[..] {
            [ErenEvent::RunCompleted {
                session_id,
                cost_usd,
                usage,
                result_text,
            }] => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(*cost_usd, Some(0.042));
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(result_text, "Done.");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cache_counters_survive_both_paths() {
        // These are the numbers that say a run was cheap — cached input costs
        // a fraction of fresh input — and they were parsed here long before
        // anything downstream kept them. Assert on both the per-message and
        // the final path, because a run is only ever priced by one of them.
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":180,"output_tokens":14,"cache_read_input_tokens":12000,"cache_creation_input_tokens":800}}}"#;
        match &parse_line(assistant)[..] {
            [_, ErenEvent::UsageUpdated { usage }] => {
                assert_eq!(usage.cache_read_tokens, 12_000);
                assert_eq!(usage.cache_creation_tokens, 800);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","session_id":"s","total_cost_usd":0.01,"usage":{"input_tokens":260,"output_tokens":59,"cache_read_input_tokens":12800,"cache_creation_input_tokens":800}}"#;
        match &parse_line(result)[..] {
            [ErenEvent::RunCompleted { usage, .. }] => {
                assert_eq!(usage.cache_read_tokens, 12_800);
                assert_eq!(usage.cache_creation_tokens, 800);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_missing_cache_field_is_zero_not_a_parse_failure() {
        // Older CLI builds omit them entirely; the run must still price.
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","session_id":"s","usage":{"input_tokens":10,"output_tokens":5}}"#;
        match &parse_line(line)[..] {
            [ErenEvent::RunCompleted { usage, .. }] => {
                assert_eq!(usage.cache_read_tokens, 0);
                assert_eq!(usage.input_tokens, 10);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn error_result_yields_run_failed() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
        let events = parse_line(line);
        assert!(matches!(&events[..], [ErenEvent::RunFailed { reason }] if reason == "boom"));
    }

    #[test]
    fn an_empty_error_result_says_what_its_errors_say() {
        // Recorded from Claude Code 2.1.259, resuming a session it did not have.
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"result":"","session_id":"3f0c2a8e","errors":["No conversation found with session ID: 3f0c2a8e"]}"#;
        let events = parse_line(line);
        let [ErenEvent::RunFailed { reason }] = &events[..] else {
            panic!("unexpected: {events:?}");
        };
        assert_eq!(reason, "No conversation found with session ID: 3f0c2a8e");
        assert!(session_not_found(reason));
        assert!(!session_not_found("boom"));

        // Neither: the subtype is still better than nothing.
        let bare = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        assert!(matches!(
            &parse_line(bare)[..],
            [ErenEvent::RunFailed { reason }] if reason == "engine reported error (error_during_execution)"
        ));
    }

    #[test]
    fn rate_limit_result_is_detected() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You have hit your usage limit. Limit resets at 3pm."}"#;
        let events = parse_line(line);
        assert!(matches!(&events[..], [ErenEvent::RateLimited { .. }]));
    }

    #[test]
    fn a_warning_reports_usage_without_stopping_the_run() {
        // The regression that cost real work: `allowed_warning` produced a
        // RateLimited event, which aborts a utility run outright.
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785183600,"rateLimitType":"seven_day"},"session_id":"s1"}"#;
        let events = parse_line(line);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ErenEvent::RateLimited { .. })),
            "a warning must not stop the run: {events:?}"
        );
        assert!(matches!(
            &events[0],
            ErenEvent::UsageStatus { status, limit_type, .. }
                if status == "warning" && limit_type == "seven_day"
        ));
    }

    #[test]
    fn a_healthy_ping_is_reported_rather_than_discarded() {
        // Shape recorded from claude CLI 2.1.205 on 2026-07-28.
        //
        // This used to produce nothing at all, which is why the first news
        // anyone had of their plan position was a run that failed. It is
        // telemetry — it must still not stop anything.
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1785183600,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false},"session_id":"s1"}"#;
        let events = parse_line(line);
        assert!(!events
            .iter()
            .any(|e| matches!(e, ErenEvent::RateLimited { .. })));
        match &events[..] {
            [ErenEvent::UsageStatus {
                limit_type,
                status,
                resets_at,
                using_overage,
            }] => {
                assert_eq!(limit_type, "five_hour");
                assert_eq!(status, "allowed");
                assert_eq!(resets_at.unwrap().timestamp(), 1785183600);
                assert!(!using_overage);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rate_limit_event_blocked_carries_reset_time() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1785183600,"rateLimitType":"five_hour"},"session_id":"s1"}"#;
        let events = parse_line(line);
        // Both: the position is recorded *and* the run is stopped. Reporting
        // usage must not have cost us the backoff.
        match &events[..] {
            [ErenEvent::UsageStatus { status, .. }, ErenEvent::RateLimited { reset_at, message }] =>
            {
                assert_eq!(status, "blocked");
                assert_eq!(reset_at.unwrap().timestamp(), 1785183600);
                assert!(message.contains("five_hour"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn malformed_and_noise_lines_are_ignored() {
        assert!(parse_line("").is_empty());
        assert!(parse_line("not json at all").is_empty());
        assert!(parse_line(r#"{"type":"unknown_thing"}"#).is_empty());
        assert!(parse_line(r#"{"no_type":true}"#).is_empty());
    }
}
