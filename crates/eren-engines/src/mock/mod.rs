//! Mock engine: replays recorded stream-json fixtures with configurable
//! pacing. The backbone of all testing — zero model usage.

use crate::claude::stream_parser;
use crate::{Capabilities, Engine, EngineInfo, EngineProcess, ProcessHandle, RunSpec};
use async_trait::async_trait;
use eren_shared::ErenEvent;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct MockEngine {
    /// Raw stream-json fixture content (one JSON object per line).
    pub fixture: String,
    /// Delay between replayed lines; zero in unit tests, ~100ms for demo UX.
    pub line_delay: Duration,
}

impl MockEngine {
    pub fn from_fixture(fixture: impl Into<String>) -> Self {
        Self {
            fixture: fixture.into(),
            line_delay: Duration::ZERO,
        }
    }

    /// Built-in happy-path transcript for demos and E2E tests.
    pub fn demo() -> Self {
        Self {
            fixture: include_str!("fixtures/simple_task.jsonl").to_string(),
            line_delay: Duration::from_millis(150),
        }
    }
}

#[async_trait]
impl Engine for MockEngine {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn label(&self) -> &'static str {
        "Mock"
    }

    fn capabilities(&self) -> Capabilities {
        // Claims everything so tests exercising any surface aren't blocked by
        // the capability gate; it replays fixtures rather than honouring any
        // of it.
        Capabilities {
            interactive_permissions: true,
            structured_rate_limit: true,
            resume_sessions: true,
            append_system_prompt: true,
            fixed_model_catalog: true,
            reports_cost: true,
            // The fixture replays; nothing it does writes.
            enforces_denied_tools: true,
            mcp_tools: true,
            auto_edit: true,
            read_only_passes: true,
        }
    }

    async fn detect(&self) -> Option<EngineInfo> {
        Some(EngineInfo {
            version: "mock-0".to_string(),
            authenticated: true,
            providers: vec![],
            models: vec![],
        })
    }

    fn start(&self, _spec: RunSpec) -> anyhow::Result<EngineProcess> {
        let (tx, rx) = mpsc::channel::<ErenEvent>(256);
        let fixture = self.fixture.clone();
        let delay = self.line_delay;
        tokio::spawn(async move {
            for line in fixture.lines() {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                for event in stream_parser::parse_line(line) {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(EngineProcess::new(rx, Box::new(MockHandle)))
    }

    /// A recorded transcript has no session to go back to.
    fn interactive_resume_argv(&self, _session_id: &str) -> Option<Vec<String>> {
        None
    }
}

struct MockHandle;

#[async_trait]
impl ProcessHandle for MockHandle {
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use eren_shared::{ModelTier, PermissionMode};
    use std::collections::HashMap;

    fn spec() -> RunSpec {
        RunSpec {
            cwd: std::env::temp_dir(),
            prompt: "demo".into(),
            model_tier: ModelTier::Medium,
            model_id: "claude-opus-5".into(),
            effort: None,
            resume_session_id: None,
            permission_mode: PermissionMode::Reviewed,
            allowed_tools: vec![],
            denied_tools: vec![],
            append_system_prompt: None,
            mcp: Default::default(),
            run_key: "test".to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: true,
            extra_env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn demo_fixture_replays_full_lifecycle() {
        let engine = MockEngine::from_fixture(include_str!("fixtures/simple_task.jsonl"));
        let mut proc = engine.start(spec()).unwrap();
        let mut events = vec![];
        while let Some(e) = proc.events.recv().await {
            events.push(e);
        }
        assert!(matches!(events.first(), Some(ErenEvent::RunStarted { .. })));
        assert!(matches!(
            events.last(),
            Some(ErenEvent::RunCompleted { .. })
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, ErenEvent::ToolCall { .. })));
    }
}
