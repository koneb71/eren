//! Picking a dead run back up instead of starting it over.
//!
//! `runs.session_id` has been written faithfully since the first Claude Code
//! run and never read back after a failure. A card that died forty minutes in
//! offered exactly one button, and it deleted the worktree *and* the branch —
//! so the only way to recover the work was to not have lost it.
//!
//! Everything here is pure, for the reason `task_plan.rs` gives: there are no
//! database-backed tests in this repository, so a decision that can be made
//! from values gets made from values, and the six ways this can be wrong are
//! asserted directly rather than discovered on a $7 card.
//!
//! **Resume writes a new run row.** It does not re-queue the dead one. Three
//! columns decide that, and each on its own would be enough:
//!
//! - `cost_usd` accumulates. Re-using the row blends a failed attempt's spend
//!   into the live one, and the daily budget guard reads that number.
//! - `events` is `UNIQUE (run_id, seq)` and `next_seq` continues from the max,
//!   so a re-used row interleaves `RunFailed` into the middle of a transcript
//!   that later completes.
//! - `error_reason` and `finished_at` are single-valued. Re-queuing means
//!   erasing the record of the failure you are resuming from — the one thing
//!   you want to still be able to read afterwards.
//!
//! The new row costs one INSERT and one nullable `resumed_from`, and the
//! dispatch path already reads `session_id`/`session_engine` off the row it is
//! dispatching, so copying those two columns needs no change there at all.

use eren_shared::RunStatus;

/// Where the resumed run would work.
///
/// Spelled out rather than inferred from `Option<&str>`, because "no worktree"
/// means two opposite things. A project without version control never had one
/// and never will — that is `InPlace`, and it is fine. A card whose worktree
/// was reclaimed also reads as no path, and resuming *that* hands a session
/// which remembers a tree of files to an empty checkout: the engine believes
/// its edits exist, does not re-make them, and reports success over nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cwd<'a> {
    /// The project has no version control, so runs happen in the checkout
    /// itself. Nothing was ever isolated, so nothing can have been reclaimed.
    InPlace,
    /// The card's worktree, confirmed present on disk by the caller.
    Worktree(&'a str),
    /// The card had a worktree and it is gone — reclaimed after landing,
    /// swept, or deleted by hand.
    Gone,
}

/// Everything `decide` needs to know about the run being resumed.
#[derive(Debug, Clone, Copy)]
pub struct Prior<'a> {
    pub status: RunStatus,
    pub session_id: Option<&'a str>,
    /// Which CLI minted `session_id`. Not the same question as `engine`: a
    /// card can be re-pointed at a different engine between runs.
    pub session_engine: Option<&'a str>,
    /// The engine the resumed run would use.
    pub engine: &'a str,
    /// `Capabilities::resume_sessions` for that engine.
    pub engine_can_resume: bool,
    pub cwd: Cwd<'a>,
    /// A plain card run. False for chat, workflow, org, comment-reply and KB
    /// runs, which each end somewhere this cannot put them back.
    pub is_task_run: bool,
    /// A review or summary pass: read-only by its trigger, which a resume
    /// does not carry — resumed, it would come back as a writable work run
    /// in the card's worktree. Such a pass is started again, never resumed.
    pub read_only_pass: bool,
}

/// Why this run cannot be picked up.
///
/// Every arm is a sentence shown to the person who clicked, because a button
/// that disappears with no explanation is worse than one that says why it is
/// disabled — the reader concludes the feature is broken rather than that
/// their situation does not qualify.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Still queued, running, parked or held. Resuming a live run would put
    /// two engines in one worktree.
    StillOwesAnOutcome(RunStatus),
    /// The engine never reported a session id — it died before its first
    /// message, or it is an engine that does not mint them.
    NothingToResume,
    /// A session id means something only to the CLI that minted it. Handing
    /// an OpenCode `ses_…` to Claude Code does not error: it starts over with
    /// none of the context, which is the quiet kind of wrong. `dispatch`
    /// already filters on this silently; here it becomes a refusal.
    DifferentEngine { session: String, engine: String },
    /// Gated at the click and never silently downgraded to a start-over, for
    /// the reason CLAUDE.md gives for OpenCode + `Reviewed`: a downgrade the
    /// user did not ask for is the button doing something else.
    EngineCannotResume { engine: String },
    /// The worktree the session remembers no longer exists.
    WorktreeGone,
    /// Chat, workflow, org and comment runs each have their own continuation
    /// and their own side effects; a generic resume would post a second reply
    /// or re-run a paid pipeline.
    NotATaskRun,
    /// A review or summary pass. See `Prior::read_only_pass`.
    ReadOnlyPass,
}

impl Refusal {
    /// One line, for a tooltip on the disabled button.
    pub fn message(&self) -> String {
        match self {
            Self::StillOwesAnOutcome(s) => {
                format!(
                    "this run is still {} — stop it first",
                    s.as_str().replace('_', " ")
                )
            }
            Self::NothingToResume => {
                "this run never reported a session, so there is nothing to pick up".into()
            }
            Self::DifferentEngine { session, engine } => {
                format!("the session belongs to {session} and this card now runs on {engine}")
            }
            Self::EngineCannotResume { engine } => {
                format!("{engine} can't resume a previous session")
            }
            Self::WorktreeGone => {
                "the worktree this session was working in has been reclaimed".into()
            }
            Self::NotATaskRun => "only a card's own run can be resumed".into(),
            Self::ReadOnlyPass => {
                "a review or summary pass is started again, not resumed — use Review again".into()
            }
        }
    }
}

/// Can this run be resumed, and with which session id?
///
/// Order matters only for which message you get first; every arm is checked
/// against the same snapshot.
pub fn decide(p: &Prior<'_>) -> Result<String, Refusal> {
    if !p.is_task_run {
        return Err(Refusal::NotATaskRun);
    }
    if p.read_only_pass {
        return Err(Refusal::ReadOnlyPass);
    }
    if !p.status.is_terminal() {
        return Err(Refusal::StillOwesAnOutcome(p.status));
    }
    let session = p.session_id.filter(|s| !s.trim().is_empty());
    let Some(session) = session else {
        return Err(Refusal::NothingToResume);
    };
    if !p.engine_can_resume {
        return Err(Refusal::EngineCannotResume {
            engine: p.engine.to_string(),
        });
    }
    match p.session_engine {
        Some(e) if e == p.engine => {}
        // A session with no recorded engine predates `session_engine` or was
        // written by a path that did not set it. Refusing is the safe half of
        // the trade: the alternative is starting over while claiming not to.
        other => {
            return Err(Refusal::DifferentEngine {
                session: other.unwrap_or("an unknown engine").to_string(),
                engine: p.engine.to_string(),
            })
        }
    }
    if p.cwd == Cwd::Gone {
        return Err(Refusal::WorktreeGone);
    }
    Ok(session.to_string())
}

/// What to say to an agent that is already half way through.
///
/// Deliberately **not** the original brief. Re-sending it on top of a
/// half-finished transcript reads as "do it again" to a model that can already
/// see its own earlier work — which is the exact failure this feature exists
/// to avoid. The original travels clipped, as reference, after the instruction.
///
/// The stop reason is included when there is one because it is usually the
/// most useful sentence available: "API Error: Unable to connect" tells the
/// agent it lost the connection, not that it did something wrong.
pub fn continuation_prompt(original: &str, stop_reason: Option<&str>) -> String {
    let mut p = String::from(
        "You are picking up work you already started in this session, not \
         starting it over.\n\n",
    );
    match stop_reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(reason) => p.push_str(&format!(
            "The previous run stopped before finishing. What it reported:\n\n{}\n\n",
            crate::runs::orchestrator::clip_chars(reason, 500),
        )),
        None => {
            p.push_str("The previous run stopped before finishing, with no reason recorded.\n\n")
        }
    }
    p.push_str(
        "First check where you actually got to — read the files you were \
         editing and the state of the working tree, because the last thing you \
         intended may not have been written. Then carry on from there. Do not \
         start over, do not redo work that is already correct, and do not \
         revert your own earlier changes.\n\n",
    );
    p.push_str(&format!(
        "For reference, the original task was:\n\n{}\n",
        crate::runs::orchestrator::clip_chars(original, 2000),
    ));
    p
}

/// The whole door, for the route and for the reaper alike: read the dead
/// run, decide with [`decide`], refuse while anything still works on the
/// card, and write the new run. `(new_run, task_id)` on success.
pub async fn resume_dead_run(
    orch: &crate::runs::orchestrator::Orchestrator,
    run_id: uuid::Uuid,
) -> Result<(uuid::Uuid, uuid::Uuid), crate::approvals::Refusal> {
    use crate::approvals::Refusal;
    use sqlx::Row;
    let row = sqlx::query(
        // `COALESCE(r.worktree_path, t.worktree_path)` mirrors what
        // `execute_task_run` picks: a bake-off variant has its own checkout,
        // everything else works in the card's.
        "SELECT r.status, r.session_id, r.session_engine, r.engine, r.task_id, r.trigger,
                r.chat_id, r.workflow_id, r.team_id, r.comment_id, r.kb_brief,
                COALESCE(r.worktree_path, t.worktree_path) AS worktree_path,
                p.vcs
         FROM runs r
         LEFT JOIN tasks t ON t.id = r.task_id
         LEFT JOIN projects p ON p.id = t.project_id
         WHERE r.id = $1",
    )
    .bind(run_id)
    .fetch_optional(&orch.db.pool)
    .await?
    .ok_or_else(|| Refusal::NotFound("no such run".into()))?;

    let engine_id: String = row.get("engine");
    let engine = orch.engine(&engine_id);
    let worktree: Option<String> = row.get("worktree_path");
    // The one part of the decision that has to touch the disk, done here so
    // `decide` stays pure. `drop_worktree` nulls the column, but a directory
    // removed by hand leaves it set and pointing at nothing.
    let cwd = if row.get::<Option<String>, _>("vcs").as_deref() != Some("git") {
        Cwd::InPlace
    } else {
        match worktree.as_deref() {
            Some(p) if std::path::Path::new(p).is_dir() => Cwd::Worktree(p),
            _ => Cwd::Gone,
        }
    };
    let status: String = row.get("status");
    let session_id: Option<String> = row.get("session_id");
    let session_engine: Option<String> = row.get("session_engine");
    let prior = Prior {
        status: RunStatus::parse(&status)
            .ok_or_else(|| Refusal::Conflict(format!("this run is {status}")))?,
        session_id: session_id.as_deref(),
        session_engine: session_engine.as_deref(),
        engine: &engine_id,
        engine_can_resume: engine
            .map(|e| e.capabilities().resume_sessions)
            .unwrap_or(false),
        cwd,
        is_task_run: row.get::<Option<uuid::Uuid>, _>("task_id").is_some()
            && row.get::<Option<uuid::Uuid>, _>("chat_id").is_none()
            && row.get::<Option<uuid::Uuid>, _>("workflow_id").is_none()
            && row.get::<Option<uuid::Uuid>, _>("team_id").is_none()
            && row.get::<Option<uuid::Uuid>, _>("comment_id").is_none()
            && row.get::<Option<String>, _>("kb_brief").is_none(),
        read_only_pass: matches!(
            row.get::<String, _>("trigger").as_str(),
            "summary" | crate::review::PEER_REVIEW
        ),
    };
    let session = decide(&prior).map_err(|r| Refusal::Conflict(r.message()))?;
    let task_id: uuid::Uuid = row
        .get::<Option<uuid::Uuid>, _>("task_id")
        .ok_or_else(|| Refusal::Conflict("only a card's run can be resumed".into()))?;

    // Whether anything at all still works on the card — a run, or a step of
    // an epic's run — because two engines in one worktree is the failure this
    // and Retry both prevent. Anything not ended, rather than a list of live
    // states: a plan parked for approval is the card's work too, and a list
    // is how that one was missed.
    orch.supersede_summary(task_id).await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE task_id = $1
                           AND status NOT IN ('completed','failed','canceled'))
             OR EXISTS (SELECT 1 FROM steps s JOIN runs r ON r.id = s.run_id
                         WHERE s.task_id = $1
                           AND s.status NOT IN ('completed','failed','canceled','skipped')
                           AND r.status NOT IN ('completed','failed','canceled'))",
    )
    .bind(task_id)
    .fetch_one(&orch.db.pool)
    .await?;
    if busy {
        return Err(Refusal::Conflict(
            "this card is already running — cancel it before resuming".into(),
        ));
    }
    let new_run = orch
        .resume_run(run_id, &session)
        .await
        .map_err(Refusal::Gated)?;
    sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
        .bind(task_id)
        .execute(&orch.db.pool)
        .await?;
    Ok((new_run, task_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok<'a>() -> Prior<'a> {
        Prior {
            status: RunStatus::Failed,
            session_id: Some("sess-1"),
            session_engine: Some("claude-code"),
            engine: "claude-code",
            engine_can_resume: true,
            cwd: Cwd::Worktree("/tmp/wt"),
            is_task_run: true,
            read_only_pass: false,
        }
    }

    /// A review or summary pass resumed would come back writable.
    #[test]
    fn a_read_only_pass_is_never_resumed() {
        let p = Prior {
            read_only_pass: true,
            ..ok()
        };
        assert_eq!(decide(&p), Err(Refusal::ReadOnlyPass));
    }

    #[test]
    fn a_failed_card_run_with_a_session_resumes() {
        assert_eq!(decide(&ok()), Ok("sess-1".to_string()));
    }

    #[test]
    fn a_project_without_git_resumes_in_place() {
        // The arm most likely to be got backwards: no worktree path is normal
        // here, not a reclaimed one.
        let p = Prior {
            cwd: Cwd::InPlace,
            ..ok()
        };
        assert_eq!(decide(&p), Ok("sess-1".to_string()));
    }

    #[test]
    fn a_run_that_has_not_ended_is_refused() {
        for status in [
            RunStatus::Queued,
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingPermission,
            RunStatus::AwaitingApproval,
            RunStatus::RateLimited,
        ] {
            let p = Prior { status, ..ok() };
            assert_eq!(
                decide(&p),
                Err(Refusal::StillOwesAnOutcome(status)),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_completed_run_is_not_refused_here() {
        // Continuing a run that finished is not *wrong* — the session is
        // valid and the worktree is there — so this layer does not invent a
        // rule against it. Whether to *offer* it is a separate question, and
        // the board answers no: nearly every card has a completed run, and a
        // button on all of them is noise rather than an offer. Keeping the two
        // apart is the point; if a surface ever wants "keep going", it does
        // not have to reopen this decision.
        let p = Prior {
            status: RunStatus::Completed,
            ..ok()
        };
        assert!(decide(&p).is_ok());
    }

    #[test]
    fn a_session_from_another_engine_is_refused_rather_than_silently_ignored() {
        let p = Prior {
            session_engine: Some("opencode"),
            ..ok()
        };
        assert_eq!(
            decide(&p),
            Err(Refusal::DifferentEngine {
                session: "opencode".into(),
                engine: "claude-code".into()
            })
        );
        // And a session with no recorded engine is treated the same way.
        let p = Prior {
            session_engine: None,
            ..ok()
        };
        assert!(matches!(decide(&p), Err(Refusal::DifferentEngine { .. })));
    }

    #[test]
    fn an_engine_that_cannot_resume_is_refused_at_the_click() {
        let p = Prior {
            engine_can_resume: false,
            ..ok()
        };
        assert_eq!(
            decide(&p),
            Err(Refusal::EngineCannotResume {
                engine: "claude-code".into()
            })
        );
    }

    #[test]
    fn a_reclaimed_worktree_is_refused() {
        let p = Prior {
            cwd: Cwd::Gone,
            ..ok()
        };
        assert_eq!(decide(&p), Err(Refusal::WorktreeGone));
    }

    #[test]
    fn a_missing_or_blank_session_is_nothing_to_resume() {
        assert_eq!(
            decide(&Prior {
                session_id: None,
                ..ok()
            }),
            Err(Refusal::NothingToResume)
        );
        assert_eq!(
            decide(&Prior {
                session_id: Some("   "),
                ..ok()
            }),
            Err(Refusal::NothingToResume)
        );
    }

    #[test]
    fn only_a_card_run_is_resumable() {
        let p = Prior {
            is_task_run: false,
            ..ok()
        };
        assert_eq!(decide(&p), Err(Refusal::NotATaskRun));
    }

    #[test]
    fn every_refusal_says_something() {
        for r in [
            Refusal::StillOwesAnOutcome(RunStatus::Running),
            Refusal::NothingToResume,
            Refusal::DifferentEngine {
                session: "a".into(),
                engine: "b".into(),
            },
            Refusal::EngineCannotResume { engine: "b".into() },
            Refusal::WorktreeGone,
            Refusal::NotATaskRun,
            Refusal::ReadOnlyPass,
        ] {
            let m = r.message();
            assert!(!m.is_empty(), "{r:?}");
            // A tooltip, not a paragraph.
            assert!(m.len() < 120, "{r:?}: {m}");
            assert!(
                !m.contains('_'),
                "{r:?}: {m} still reads as a database value"
            );
        }
    }

    #[test]
    fn the_continuation_carries_the_reason_and_says_not_to_start_over() {
        let p = continuation_prompt(
            "Build the login page",
            Some("API Error: connection refused"),
        );
        assert!(p.contains("API Error: connection refused"));
        assert!(p.contains("Do not start over"));
        assert!(p.contains("Build the login page"));
        // The instruction comes before the original brief, so the last thing
        // read is reference material rather than a fresh-sounding task.
        assert!(p.find("Do not start over").unwrap() < p.find("Build the login page").unwrap());
    }

    #[test]
    fn no_reason_still_produces_a_usable_prompt() {
        let p = continuation_prompt("Build the login page", None);
        assert!(p.contains("no reason recorded"));
        assert!(p.contains("Do not start over"));
        // An empty string is a missing reason, not a reason that is blank.
        assert_eq!(p, continuation_prompt("Build the login page", Some("  ")));
    }

    #[test]
    fn a_pathological_original_stays_bounded() {
        let huge = "x".repeat(50_000);
        let p = continuation_prompt(&huge, Some(&"y".repeat(50_000)));
        assert!(p.chars().count() < 4_000, "{}", p.chars().count());
        assert!(p.contains("Do not start over"));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::approvals::Refusal as Answer;
    use crate::testdb;

    /// A plan parked for approval is the card's live work: resuming an older
    /// run of the card under it would put a second engine in the worktree the
    /// approved plan is about to run in.
    #[tokio::test]
    async fn resume_is_refused_while_a_plan_waits_for_approval() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        orch.set_queue_paused(true).await.unwrap();
        let (_, project) = t.project(dir.path(), true).await;
        let card = t.card(project, "planned").await;
        let dead: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO runs (task_id, status, trigger, engine, session_id, session_engine)
             VALUES ($1, 'failed', 'manual', 'mock', 'sess-1', 'mock') RETURNING id",
        )
        .bind(card)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (task_id, status, trigger, engine, plan_approval)
             VALUES ($1, 'awaiting_approval', 'manual', 'mock', TRUE)",
        )
        .bind(card)
        .execute(&t.db.pool)
        .await
        .unwrap();

        match resume_dead_run(&orch, dead).await {
            Err(Answer::Conflict(m)) => assert!(m.contains("already running"), "{m}"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE task_id = $1")
            .bind(card)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(runs, 2, "nothing was started");
        t.finish().await;
    }
}
