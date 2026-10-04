//! Organizations: a team with a manager.
//!
//! The manager reads the goal and the roster, splits the work into briefed
//! assignments, and hands them out. Specialists can talk to the team and
//! escalate decisions back. After every hand-off the manager gets a chance
//! to adjust what is still queued, and a failure goes to it for a decision
//! rather than killing the run.
//!
//! **The plan lives in the database, not on this stack frame.** That single
//! choice is what makes three things the same mechanism: a human editing the
//! plan before work starts, the manager revising it mid-run, and a parked or
//! crashed run picking up where it left off.
//!
//! Assignments share one worktree, so a run produces a single coherent diff
//! to review rather than N branches to merge. Within that, work runs in
//! parallel exactly when the manager declared non-overlapping file scopes
//! and no dependency between them — see `schedule`.

pub mod epic;
pub mod plan;
pub mod replan;
pub mod roster;
pub mod schedule;

use eren_engines::RunSpec;
use eren_shared::{ModelTier, PermissionMode, ReasoningEffort, RunStatus};
use futures::StreamExt;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use super::memory;
use super::orchestrator::{next_seq, slugify, CallerKind, Orchestrator, SeqAlloc, StreamOutcome};
use plan::{
    assignment_prompt, has_blocking, inspect_plan, parse_plan, plan_prompt, repair_prompt,
    resolve_assignee, Defect, Plan, PlannedTask, Severity, TaskSize,
};
use replan::{
    apply_decision, clip, replan_prompt, triage_prompt, Mutation, ReplanDecision, Triage,
    TriageAction, MAX_ADDED_ASSIGNMENTS, MAX_REPLANS,
};
use roster::{render_roster, Member};
use schedule::{parallel_batch, MAX_PARALLEL_ASSIGNMENTS};

/// Tools the whole org shares for talking to each other.
pub const ORG_TOOLS: &[&str] = &[
    "mcp__eren__post_message",
    "mcp__eren__read_messages",
    "mcp__eren__ask_manager",
];

/// Planning is read-only: the manager inspects and delegates, never edits.
const MANAGER_TOOLS: &[&str] = &["Read", "Grep", "Glob", "mcp__eren__post_message"];

/// Workers get the usual coding surface plus the org tools. Listed
/// explicitly because org runs don't use the interactive permission proxy.
const WORKER_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Bash",
    "TodoWrite",
];

/// A worker may escalate this many times before it has to make a call.
pub const MAX_CONSULTS_PER_STEP: i64 = 3;

/// Steps that are the manager thinking, not work assigned to anyone. Slashes
/// rather than underscores so the SQL `LIKE` needs no escaping.
pub(super) const NOT_AN_ASSIGNMENT: &str = "step_key NOT IN ('plan','plan_repair','review') \
     AND step_key NOT LIKE 'replan/%' AND step_key NOT LIKE 'triage/%'";

/// A board that has fallen behind is not worth failing a run over, but it is
/// worth saying out loud.
///
/// The first version swallowed this with `.ok()`, and a malformed `UPDATE …
/// FROM` then left every sub-task card sitting in Backlog while its assignment
/// ran to completion — with nothing anywhere to say why.
fn log_mirror(result: anyhow::Result<()>, step_id: Uuid) {
    if let Err(e) = result {
        tracing::warn!(%step_id, error=%e, "could not update this assignment's card");
    }
}

/// A dependency's report is worth reading in full; everyone else's is worth
/// one line. Unbounded accumulation used to put every prior assignment's
/// whole output into every later prompt.
const DEP_CONTEXT_CHARS: usize = 1500;
const MAX_CONTEXT_CHARS: usize = 6000;

/// One assignment, as it exists in the database.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub step_id: Uuid,
    pub key: String,
    pub title: String,
    pub brief: String,
    pub assignee: String,
    pub done_when: Vec<String>,
    pub size: TaskSize,
    pub depends_on: Vec<String>,
    /// Files this assignment expects to change; drives what may run at the
    /// same time. Empty means unknown, which is treated as "everything".
    pub touches: Vec<String>,
    pub status: String,
    pub output: Option<String>,
    pub attempt: i32,
}

/// Everything an org run needs to spawn a member, gathered once.
struct OrgCtx {
    run_id: Uuid,
    task_id: Option<Uuid>,
    project_id: Uuid,
    goal: String,
    team_name: String,
    engine: Arc<dyn eren_engines::Engine>,
    worktree: PathBuf,
    seq: SeqAlloc,
    /// Floor applied to the manager's thinking while planning.
    planning_effort: ReasoningEffort,
    /// The project's brain and the card's skill, read once for the whole run.
    ///
    /// Once, not per assignment: a run is one decision, and a Brain edited
    /// while an eight-assignment run is in flight should not leave the second
    /// half of the team working from different facts than the first.
    ///
    /// The cost, stated rather than hand-waved: one injection per *fresh*
    /// context. An eight-assignment run pays ten — the plan, the eight briefs,
    /// and the review — at roughly a thousand tokens each. The repair, replan
    /// and triage turns pay nothing, because they resume the manager's own
    /// session and already have it.
    standing: crate::runs::context::Standing,
}

/// Where a dispatch of this run should pick up.
enum Phase {
    Plan,
    Execute { manager_session: Option<String> },
}

impl Orchestrator {
    /// Drop from a batch the assignments whose member is already at its
    /// `max_concurrent` — counting the ones this batch would add — or whose
    /// own budget is spent. A spent budget on the first assignment ends the
    /// run, saying so: the team cannot make progress without it.
    async fn within_limits(
        &self,
        batch: Vec<usize>,
        pending: &[Assignment],
        workers: &[Member],
    ) -> Result<Vec<usize>, crate::budgets::OverBudget> {
        let mut room: HashMap<Uuid, i64> = HashMap::new();
        let mut kept = Vec::with_capacity(batch.len());
        for (position, index) in batch.into_iter().enumerate() {
            let Some(member) = workers.iter().find(|m| m.name == pending[index].assignee) else {
                kept.push(index);
                continue;
            };
            let own = crate::budgets::Scope {
                agent: Some(member.agent_id),
                ..Default::default()
            };
            if let Err(over) = crate::budgets::check(&self.db, &own, false).await {
                if position == 0 {
                    return Err(over);
                }
                continue;
            }
            let left = match room.get(&member.agent_id) {
                Some(left) => Some(*left),
                None => crate::agents::room(&self.db, member.agent_id)
                    .await
                    .ok()
                    .flatten(),
            };
            match left {
                Some(left) if left <= 0 && position > 0 => continue,
                Some(left) => {
                    room.insert(member.agent_id, left - 1);
                    kept.push(index);
                }
                None => kept.push(index),
            }
        }
        Ok(kept)
    }

    /// Queue an organization run for a goal.
    pub async fn enqueue_org_run(
        &self,
        team_id: Uuid,
        project_id: Uuid,
        goal: &str,
        plan_approval: bool,
    ) -> anyhow::Result<Uuid> {
        crate::agents::assert_team_can_run(&self.db, team_id).await?;
        let workspace: Option<Uuid> =
            sqlx::query_scalar("SELECT workspace_id FROM projects WHERE id = $1")
                .bind(project_id)
                .fetch_optional(&self.db.pool)
                .await?;
        let scope = crate::budgets::Scope {
            workspace,
            project: Some(project_id),
            team: Some(team_id),
            ..Default::default()
        };
        crate::budgets::check(&self.db, &scope, true).await?;
        let team_engine: Option<String> =
            sqlx::query_scalar("SELECT engine FROM teams WHERE id = $1")
                .bind(team_id)
                .fetch_optional(&self.db.pool)
                .await?
                .flatten();
        self.vet_team_engine(&team_engine.unwrap_or_else(|| self.default_engine()))?;
        let row = sqlx::query(
            "INSERT INTO runs (team_id, project_id, goal, plan_approval, status, trigger, engine)
             SELECT $1, $2, $3, $4, 'queued', 'org', COALESCE(engine, $5)
             FROM teams WHERE id = $1 RETURNING id",
        )
        .bind(team_id)
        .bind(project_id)
        .bind(goal)
        .bind(plan_approval)
        .bind(self.default_engine())
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = row.get("id");
        self.queue(run_id, 15).await?;
        Ok(run_id)
    }

    /// Can an org team run on this engine? Every member runs on the run's
    /// engine — never its own — hands work over through Eren's tools, and
    /// works in Auto-edit. So that one engine needs both, checked by both
    /// doors into a team run: the Teams page and a card assigned to a team.
    fn vet_team_engine(&self, engine: &str) -> anyhow::Result<()> {
        self.needs_tools(engine, "a team")?;
        if let Some(why) = self.vet_engine(engine, PermissionMode::AutoEdit) {
            return Err(crate::runs::orchestrator::CantHonour(format!(
                "A team's members work in Auto-edit. {why}"
            ))
            .into());
        }
        Ok(())
    }

    /// Put a run on the queue. Public because approving a parked plan
    /// re-dispatches it from an HTTP route.
    ///
    /// A second call is a person saying *now*, and used to be swallowed:
    /// `DO NOTHING` meant that approving a plan for a run which happened to
    /// be held behind a rate-limit backoff left it sitting there for the rest
    /// of the wait, with the button looking like it had done nothing. So the
    /// conflict raises the priority — `GREATEST`, never lowers it — and clears
    /// the backoff, because the click is newer information than the hold.
    pub async fn queue(&self, run_id: Uuid, priority: i32) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO queue (run_id, priority) VALUES ($1, $2)
             ON CONFLICT (run_id) DO UPDATE
                SET priority = GREATEST(queue.priority, EXCLUDED.priority),
                    not_before = NULL",
        )
        .bind(run_id)
        .bind(priority)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// A board task assigned to a team. Organizations get an org run;
    /// pipeline/debate/swarm teams are translated to a workflow and run
    /// through the pipeline executor. Either way the run carries the
    /// `task_id`, so it works in the task's worktree and lands on review.
    pub(crate) async fn enqueue_task_for_team(
        &self,
        task_id: Uuid,
        team_id: Uuid,
    ) -> anyhow::Result<Uuid> {
        crate::agents::assert_team_can_run(&self.db, team_id).await?;
        let row = sqlx::query(
            "SELECT t.prompt, t.title, t.project_id, t.engine AS task_engine,
                    tm.name AS team_name, tm.pattern, tm.definition, tm.engine AS team_engine
             FROM tasks t JOIN teams tm ON tm.id = t.team_id WHERE t.id = $1",
        )
        .bind(task_id)
        .fetch_one(&self.db.pool)
        .await?;

        let project_id: Uuid = row.get("project_id");
        let goal: String = row.get("prompt");
        let pattern: String = row.get("pattern");
        // A team that pins an engine wins over the card's, because the card's
        // is a default nobody chose while the team's is a stated preference.
        let engine: String = row
            .get::<Option<String>, _>("team_engine")
            .unwrap_or_else(|| row.get("task_engine"));

        if pattern == "org" {
            // The card's door into a team, held to what the Teams page's is.
            self.vet_team_engine(&engine)?;
            let run = sqlx::query(
                "INSERT INTO runs (task_id, team_id, project_id, goal, status, trigger, engine)
                 VALUES ($1,$2,$3,$4,'queued','task',$5) RETURNING id",
            )
            .bind(task_id)
            .bind(team_id)
            .bind(project_id)
            .bind(&goal)
            .bind(&engine)
            .fetch_one(&self.db.pool)
            .await?;
            let run_id: Uuid = run.get("id");
            self.queue(run_id, 12).await?;
            return Ok(run_id);
        }

        // Non-org patterns become a workflow named after the task, so
        // re-running the task overwrites its workflow rather than piling up.
        let definition: serde_json::Value = row.get("definition");
        let names = self.member_names(&definition).await?;
        if names.is_empty() {
            anyhow::bail!("this team has no members to assign work to");
        }
        let team_name: String = row.get("team_name");
        let title: String = row.get("title");
        let mut workflow = eren_shared::workflow::from_team(&team_name, &pattern, &names, &goal);
        // Dispatch reads the engine off the workflow, not the run row, so a
        // team pinned to OpenCode has to say so here or it silently reverts.
        workflow.defaults.engine = engine.clone();
        workflow.name = format!("{team_name} · {title}");
        workflow.validate()?;
        let yaml = serde_yaml::to_string(&workflow)?;

        let wf = sqlx::query(
            "INSERT INTO workflows (project_id, name, description, kind, source_yaml)
             VALUES ($1,$2,$3,'team',$4)
             ON CONFLICT (project_id, name) DO UPDATE SET source_yaml = EXCLUDED.source_yaml
             RETURNING id",
        )
        .bind(project_id)
        .bind(&workflow.name)
        .bind(&workflow.description)
        .bind(&yaml)
        .fetch_one(&self.db.pool)
        .await?;
        let workflow_id: Uuid = wf.get("id");

        let run = sqlx::query(
            "INSERT INTO runs (task_id, workflow_id, status, trigger, engine)
             VALUES ($1,$2,'queued','task',$3) RETURNING id",
        )
        .bind(task_id)
        .bind(workflow_id)
        .bind(&engine)
        .fetch_one(&self.db.pool)
        .await?;
        let run_id: Uuid = run.get("id");
        self.queue(run_id, 12).await?;
        Ok(run_id)
    }

    /// Member agent names, in the order the team author arranged them.
    async fn member_names(&self, definition: &serde_json::Value) -> anyhow::Result<Vec<String>> {
        let mut names = vec![];
        for entry in definition
            .get("members")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(agent_id) = entry
                .get("agent_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            if let Some(r) = sqlx::query("SELECT name FROM agents WHERE id = $1")
                .bind(agent_id)
                .fetch_optional(&self.db.pool)
                .await?
            {
                names.push(r.get::<String, _>("name"));
            }
        }
        Ok(names)
    }

    pub(crate) async fn execute_org_run(
        self: &Arc<Self>,
        run_id: Uuid,
        _team_id: Uuid,
    ) -> anyhow::Result<()> {
        let row = sqlx::query(
            // `ta`, not `t` — `t` is already `teams` here, and a second `t`
            // is a duplicate-alias error at best. LEFT, because an org run
            // launched from the team room has no card at all
            // (`enqueue_org_run` inserts no `task_id`); an inner join would
            // stop every one of those from starting.
            "SELECT t.name AS team_name, t.definition, t.planning_effort,
                    r.goal, r.engine, r.task_id, r.worktree_path, r.plan_approval,
                    r.plan_approved_at, ta.skill_id,
                    p.id AS project_id, p.path AS project_path, p.default_branch, p.workspace_id
             FROM runs r JOIN teams t ON t.id = r.team_id
             JOIN projects p ON p.id = r.project_id
             LEFT JOIN tasks ta ON ta.id = r.task_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        self.set_status(run_id, RunStatus::Starting).await?;

        let definition: serde_json::Value = row.get("definition");
        let workspace_id: Uuid = row.get("workspace_id");
        let team_name: String = row.get("team_name");
        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;

        let manager = self
            .resolve_member(workspace_id, definition.get("manager"), "Manager")
            .await?
            .ok_or_else(|| anyhow::anyhow!("this organization has no manager"))?;
        let workers = self.resolve_members(workspace_id, &definition).await?;
        if workers.is_empty() {
            anyhow::bail!("this organization has no specialists to delegate to");
        }

        let task_id: Option<Uuid> = row.get("task_id");
        let phase = self.org_phase(run_id).await?;

        // A resumed run must reach the same worktree it planned against.
        let worktree = match (&phase, row.get::<Option<String>, _>("worktree_path")) {
            (Phase::Execute { .. }, Some(path)) => PathBuf::from(path),
            _ => {
                let created = self
                    .worktree_for_run(
                        run_id,
                        task_id,
                        &PathBuf::from(row.get::<String, _>("project_path")),
                        &row.get::<String, _>("default_branch"),
                        &slugify(&team_name),
                    )
                    .await?;
                sqlx::query("UPDATE runs SET worktree_path=$1 WHERE id=$2")
                    .bind(created.path.to_string_lossy().as_ref())
                    .bind(run_id)
                    .execute(&self.db.pool)
                    .await?;
                created.path
            }
        };

        let ctx = OrgCtx {
            run_id,
            task_id,
            project_id: row.get("project_id"),
            goal: row.get::<Option<String>, _>("goal").unwrap_or_default(),
            team_name,
            engine,
            worktree,
            seq: SeqAlloc::starting_at(next_seq(&self.db, run_id).await?),
            planning_effort: ReasoningEffort::parse(&row.get::<String, _>("planning_effort"))
                .unwrap_or(ReasoningEffort::High),
            // A team card's skill reaches the teammates too. The picker sat
            // on the card doing nothing for a team run, which is the same
            // asymmetry as a button that only works on some cards.
            standing: crate::runs::context::Standing::load(
                &self.db,
                Some(row.get("project_id")),
                row.get::<Option<Uuid>, _>("skill_id"),
            )
            .await,
        };

        let manager_session = match phase {
            Phase::Execute { manager_session } => manager_session,
            Phase::Plan => {
                match self.plan_phase(&ctx, &manager, &workers).await? {
                    Some(session) => {
                        // Park before anyone starts, if the user asked to
                        // look first. Returning here drops this run's queue
                        // permit rather than holding a slot for however long
                        // a person takes.
                        if row.get::<bool, _>("plan_approval")
                            && row
                                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("plan_approved_at")
                                .is_none()
                        {
                            self.post(
                                run_id,
                                None,
                                "system",
                                None,
                                "status",
                                "Plan ready — waiting for your approval before anyone starts.",
                            )
                            .await?;
                            self.set_status(run_id, RunStatus::AwaitingApproval).await?;
                            // The same knock a card's plan gets: a team plan
                            // waiting on a person is no less waiting.
                            let ctx = crate::attention::Ctx {
                                title: "eren: a team plan needs your review".to_string(),
                                ..crate::attention::ctx_for_run(&self.db, run_id, None).await
                            };
                            crate::attention::fire(&self.db, crate::attention::Event::Plan, ctx)
                                .await;
                            return Ok(());
                        }
                        session
                    }
                    // Planning failed; plan_phase already finished the run.
                    None => return Ok(()),
                }
            }
        };

        self.set_status(run_id, RunStatus::Running).await?;
        self.work_phase(&ctx, &manager, &workers, manager_session)
            .await
    }

    /// Which half of the run this dispatch is for.
    ///
    /// `Execute` requires both a completed plan step *and* at least one
    /// assignment: a half-materialized plan must re-plan rather than run an
    /// empty team.
    async fn org_phase(&self, run_id: Uuid) -> anyhow::Result<Phase> {
        let planned =
            sqlx::query("SELECT session_id, status FROM steps WHERE run_id=$1 AND step_key='plan'")
                .bind(run_id)
                .fetch_optional(&self.db.pool)
                .await?;
        let Some(planned) = planned else {
            return Ok(Phase::Plan);
        };
        if planned.get::<String, _>("status") != "completed" {
            return Ok(Phase::Plan);
        }
        let assignments: i64 = sqlx::query(&format!(
            "SELECT COUNT(*) AS n FROM steps WHERE run_id=$1 AND {NOT_AN_ASSIGNMENT}"
        ))
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?
        .get("n");
        if assignments == 0 {
            return Ok(Phase::Plan);
        }
        Ok(Phase::Execute {
            manager_session: planned.get("session_id"),
        })
    }

    /// Plan, repair once if needed, and materialize the assignments.
    /// `Ok(None)` means the run is already finished as failed.
    async fn plan_phase(
        self: &Arc<Self>,
        ctx: &OrgCtx,
        manager: &Member,
        workers: &[Member],
    ) -> anyhow::Result<Option<Option<String>>> {
        self.post(
            ctx.run_id,
            None,
            "system",
            None,
            "status",
            &format!("{} is on it.", ctx.team_name),
        )
        .await?;

        let plan_step = self
            .create_step(
                ctx.run_id,
                "plan",
                Some(&manager.name),
                "Plan the work",
                "",
                0.0,
            )
            .await?;
        let effort = Some(
            manager
                .effort
                .map_or(ctx.planning_effort, |e| e.at_least(ctx.planning_effort)),
        );

        let outcome = self
            .run_member(
                ctx,
                plan_step,
                manager,
                // Fresh context, so it gets the standing block — passed in
                // rather than appended, because this prompt ends with "reply
                // with ONLY a JSON object" and `parse_plan` has to read what
                // comes back. The manager plans from it and the plan carries
                // into every prompt it writes; the repair/replan/triage turns
                // below resume *this* session and must not be given it twice.
                plan_prompt(&ctx.goal, &render_roster(workers), &ctx.standing.block()),
                None,
                MANAGER_TOOLS,
                PermissionMode::Reviewed,
                effort,
            )
            .await?;
        if outcome.status != RunStatus::Completed {
            let reason = outcome.reason.unwrap_or_default();
            self.post(
                ctx.run_id,
                Some(plan_step),
                "system",
                None,
                "status",
                &format!("Planning failed: {reason}"),
            )
            .await?;
            self.finish(ctx.run_id, outcome.status, Some(reason))
                .await?;
            return Ok(None);
        }

        let (parsed, defects) = self.check_plan(&outcome.output, workers);
        let (parsed, defects) = if defects.is_empty() {
            (parsed, defects)
        } else {
            // One repair round on the manager's own session, naming the
            // specific problems. A plan that was one missing field away from
            // usable used to kill the whole run.
            self.post(
                ctx.run_id,
                Some(plan_step),
                "system",
                None,
                "status",
                &format!(
                    "The manager is tightening the plan ({} issue{}).",
                    defects.len(),
                    if defects.len() == 1 { "" } else { "s" }
                ),
            )
            .await?;
            let repair_step = self
                .create_step(
                    ctx.run_id,
                    "plan_repair",
                    Some(&manager.name),
                    "Revise the plan",
                    "",
                    0.1,
                )
                .await?;
            let repaired = self
                .run_member(
                    ctx,
                    repair_step,
                    manager,
                    repair_prompt(&defects),
                    outcome.session_id.clone(),
                    MANAGER_TOOLS,
                    PermissionMode::Reviewed,
                    effort,
                )
                .await?;
            if repaired.status == RunStatus::Completed {
                self.check_plan(&repaired.output, workers)
            } else {
                (parsed, defects)
            }
        };

        if has_blocking(&defects) {
            let why = defects
                .iter()
                .filter(|d| d.severity() == Severity::Blocking)
                .map(Defect::message)
                .collect::<Vec<_>>()
                .join(" ");
            self.post(
                ctx.run_id,
                Some(plan_step),
                "system",
                None,
                "status",
                &format!("The plan still isn't runnable. {why}"),
            )
            .await?;
            self.finish(ctx.run_id, RunStatus::Failed, Some(why))
                .await?;
            return Ok(None);
        }
        for advisory in &defects {
            self.post(
                ctx.run_id,
                Some(plan_step),
                "system",
                None,
                "status",
                &advisory.message(),
            )
            .await?;
        }

        let plan = parsed.unwrap_or_default();
        if !plan.summary.trim().is_empty() {
            self.post(
                ctx.run_id,
                Some(plan_step),
                &manager.name,
                None,
                "message",
                &plan.summary,
            )
            .await?;
        }

        for (index, task) in plan.tasks.iter().enumerate() {
            // Validation already proved the assignee resolves.
            let assignee = resolve_assignee(workers, &task.assignee)
                .unwrap_or_else(|| workers[0].name.clone());
            let step_id = self
                .create_assignment(ctx.run_id, task, &assignee, index as f64 + 1.0, "plan", 1)
                .await?;
            self.post(
                ctx.run_id,
                Some(step_id),
                &manager.name,
                Some(&assignee),
                "assignment",
                &format!("**{}** — {}", task.title, task.brief),
            )
            .await?;
        }

        Ok(Some(outcome.session_id))
    }

    fn check_plan(&self, output: &str, workers: &[Member]) -> (Option<Plan>, Vec<Defect>) {
        match parse_plan(output) {
            Ok(plan) => {
                let defects = inspect_plan(&plan, workers);
                (Some(plan), defects)
            }
            Err(defect) => (None, vec![defect]),
        }
    }

    /// Run the assignments, re-planning after each and triaging failures.
    async fn work_phase(
        self: &Arc<Self>,
        ctx: &OrgCtx,
        manager: &Member,
        workers: &[Member],
        manager_session: Option<String>,
    ) -> anyhow::Result<()> {
        let mut aborted: Option<String> = None;
        let mut dropped = 0usize;
        let mut added = 0usize;

        loop {
            // Put the plan on the board before anyone starts on it.
            //
            // Here rather than at the end of planning, because `work_phase` is
            // reached only once the plan is accepted — whether that was an
            // explicit approval or a run with no approval gate at all. Rejecting
            // a plan therefore leaves no tickets behind. Re-running each pass
            // also picks up assignments a re-plan added, with no extra wiring.
            if let Err(e) = epic::reconcile(&self.db, ctx.run_id).await {
                // Worth saying out loud, but not worth failing the run over: the
                // work can proceed perfectly well with the board out of date.
                tracing::warn!(run_id=%ctx.run_id, error=%e, "could not put the plan on the board");
            }

            let all = self.load_assignments(ctx.run_id).await?;
            let satisfied: HashSet<String> = all
                .iter()
                .filter(|a| a.status == "completed" || a.status == "skipped")
                .map(|a| a.key.clone())
                .collect();
            let pending: Vec<Assignment> = all
                .iter()
                .filter(|a| a.status == "queued")
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            // Interrupting the step that was running is not enough — without
            // this the run would simply start the next assignment.
            if self.cancel_requested(ctx.run_id) {
                aborted = Some("canceled".to_string());
                break;
            }
            // The team finishes the assignments it is working on, but no new
            // batch starts on a budget that is spent.
            if let Err(over) = self.budget_allows_more(ctx.run_id).await {
                aborted = Some(over.to_string());
                break;
            }

            // Everything ready whose file scopes don't collide. Usually one;
            // several when the manager split the work cleanly.
            let batch = parallel_batch(&pending, &satisfied, MAX_PARALLEL_ASSIGNMENTS);
            // A specialist at its limit of runs at once sits out the rest of
            // this batch. The first assignment always goes, so the team never
            // stalls waiting on itself.
            let batch = match self.within_limits(batch, &pending, &workers).await {
                Ok(batch) => batch,
                Err(over) => {
                    aborted = Some(over.to_string());
                    break;
                }
            };
            let completed: Vec<&Assignment> =
                all.iter().filter(|a| a.status == "completed").collect();

            let mut tools: Vec<&str> = WORKER_TOOLS.to_vec();
            tools.extend_from_slice(ORG_TOOLS);

            let mut jobs = Vec::with_capacity(batch.len());
            for &index in &batch {
                let assignment = pending[index].clone();
                let member = workers
                    .iter()
                    .find(|m| m.name == assignment.assignee)
                    .unwrap_or(&workers[0])
                    .clone();
                // A worker starts fresh every time — new assignment, new
                // attempt, every member of a parallel batch — and this brief
                // is the only thing it ever sees. It does not inherit the
                // manager's session, so without this the teammate holding
                // Edit, Write and Bash is the one member of the team working
                // without the facts. That is where the $75 card lost the note
                // saying which compose file is real.
                let prompt = ctx.standing.apply(&assignment_prompt(
                    &member.name,
                    &member.title,
                    &ctx.team_name,
                    &ctx.goal,
                    &assignment.title,
                    &assignment.brief,
                    &assignment.done_when,
                    &context_for(&assignment, &completed),
                ));
                self.post(
                    ctx.run_id,
                    Some(assignment.step_id),
                    &member.name,
                    None,
                    "status",
                    &format!("Starting: {}", assignment.title),
                )
                .await?;
                jobs.push((assignment, member, prompt));
            }
            if batch.len() > 1 {
                self.post(
                    ctx.run_id,
                    None,
                    "system",
                    None,
                    "status",
                    &format!(
                        "{} are working in parallel — their files don't overlap.",
                        jobs.iter()
                            .map(|(_, m, _)| m.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                )
                .await?;
            }

            // This run holds one queue permit already; extras are taken only
            // if immediately free, so a batch can never deadlock against
            // another run waiting for the same semaphore.
            let extra: Vec<_> = (1..jobs.len())
                .filter_map(|_| self.slots.sem().try_acquire_owned().ok())
                .collect();
            let concurrency = 1 + extra.len();

            let results =
                futures::stream::iter(jobs.into_iter().map(|(assignment, member, prompt)| {
                    let this = self.clone();
                    let tools = tools.clone();
                    async move {
                        let outcome = this
                            .run_member(
                                ctx,
                                assignment.step_id,
                                &member,
                                prompt,
                                None,
                                &tools,
                                PermissionMode::AutoEdit,
                                member.effort,
                            )
                            .await;
                        (assignment, member, outcome)
                    }
                }))
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
            drop(extra);

            // Report everything that landed before deciding what to do about
            // anything that didn't — the manager should see the whole batch.
            let mut finished: Vec<(Assignment, String, String)> = vec![];
            let mut failures: Vec<(Assignment, String, StreamOutcome)> = vec![];
            for (assignment, member, outcome) in results {
                let outcome = outcome?;
                if outcome.status == RunStatus::Completed {
                    self.post(
                        ctx.run_id,
                        Some(assignment.step_id),
                        &member.name,
                        None,
                        "result",
                        &outcome.output,
                    )
                    .await?;
                    self.remember_assignment(ctx, &member, &assignment.title, &outcome.output)
                        .await;
                    finished.push((assignment, member.name.clone(), outcome.output));
                } else {
                    failures.push((assignment, member.name.clone(), outcome));
                }
            }

            if !finished.is_empty() {
                let who = finished
                    .iter()
                    .map(|(_, name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(" and ");
                let titles = finished
                    .iter()
                    .map(|(a, _, _)| a.title.as_str())
                    .collect::<Vec<_>>()
                    .join("\" and \"");
                let report = finished
                    .iter()
                    .map(|(a, name, output)| format!("{} ({name}):\n{output}", a.title))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let (last, _, _) = &finished[finished.len() - 1];
                added += self
                    .replan_round(
                        ctx,
                        manager,
                        workers,
                        &manager_session,
                        last,
                        &who,
                        &titles,
                        &report,
                        added,
                    )
                    .await?;
            }

            for (assignment, who, outcome) in failures {
                match self
                    .triage_failure(
                        ctx,
                        manager,
                        workers,
                        &manager_session,
                        &assignment,
                        &who,
                        &outcome,
                    )
                    .await?
                {
                    Some(reason) => {
                        aborted = Some(reason);
                        break;
                    }
                    None => dropped += 1,
                }
            }
            if aborted.is_some() {
                break;
            }
        }

        // No point asking for a review of work the user just stopped.
        if aborted.is_none() {
            self.review(ctx, manager).await?;
        }

        let (status, reason) = match aborted {
            // A cancel is a decision, not a failure; saying "failed" would
            // misreport what happened.
            Some(reason) if reason == "canceled" => (RunStatus::Canceled, None),
            Some(reason) => (RunStatus::Failed, Some(reason)),
            None if dropped > 0 => (
                RunStatus::Completed,
                Some(format!(
                    "{dropped} assignment{} were dropped after failing",
                    if dropped == 1 { " was" } else { "s" }
                )),
            ),
            None => (RunStatus::Completed, None),
        };
        self.finish(ctx.run_id, status, reason).await?;
        self.settle_task_for_run(ctx.task_id, status).await?;
        Ok(())
    }

    /// Give the manager a chance to adjust what is still queued. Returns how
    /// many assignments it added.
    #[allow(clippy::too_many_arguments)]
    async fn replan_round(
        self: &Arc<Self>,
        ctx: &OrgCtx,
        manager: &Member,
        workers: &[Member],
        manager_session: &Option<String>,
        finished: &Assignment,
        who: &str,
        titles: &str,
        outcome: &str,
        added_so_far: usize,
    ) -> anyhow::Result<usize> {
        let _ = finished; // kept for the call site's readability
        let all = self.load_assignments(ctx.run_id).await?;
        let pending: Vec<Assignment> = all
            .iter()
            .filter(|a| a.status == "queued")
            .cloned()
            .collect();
        // Nothing left to adjust: skip the call entirely.
        if pending.is_empty() {
            return Ok(0);
        }

        let round: i32 =
            sqlx::query("UPDATE runs SET replans = replans + 1 WHERE id=$1 RETURNING replans")
                .bind(ctx.run_id)
                .fetch_one(&self.db.pool)
                .await?
                .get("replans");
        if round > MAX_REPLANS {
            return Ok(0);
        }

        let budget = MAX_ADDED_ASSIGNMENTS.saturating_sub(added_so_far);
        let step = self
            .create_step(
                ctx.run_id,
                &format!("replan/{round}"),
                Some(&manager.name),
                "Adjust the plan",
                "",
                0.2,
            )
            .await?;
        let outcome_run = self
            .run_member(
                ctx,
                step,
                manager,
                replan_prompt(who, titles, outcome, &pending, budget),
                manager_session.clone(),
                MANAGER_TOOLS,
                PermissionMode::Reviewed,
                Some(
                    manager
                        .effort
                        .map_or(ctx.planning_effort, |e| e.at_least(ctx.planning_effort)),
                ),
            )
            .await?;
        if outcome_run.status != RunStatus::Completed {
            return Ok(0);
        }

        let decision: ReplanDecision = crate::runs::utility::extract_json(&outcome_run.output)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        if decision.is_noop() {
            return Ok(0);
        }

        let note = decision.note.clone();
        let (mutations, refused) = apply_decision(decision, &pending, workers, all.len(), budget);
        let mut new_positions = all.len() as f64 + 1.0;
        let mut added = 0usize;

        for mutation in mutations {
            match mutation {
                Mutation::Drop { step_id, key } => {
                    sqlx::query("UPDATE steps SET status='skipped', finished_at=now() WHERE id=$1")
                        .bind(step_id)
                        .execute(&self.db.pool)
                        .await?;
                    log_mirror(epic::mirror_step(&self.db, step_id).await, step_id);
                    self.post(
                        ctx.run_id,
                        Some(step_id),
                        &manager.name,
                        None,
                        "status",
                        &format!("Dropped \"{key}\" — no longer needed."),
                    )
                    .await?;
                }
                Mutation::Revise {
                    step_id,
                    key,
                    assignee,
                    brief,
                    done_when,
                } => {
                    sqlx::query(
                        "UPDATE steps SET assignee = COALESCE($1, assignee),
                                brief = COALESCE($2, brief),
                                done_when = COALESCE($3, done_when)
                         WHERE id = $4",
                    )
                    .bind(&assignee)
                    .bind(&brief)
                    .bind(&done_when)
                    .bind(step_id)
                    .execute(&self.db.pool)
                    .await?;
                    let moved = assignee
                        .as_deref()
                        .map(|t| format!(" — now with {t}"))
                        .unwrap_or_default();
                    let rewritten = brief.map(|b| format!("\n\n{b}")).unwrap_or_default();
                    let note = format!("Revised \"{key}\"{moved}{rewritten}");
                    self.post(
                        ctx.run_id,
                        Some(step_id),
                        &manager.name,
                        assignee.as_deref(),
                        "assignment",
                        &note,
                    )
                    .await?;
                    // Told to the card, not written over it. The manager may
                    // have rewritten a brief that somebody has since edited by
                    // hand, and silently replacing their words with the model's
                    // is the worst outcome available — so the card keeps what it
                    // says and gains a comment explaining what changed.
                    epic::note_revision(&self.db, step_id, &manager.name, &note)
                        .await
                        .ok();
                }
                Mutation::Add(task) => {
                    let assignee = resolve_assignee(workers, &task.assignee)
                        .unwrap_or_else(|| workers[0].name.clone());
                    let step_id = self
                        .create_assignment(ctx.run_id, &task, &assignee, new_positions, "replan", 1)
                        .await?;
                    new_positions += 1.0;
                    added += 1;
                    self.post(
                        ctx.run_id,
                        Some(step_id),
                        &manager.name,
                        Some(&assignee),
                        "assignment",
                        &format!("**{}** — {}", task.title, task.brief),
                    )
                    .await?;
                }
            }
        }
        if !note.trim().is_empty() {
            self.post(
                ctx.run_id,
                Some(step),
                &manager.name,
                None,
                "message",
                &note,
            )
            .await?;
        }
        for refusal in refused {
            tracing::debug!(run_id = %ctx.run_id, "re-plan refused: {refusal}");
        }
        Ok(added)
    }

    /// A failed assignment goes to the manager. `Some(reason)` aborts the
    /// run; `None` means the plan carries on without it.
    #[allow(clippy::too_many_arguments)]
    async fn triage_failure(
        self: &Arc<Self>,
        ctx: &OrgCtx,
        manager: &Member,
        workers: &[Member],
        manager_session: &Option<String>,
        failed: &Assignment,
        who: &str,
        outcome: &StreamOutcome,
    ) -> anyhow::Result<Option<String>> {
        let reason = outcome.reason.clone().unwrap_or_default();
        self.post(
            ctx.run_id,
            Some(failed.step_id),
            "system",
            None,
            "status",
            &format!("{who} could not finish \"{}\": {reason}", failed.title),
        )
        .await?;

        // A second failure on the same work is a pattern, not a fluke.
        if failed.attempt >= 2 {
            return Ok(Some(format!("\"{}\" failed twice: {reason}", failed.title)));
        }

        let step = self
            .create_step(
                ctx.run_id,
                &format!("triage/{}", failed.key),
                Some(&manager.name),
                "Decide what to do",
                "",
                0.3,
            )
            .await?;
        let decided = self
            .run_member(
                ctx,
                step,
                manager,
                triage_prompt(who, &failed.title, &reason, &outcome.output),
                manager_session.clone(),
                MANAGER_TOOLS,
                PermissionMode::Reviewed,
                Some(
                    manager
                        .effort
                        .map_or(ctx.planning_effort, |e| e.at_least(ctx.planning_effort)),
                ),
            )
            .await?;
        let triage = if decided.status == RunStatus::Completed {
            Triage::parse(&decided.output)
        } else {
            Triage::parse("")
        };
        if !triage.note.trim().is_empty() {
            self.post(
                ctx.run_id,
                Some(step),
                &manager.name,
                None,
                "message",
                &triage.note,
            )
            .await?;
        }

        match triage.action {
            TriageAction::Abort => Ok(Some(format!("\"{}\" failed: {reason}", failed.title))),
            TriageAction::Drop => {
                self.post(
                    ctx.run_id,
                    Some(failed.step_id),
                    &manager.name,
                    None,
                    "status",
                    &format!(
                        "Dropping \"{}\" — the goal survives without it.",
                        failed.title
                    ),
                )
                .await?;
                Ok(None)
            }
            TriageAction::Retry | TriageAction::Reassign => {
                let assignee = triage
                    .assignee
                    .as_deref()
                    .and_then(|n| resolve_assignee(workers, n))
                    .unwrap_or_else(|| failed.assignee.clone());
                let task = PlannedTask {
                    key: format!("{}#2", failed.key),
                    title: failed.title.clone(),
                    brief: triage.brief.unwrap_or_else(|| failed.brief.clone()),
                    assignee: assignee.clone(),
                    done_when: failed.done_when.clone(),
                    size: failed.size,
                    depends_on: vec![],
                    touches: failed.touches.clone(),
                };
                let step_id = self
                    .create_assignment(ctx.run_id, &task, &assignee, 999.0, "replan", 2)
                    .await?;
                self.post(
                    ctx.run_id,
                    Some(step_id),
                    &manager.name,
                    Some(&assignee),
                    "assignment",
                    &format!("Another go at **{}** — {}", task.title, task.brief),
                )
                .await?;
                Ok(None)
            }
        }
    }

    async fn review(self: &Arc<Self>, ctx: &OrgCtx, manager: &Member) -> anyhow::Result<()> {
        let all = self.load_assignments(ctx.run_id).await?;
        let reports = all
            .iter()
            .filter(|a| a.status == "completed")
            .map(|a| {
                format!(
                    "### {} (by {})\n{}",
                    a.title,
                    a.assignee,
                    clip(a.output.as_deref().unwrap_or(""), DEP_CONTEXT_CHARS)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let step = self
            .create_step(
                ctx.run_id,
                "review",
                Some(&manager.name),
                "Review the work",
                "",
                1000.0,
            )
            .await?;
        let outcome = self
            .run_member(
                ctx,
                step,
                manager,
                // Fresh, despite being the manager: this passes `None`, not
                // the planning session, so the verdict is written by someone
                // who has read the reports and nothing else. It gets the
                // standing block for the same reason a worker does — without
                // it the final judgement of the work is made against facts
                // the person wrote down and nobody passed on.
                ctx.standing.apply(&format!(
                    "Your team has finished work toward this goal:\n\n{}\n\n\
                     Their reports:\n\n{reports}\n\n\
                     Inspect what actually changed in the repository and give the user a short \
                     verdict: what was accomplished, anything you'd flag, and whether it's ready \
                     to review. Be direct — say so if something looks wrong or incomplete.",
                    ctx.goal
                )),
                None,
                MANAGER_TOOLS,
                PermissionMode::Reviewed,
                manager.effort,
            )
            .await?;
        if outcome.status == RunStatus::Completed {
            self.post(
                ctx.run_id,
                Some(step),
                &manager.name,
                None,
                "result",
                &outcome.output,
            )
            .await?;
        }
        Ok(())
    }

    /// Run one member's step. Each gets its own MCP endpoint so the server
    /// knows which teammate is calling a tool.
    #[allow(clippy::too_many_arguments)]
    async fn run_member(
        self: &Arc<Self>,
        ctx: &OrgCtx,
        step_id: Uuid,
        member: &Member,
        prompt: String,
        resume: Option<String>,
        tools: &[&str],
        permission_mode: PermissionMode,
        effort: Option<ReasoningEffort>,
    ) -> anyhow::Result<StreamOutcome> {
        // Paused or retired since the team started: this assignment fails with
        // the reason, and the manager hears it as it hears any failed one — it
        // can hand the work to someone else.
        if let Err(e) = crate::agents::assert_can_run(&self.db, &[member.agent_id]).await {
            let reason = e.to_string();
            sqlx::query(
                "UPDATE steps SET status = 'failed', output_text = $2, finished_at = now()
                  WHERE id = $1",
            )
            .bind(step_id)
            .bind(&reason)
            .execute(&self.db.pool)
            .await?;
            log_mirror(epic::mirror_step(&self.db, step_id).await, step_id);
            return Ok(StreamOutcome {
                status: RunStatus::Failed,
                reason: Some(reason.clone()),
                output: reason,
                session_id: None,
            });
        }
        // A specialist brings the same connected servers into a team run that
        // it would have on a board task — an agent's capabilities shouldn't
        // depend on which surface launched it.
        let user_servers = crate::mcp_servers::for_agent(&self.db, Some(member.agent_id))
            .await
            .unwrap_or_default();

        // Engine-neutral: a team member on any MCP-capable engine gets the
        // org tools, not just Claude.
        let tool_timeout_ms = self.mcp_tool_timeout_ms().await;
        let mcp = eren_shared::McpWiring {
            eren_url: self
                .mcp_base_url
                .as_ref()
                .map(|b| format!("{b}/mcp/org/{}/{step_id}", ctx.run_id)),
            servers: user_servers.iter().map(|s| s.to_spec()).collect(),
        };

        // Org runs always pass an explicit tool list, so this is never the
        // "empty means everything" case the board path has to guard against.
        let mut allowed_tools: Vec<String> = tools.iter().map(|s| s.to_string()).collect();
        allowed_tools.extend(user_servers.iter().map(|s| s.tool_prefix()));

        // What this agent remembers about the project travels with it, the
        // same as it does for a board task.
        let recalled = memory::recall(&self.db, member.agent_id, Some(ctx.project_id))
            .await
            .ok()
            .as_deref()
            .and_then(memory::render);
        let append_system_prompt = match (
            (!member.system_prompt.is_empty()).then(|| member.system_prompt.clone()),
            recalled,
        ) {
            (Some(p), Some(m)) => Some(format!("{p}{m}")),
            (Some(p), None) => Some(p),
            (None, Some(m)) => Some(m),
            (None, None) => None,
        };

        let spec = RunSpec {
            cwd: ctx.worktree.clone(),
            prompt,
            model_tier: member.tier,
            model_id: self.model_for(ctx.engine.id(), member.tier),
            effort,
            resume_session_id: resume,
            permission_mode,
            allowed_tools,
            append_system_prompt,
            denied_tools: vec![],
            mcp,
            run_key: step_id.to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: eren_shared::brand::with_legacy_env(HashMap::from([
                ("EREN_RUN_ID".to_string(), ctx.run_id.to_string()),
                ("MCP_TOOL_TIMEOUT".to_string(), tool_timeout_ms.clone()),
            ])),
        };

        // The step row is what the live view reads to show who's busy, so
        // it has to bracket the actual work.
        sqlx::query("UPDATE steps SET status='running', started_at=now() WHERE id=$1")
            .bind(step_id)
            .execute(&self.db.pool)
            .await?;
        // The step's card follows it immediately rather than at the next pass of
        // the batch loop, which can hold for as long as the assignment takes.
        log_mirror(epic::mirror_step(&self.db, step_id).await, step_id);

        let outcome = self
            .stream_run(
                ctx.run_id,
                Some(step_id),
                &ctx.seq,
                ctx.engine.clone(),
                spec,
                CallerKind::OrgMember,
            )
            .await?;

        sqlx::query(
            "UPDATE steps SET status=$1, session_id=$2, session_engine=$5, output_text=$3,
             finished_at=now() WHERE id=$4",
        )
        .bind(outcome.status.as_str())
        .bind(&outcome.session_id)
        .bind(&outcome.output)
        .bind(step_id)
        .bind(ctx.engine.id())
        .execute(&self.db.pool)
        .await?;
        log_mirror(epic::mirror_step(&self.db, step_id).await, step_id);
        if outcome.status == RunStatus::Completed {
            if let Err(e) = epic::note_result(&self.db, step_id, &outcome.output).await {
                tracing::warn!(%step_id, error = %e, "could not post the assignment's report");
            }
        }

        Ok(outcome)
    }

    async fn remember_assignment(&self, ctx: &OrgCtx, member: &Member, title: &str, summary: &str) {
        let goal = clip(&ctx.goal, 80);
        let note = format!(
            "On {} toward \"{goal}\": finished \"{title}\" — {}",
            ctx.team_name,
            clip(summary, 200)
        );
        if let Err(e) = memory::remember(
            &self.db,
            member.agent_id,
            Some(ctx.project_id),
            ctx.task_id,
            "org_assignment",
            &note,
        )
        .await
        {
            // Never fail a completed assignment over bookkeeping.
            tracing::warn!(error = %e, "could not record org memory");
        }
    }

    /// A worker escalated a question. Wake the manager on its planning
    /// session so it answers with the full context of the plan it wrote.
    pub async fn consult_manager(
        self: &Arc<Self>,
        run_id: Uuid,
        step_id: Uuid,
        from_agent: &str,
        question: &str,
    ) -> anyhow::Result<String> {
        self.post(
            run_id,
            Some(step_id),
            from_agent,
            None,
            "question",
            question,
        )
        .await?;
        // Counted after posting, so the third question is the last one
        // answered rather than the fourth.
        let asked: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM org_messages WHERE step_id = $1 AND kind = 'question'",
        )
        .bind(step_id)
        .fetch_one(&self.db.pool)
        .await?
        .get("n");
        if asked > MAX_CONSULTS_PER_STEP {
            let answer = "You've already escalated several times — use your best judgement \
                          on this one and note the assumption in your summary.";
            self.post(
                run_id,
                Some(step_id),
                "system",
                Some(from_agent),
                "answer",
                answer,
            )
            .await?;
            return Ok(answer.to_string());
        }

        let row = sqlx::query(
            "SELECT r.engine, r.goal, COALESCE(r.worktree_path, p.path) AS cwd,
                    t.planning_effort,
                    (SELECT session_id FROM steps WHERE run_id = r.id AND step_key = 'plan') AS manager_session,
                    (SELECT assignee FROM steps WHERE run_id = r.id AND step_key = 'plan') AS manager_name,
                    (SELECT a.model_tier FROM agents a WHERE a.id = (t.definition->>'manager')::uuid) AS manager_tier,
                    (SELECT a.effort FROM agents a WHERE a.id = (t.definition->>'manager')::uuid) AS manager_effort
             FROM runs r JOIN projects p ON p.id = r.project_id
             LEFT JOIN teams t ON t.id = r.team_id
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_one(&self.db.pool)
        .await?;

        let engine_id: String = row.get("engine");
        let engine = self
            .engine(&engine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown engine {engine_id}"))?;
        let manager_name: String = row
            .get::<Option<String>, _>("manager_name")
            .unwrap_or_else(|| "Manager".into());
        let tier: ModelTier = row
            .get::<Option<String>, _>("manager_tier")
            .and_then(|t| serde_json::from_value(serde_json::Value::String(t)).ok())
            .unwrap_or(ModelTier::Medium);

        let spec = RunSpec {
            // The worktree, not the project root: a manager answering a
            // question about work in progress has to be able to see it.
            cwd: PathBuf::from(row.get::<String, _>("cwd")),
            prompt: format!(
                "{from_agent} is mid-assignment and asks:\n\n{question}\n\n\
                 Answer in two or three sentences so they can keep moving. Decide — don't \
                 hedge or hand the decision back."
            ),
            model_tier: tier,
            model_id: self.model_for(&engine_id, tier),
            effort: row
                .get::<Option<String>, _>("manager_effort")
                .and_then(|e| ReasoningEffort::parse(&e)),
            resume_session_id: row.get("manager_session"),
            permission_mode: PermissionMode::Reviewed,
            allowed_tools: MANAGER_TOOLS.iter().map(|s| s.to_string()).collect(),
            append_system_prompt: None,
            denied_tools: vec![],
            mcp: Default::default(),
            run_key: run_id.to_string(),
            extra_read_dirs: vec![],
            permission_prompt_tool: false,
            extra_env: HashMap::new(),
        };

        // Deliberately not queued: a worker's tool call is blocked waiting
        // on this, and the run already holds its concurrency permit.
        // What a local runtime holds now, not at boot — see `Engine::refresh`.
        engine.refresh().await;
        let mut proc = engine.start(spec)?;
        let mut text = String::new();
        while let Some(event) = proc.events.recv().await {
            match event {
                eren_shared::ErenEvent::RunCompleted { result_text, .. } => {
                    text = result_text;
                    break;
                }
                eren_shared::ErenEvent::AssistantText { text: t } => {
                    if text.is_empty() {
                        text = t;
                    }
                }
                eren_shared::ErenEvent::RunFailed { .. } => break,
                _ => {}
            }
        }
        if text.trim().is_empty() {
            text = "I couldn't weigh in right now — use your best judgement and flag the \
                    assumption in your summary."
                .to_string();
        }
        self.post(
            run_id,
            Some(step_id),
            &manager_name,
            Some(from_agent),
            "answer",
            &text,
        )
        .await?;
        Ok(text)
    }

    /// Append to the team's conversation.
    pub async fn post(
        &self,
        run_id: Uuid,
        step_id: Option<Uuid>,
        from_agent: &str,
        to_agent: Option<&str>,
        kind: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO org_messages (run_id, step_id, from_agent, to_agent, kind, content)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(run_id)
        .bind(step_id)
        .bind(from_agent)
        .bind(to_agent)
        .bind(kind)
        .bind(content)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    /// A manager step: thinking, not work assigned to anyone.
    async fn create_step(
        &self,
        run_id: Uuid,
        key: &str,
        assignee: Option<&str>,
        title: &str,
        brief: &str,
        position: f64,
    ) -> anyhow::Result<Uuid> {
        let row = sqlx::query(
            "INSERT INTO steps (run_id, step_key, status, assignee, title, brief, position)
             VALUES ($1,$2,'queued',$3,$4,$5,$6)
             ON CONFLICT (run_id, step_key) DO UPDATE SET status='queued'
             RETURNING id",
        )
        .bind(run_id)
        .bind(key)
        .bind(assignee)
        .bind(title)
        .bind(brief)
        .bind(position)
        .fetch_one(&self.db.pool)
        .await?;
        Ok(row.get("id"))
    }

    async fn create_assignment(
        &self,
        run_id: Uuid,
        task: &PlannedTask,
        assignee: &str,
        position: f64,
        origin: &str,
        attempt: i32,
    ) -> anyhow::Result<Uuid> {
        let row = sqlx::query(
            "INSERT INTO steps (run_id, step_key, status, assignee, title, brief,
                                done_when, size, depends_on, touches, position, origin, attempt)
             VALUES ($1,$2,'queued',$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT (run_id, step_key) DO NOTHING
             RETURNING id",
        )
        .bind(run_id)
        .bind(&task.key)
        .bind(assignee)
        .bind(&task.title)
        .bind(&task.brief)
        .bind(&task.done_when)
        .bind(task.size.as_str())
        .bind(&task.depends_on)
        .bind(&task.touches)
        .bind(position)
        .bind(origin)
        .bind(attempt)
        .fetch_optional(&self.db.pool)
        .await?;
        match row {
            Some(r) => Ok(r.get("id")),
            // The unique index absorbed a duplicate; reuse the existing row.
            None => {
                let existing = sqlx::query("SELECT id FROM steps WHERE run_id=$1 AND step_key=$2")
                    .bind(run_id)
                    .bind(&task.key)
                    .fetch_one(&self.db.pool)
                    .await?;
                Ok(existing.get("id"))
            }
        }
    }

    pub(crate) async fn load_assignments(&self, run_id: Uuid) -> anyhow::Result<Vec<Assignment>> {
        let rows = sqlx::query(&format!(
            "SELECT id, step_key, title, brief, assignee, done_when, size, depends_on,
                    touches, status, output_text, attempt
             FROM steps WHERE run_id = $1 AND {NOT_AN_ASSIGNMENT}
             ORDER BY position NULLS LAST, started_at NULLS LAST, id"
        ))
        .bind(run_id)
        .fetch_all(&self.db.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Assignment {
                step_id: r.get("id"),
                key: r.get("step_key"),
                title: r.get::<Option<String>, _>("title").unwrap_or_default(),
                brief: r.get::<Option<String>, _>("brief").unwrap_or_default(),
                assignee: r.get::<Option<String>, _>("assignee").unwrap_or_default(),
                done_when: r.get("done_when"),
                size: r
                    .get::<Option<String>, _>("size")
                    .map(|s| TaskSize::parse(&s))
                    .unwrap_or_default(),
                depends_on: r.get("depends_on"),
                touches: r.get("touches"),
                status: r.get("status"),
                output: r.get("output_text"),
                attempt: r.get("attempt"),
            })
            .collect())
    }

    async fn resolve_member(
        &self,
        workspace_id: Uuid,
        id: Option<&serde_json::Value>,
        title: &str,
    ) -> anyhow::Result<Option<Member>> {
        let Some(agent_id) = id
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return Ok(None);
        };
        Ok(self.load_member(workspace_id, agent_id, title).await?)
    }

    async fn resolve_members(
        &self,
        workspace_id: Uuid,
        definition: &serde_json::Value,
    ) -> anyhow::Result<Vec<Member>> {
        let entries = definition
            .get("members")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut members = vec![];
        for entry in entries {
            let Some(agent_id) = entry
                .get("agent_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            let title = entry
                .get("role")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Specialist");
            if let Some(member) = self.load_member(workspace_id, agent_id, title).await? {
                members.push(member);
            }
        }
        Ok(members)
    }

    /// Scoped to the workspace: two agents may share a name across
    /// workspaces, and picking the wrong one silently misroutes work.
    async fn load_member(
        &self,
        workspace_id: Uuid,
        agent_id: Uuid,
        title: &str,
    ) -> anyhow::Result<Option<Member>> {
        let row = sqlx::query(
            "SELECT id, name, description, model_tier, effort, system_prompt
             FROM agents WHERE id = $1 AND workspace_id = $2",
        )
        .bind(agent_id)
        .bind(workspace_id)
        .fetch_optional(&self.db.pool)
        .await?;
        Ok(row.map(|r| Member {
            agent_id: r.get("id"),
            name: r.get("name"),
            title: title.to_string(),
            description: r.get("description"),
            // Same reasoning as `load_agent`: `auto` is not offered for a
            // roster member yet, and resolving it here rather than through an
            // unwrap_or_default keeps that a stated decision.
            tier: eren_shared::TierChoice::parse(&r.get::<String, _>("model_tier"))
                .and_then(eren_shared::TierChoice::fixed)
                .unwrap_or_default(),
            effort: r
                .get::<Option<String>, _>("effort")
                .and_then(|e| ReasoningEffort::parse(&e)),
            system_prompt: r.get("system_prompt"),
        }))
    }
}

/// What a specialist needs to know about work already done.
///
/// Its declared dependencies are worth reading; everything else is a one-line
/// index so it knows the work exists without carrying every word of it.
pub fn context_for(task: &Assignment, completed: &[&Assignment]) -> String {
    if completed.is_empty() {
        return String::new();
    }
    let deps: HashSet<&str> = task.depends_on.iter().map(|d| d.trim()).collect();
    let mut detailed = vec![];
    let mut index = vec![];

    for done in completed {
        let output = done.output.as_deref().unwrap_or("").trim();
        if deps.contains(done.key.as_str()) && !output.is_empty() {
            detailed.push(format!(
                "### {} (by {})\n{}",
                done.title,
                done.assignee,
                clip(output, DEP_CONTEXT_CHARS)
            ));
        } else {
            index.push(format!("- {} (by {}) — done", done.title, done.assignee));
        }
    }

    let mut body = String::new();
    if !detailed.is_empty() {
        body.push_str("\n\nWhat your assignment builds on:\n");
        body.push_str(&detailed.join("\n\n"));
    }
    if !index.is_empty() {
        body.push_str("\n\nAlso finished (ask if you need detail):\n");
        body.push_str(&index.join("\n"));
    }
    // Oldest non-dependency detail is the first thing worth losing.
    clip(&body, MAX_CONTEXT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(key: &str, depends_on: &[&str]) -> Assignment {
        Assignment {
            step_id: Uuid::new_v4(),
            key: key.into(),
            title: format!("Do {key}"),
            brief: String::new(),
            assignee: "Rex".into(),
            done_when: vec![],
            size: TaskSize::Medium,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            touches: vec![],
            status: "queued".into(),
            output: None,
            attempt: 1,
        }
    }

    #[test]
    fn context_gives_dependencies_in_full_and_everyone_else_a_line() {
        let mut api = assignment("api", &[]);
        api.status = "completed".into();
        api.output = Some("Added GET /leads returning JSON".into());
        let mut docs = assignment("docs", &[]);
        docs.status = "completed".into();
        docs.output = Some("Wrote the README section".into());

        let ui = assignment("ui", &["api"]);
        let context = context_for(&ui, &[&api, &docs]);

        assert!(context.contains("Added GET /leads returning JSON"));
        assert!(context.contains("- Do docs (by Rex) — done"));
        assert!(!context.contains("Wrote the README section"));
    }

    #[test]
    fn context_is_empty_when_nothing_has_finished() {
        assert_eq!(context_for(&assignment("a", &[]), &[]), "");
    }

    #[test]
    fn context_stays_bounded_however_much_was_written() {
        let mut huge = assignment("api", &[]);
        huge.status = "completed".into();
        huge.output = Some("x".repeat(50_000));
        let ui = assignment("ui", &["api"]);
        let context = context_for(&ui, &[&huge]);
        assert!(context.chars().count() <= MAX_CONTEXT_CHARS + 1);
    }
}
