use super::{attachments, internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use eren_core::runs::follow_up::FollowUp;
use eren_core::runs::mentions;
use eren_core::runs::orchestrator::Variant;
use eren_core::scope::Owned;
use eren_shared::{PermissionMode, ReasoningEffort, TierChoice};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(create))
        .route(
            "/tasks/{id}",
            axum::routing::patch(move_task_route).delete(delete_task),
        )
        .route("/tasks/{id}/blockers", post(add_blocker))
        .route(
            "/tasks/{id}/blockers/{blocker_id}",
            axum::routing::delete(remove_blocker),
        )
        .route("/tasks/{id}/retry", post(retry))
        .route("/tasks/{id}/comments", get(comments).post(post_comment))
        .route(
            "/tasks/{id}/articles",
            get(task_articles).put(set_task_articles),
        )
        .route("/tasks/{id}/attachments/claim", post(attach_to_task))
        .route("/tasks/{id}/start", post(start))
        .route("/tasks/{id}/bakeoff", get(bakeoff).post(start_bakeoff))
        .route("/tasks/{id}/runs", get(task_runs))
        .route("/tasks/{id}/base", get(base_status))
        .route("/tasks/{id}/update-from-base", post(update_from_base))
        .route("/runs/{id}/keep", post(keep_variant))
        .route("/tasks/{id}/diff", get(diff))
        .route("/tasks/{id}/merge", post(merge))
        .route("/runs/{id}/events", get(run_events))
        .route("/runs/{id}/pending-permissions", get(pending_permissions))
        .route("/runs/{id}/cancel", post(cancel_run_route))
        .route("/runs/{id}/resume", post(resume_run))
        .route("/runs/{id}/plan", get(plan).patch(edit_plan))
        .route("/runs/{id}/plan/approve", post(approve_plan))
        .route("/runs/{id}/plan/revise", post(revise_plan))
        .route(
            "/permissions/{request_id}/resolve",
            post(resolve_permission),
        )
}

#[derive(Deserialize)]
struct TaskFilter {
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
}

/// A stored level, or none. Anything unparseable is treated as unset rather
/// than as an error: a row that predates a level being renamed should inherit,
/// not break the board.
fn parse_effort(stored: Option<String>) -> Option<ReasoningEffort> {
    stored.as_deref().and_then(ReasoningEffort::parse)
}

async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(filter): Query<TaskFilter>,
) -> Result<Json<Value>, ApiError> {
    let workspaces = caller.workspace_filter(&state, filter.workspace_id).await?;
    caller
        .require_opt(&state, filter.project_id, Owned::Project)
        .await?;
    let rows = sqlx::query(
        // The epic columns ride along on the one query the board already makes.
        // Counting children per card from the client would be an N+1 over a list
        // that refreshes every 2.5 seconds.
        //
        // The roll-up counts `board_column`, not step status, on purpose: it has
        // to keep telling the truth after the org run — and its steps — have been
        // deleted, which is exactly when someone is looking back at what an epic
        // turned into.
        "SELECT t.id, t.title, t.prompt, t.model_tier, t.board_column, t.branch, t.position,
                (SELECT COALESCE(json_agg(json_build_object(
                         'id', b.id, 'title', b.title, 'boardColumn', b.board_column)
                         ORDER BY b.title), '[]'::json)
                   FROM task_deps d JOIN tasks b ON b.id = d.blocked_by
                  WHERE d.task_id = t.id) AS blocked_by,
                t.pr_number, t.pr_url, t.pr_state, t.pr_checks, t.pr_review,
                t.project_id, t.agent_id, COALESCE(a.engine, t.engine) AS engine, t.plan_first,
                t.start_when_unblocked, t.blocked_note,
                a.name AS agent_name, a.color AS agent_color, a.status AS agent_status,
                t.skill_id, sk.name AS skill_name, t.goal_id,
                (SELECT title FROM goals WHERE id = t.goal_id) AS goal_title,
                t.team_id, tm.name AS team_name, tm.pattern AS team_pattern,
                t.parent_id, parent.title AS parent_title,
                COALESCE(kids.total, 0) AS child_count,
                COALESCE(kids.resolved, 0) AS child_resolved,
                s.status AS step_status, t.effort AS card_effort,
                a.effort AS agent_effort,
                -- The mode this card will actually run under, and which of the
                -- three places decided it. Resolved here in exactly the order
                -- the orchestrator resolves it (orchestrator.rs:1090) so the
                -- board cannot disagree with what the run does.
                --
                -- Worth showing at all because the precedence surprises people:
                -- a project set to work without asking still prompts when the
                -- bound agent carries its own preset, and nothing said so.
                COALESCE(a.permission_preset, t.permission_mode,
                         (SELECT value #>> '{}' FROM settings
                           WHERE key = 'default_permission_mode'),
                         'reviewed') AS effective_mode,
                CASE WHEN a.permission_preset IS NOT NULL THEN 'agent'
                     WHEN t.permission_mode IS NOT NULL THEN 'card'
                     ELSE 'default' END AS permission_source,
                r.id AS run_id, r.status AS run_status, r.error_reason AS run_error,
                -- Should this card offer Resume? Everything here is a column
                -- on the row the LATERAL already fetched, so it costs nothing.
                --
                -- Narrower than `resume::decide`, deliberately. `decide` also
                -- accepts a *completed* run, because continuing one is not
                -- wrong — but 22 of 23 cards here have one, and a button on
                -- every card is not an offer, it is noise. What Resume is for
                -- is a run that stopped short.
                --
                -- The team/workflow/comment/KB exclusions mirror
                -- `Refusal::NotATaskRun`: without them the board offered
                -- Resume on team cards and the click refused, which is the
                -- same asymmetry a silent capability downgrade would be.
                --
                -- The worktree clause is the reason this is not just a run
                -- predicate: `drop_worktree` nulls the column, and a session
                -- resumed into a checkout that no longer exists believes its
                -- edits are there and reports success over nothing. A project
                -- without git never had one, which is not the same thing.
                --
                -- What is left out is only what SQL cannot answer: whether the
                -- engine can resume at all (a registry lookup) and whether the
                -- directory is still on disk (a stat). Neither is affordable
                -- once per card every 2.5 seconds, so both are answered on the
                -- click, as a 409 that says which.
                (r.session_id IS NOT NULL
                 AND r.session_engine = r.engine
                 AND r.status IN ('failed', 'canceled')
                 AND r.team_id IS NULL AND r.workflow_id IS NULL
                 AND r.comment_id IS NULL AND r.kb_brief IS NULL
                 AND (p.vcs <> 'git'
                      OR COALESCE(r.worktree_path, t.worktree_path) IS NOT NULL)) AS run_resumable,
                r.cost_usd, r.model,
                spent.run_count, spent.total_cost,
                ck.status AS checks_status, ck.passed AS checks_passed, ck.total AS checks_total,
                r.tier_resolved, r.tier_reason,
                r.team_id AS run_team_id
         FROM tasks t
         JOIN projects p ON p.id = t.project_id
         LEFT JOIN agents a ON a.id = t.agent_id
         LEFT JOIN skills sk ON sk.id = t.skill_id
         LEFT JOIN teams tm ON tm.id = t.team_id
         LEFT JOIN tasks parent ON parent.id = t.parent_id
         LEFT JOIN steps s ON s.task_id = t.id
         LEFT JOIN LATERAL (
             SELECT count(*) AS total,
                    count(*) FILTER (WHERE board_column IN ('review','done')) AS resolved
             FROM tasks c WHERE c.parent_id = t.id
         ) kids ON TRUE
         LEFT JOIN LATERAL (
             SELECT * FROM runs WHERE task_id = t.id ORDER BY created_at DESC LIMIT 1
         ) r ON TRUE
         -- Every attempt, not just the newest: a retry or a follow-up costs
         -- money too, and a card that showed only its last run's dollars
         -- understated itself by everything before it.
         LEFT JOIN LATERAL (
             SELECT count(*) AS run_count, SUM(cost_usd) AS total_cost
               FROM runs WHERE task_id = t.id
         ) spent ON TRUE
         -- The newest check run, as counts only: its output can be tens of
         -- kilobytes per command, and this list is fetched every few seconds.
         LEFT JOIN LATERAL (
             SELECT c.status,
                    (SELECT count(*) FROM jsonb_array_elements(c.results) e
                      WHERE e->>'exitCode' = '0' AND e->>'timedOut' = 'false') AS passed,
                    GREATEST(jsonb_array_length(c.results),
                             COALESCE((SELECT jsonb_array_length(commands) FROM project_checks
                                        WHERE project_id = t.project_id), 0)) AS total
               FROM check_runs c WHERE c.task_id = t.id
              ORDER BY c.created_at DESC LIMIT 1
         ) ck ON TRUE
         WHERE ($1::uuid[] IS NULL OR p.workspace_id = ANY($1))
           AND ($2::uuid IS NULL OR t.project_id = $2)
         ORDER BY t.position, t.created_at",
    )
    .bind(workspaces)
    .bind(filter.project_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    // Effort is the one thing here the database cannot settle on its own: the
    // tier's budget depends on which engine the card resolved to, and that
    // mapping lives in a settings blob rather than a column. Both halves are
    // read once for the whole list rather than per row.
    let tier_efforts = state.orchestrator.tier_efforts();
    let machine_default = state.orchestrator.default_effort().await;
    let tasks: Vec<Value> = rows
        .iter()
        .map(|r| {
            // The same function the orchestrator resolves with, so the board
            // cannot disagree with what the run actually does.
            // An `auto` card has no tier until it runs, so the board shows
            // the effort it would get at Medium. Marked in the payload below
            // so the UI can say "decided per run" rather than state a figure
            // as though it were settled.
            let choice =
                TierChoice::parse(&r.get::<String, _>("model_tier")).unwrap_or(TierChoice::Medium);
            let tier = choice.fixed().unwrap_or_default();
            let (effective_effort, effort_source) = eren_shared::resolve_effort(
                parse_effort(r.get("agent_effort")),
                parse_effort(r.get("card_effort")),
                tier_efforts.effort_for(&r.get::<String, _>("engine"), tier),
                machine_default,
            );
            json!({
                "id": r.get::<Uuid, _>("id"),
                "title": r.get::<String, _>("title"),
                // The card's own words. Selected since the beginning, emitted
                // never — the drawer had no way to show what a card asks for.
                "prompt": r.get::<String, _>("prompt"),
                "blockedBy": r.get::<serde_json::Value, _>("blocked_by"),
                "modelTier": r.get::<String, _>("model_tier"),
                // True when the tier is not settled until the run starts.
                "tierIsAuto": choice == TierChoice::Auto,
                "boardColumn": r.get::<String, _>("board_column"),
                "position": r.get::<f64, _>("position"),
                "branch": r.get::<Option<String>, _>("branch"),
                "projectId": r.get::<Uuid, _>("project_id"),
                "agentId": r.get::<Option<Uuid>, _>("agent_id"),
                "agentName": r.get::<Option<String>, _>("agent_name"),
                "skillId": r.get::<Option<Uuid>, _>("skill_id"),
                "skillName": r.get::<Option<String>, _>("skill_name"),
                "goalId": r.get::<Option<Uuid>, _>("goal_id"),
                "goalTitle": r.get::<Option<String>, _>("goal_title"),
                "agentColor": r.get::<Option<String>, _>("agent_color"),
                "teamId": r.get::<Option<Uuid>, _>("team_id"),
                "teamName": r.get::<Option<String>, _>("team_name"),
                "teamPattern": r.get::<Option<String>, _>("team_pattern"),
                "parentId": r.get::<Option<Uuid>, _>("parent_id"),
                "parentTitle": r.get::<Option<String>, _>("parent_title"),
                "childCount": r.get::<i64, _>("child_count"),
                "childResolved": r.get::<i64, _>("child_resolved"),
                // The raw assignment status, so the card can say "failed" or
                // "dropped" — things the four columns have no room for.
                "stepStatus": r.get::<Option<String>, _>("step_status"),
                "effectiveMode": r.get::<String, _>("effective_mode"),
                "effort": r.get::<Option<String>, _>("card_effort"),
                "effectiveEffort": effective_effort.map(|e| e.as_str()),
                "effortSource": effort_source.as_str(),
                "permissionSource": r.get::<String, _>("permission_source"),
                "orgRunId": r.get::<Option<Uuid>, _>("run_team_id")
                    .and(r.get::<Option<Uuid>, _>("run_id")),
                "runId": r.get::<Option<Uuid>, _>("run_id"),
                "runStatus": r.get::<Option<String>, _>("run_status"),
                // The last thing said about this run — which is not always an
                // error. A parked run carries "waiting for you to allow Bash"
                // here and `unpark` clears it again, so the client decides the
                // treatment from the *pair*; see `stopReason` in runStatus.ts.
                "runError": r.get::<Option<String>, _>("run_error"),
                // NULL when the card has never run at all — the LATERAL join
                // produced no row — so this is an Option, not a bool.
                "runResumable": r.get::<Option<bool>, _>("run_resumable").unwrap_or(false),
                "costUsd": r.get::<Option<f64>, _>("cost_usd"),
                "runCount": r.get::<i64, _>("run_count"),
                // Named apart from `prChecks`, which is GitHub's CI.
                "localChecks": r.get::<Option<String>, _>("checks_status").map(|status| json!({
                    "status": status,
                    "passed": r.get::<Option<i64>, _>("checks_passed").unwrap_or(0),
                    "total": r.get::<Option<i32>, _>("checks_total").unwrap_or(0),
                })),
                "totalCostUsd": r.get::<Option<f64>, _>("total_cost"),
                // Enough for the chip. Anything more — how fresh it is, why
                // the button is refused — is the drawer's own fetch, because
                // this list refreshes every couple of seconds.
                "prNumber": r.get::<Option<i32>, _>("pr_number"),
                "prUrl": r.get::<Option<String>, _>("pr_url"),
                "prState": r.get::<Option<String>, _>("pr_state"),
                "prChecks": r.get::<Option<String>, _>("pr_checks"),
                "prReview": r.get::<Option<String>, _>("pr_review"),
                "model": r.get::<Option<String>, _>("model"),
                // What the last run actually ran at, and why — the whole
                // point of an automatic choice being allowed to happen at all.
                "tierResolved": r.get::<Option<String>, _>("tier_resolved"),
                "tierReason": r.get::<Option<String>, _>("tier_reason"),
                "engine": r.get::<String, _>("engine"),
                "planFirst": r.get::<bool, _>("plan_first"),
                "startWhenUnblocked": r.get::<bool, _>("start_when_unblocked"),
                "agentStatus": r.get::<Option<String>, _>("agent_status"),
                // What the agent said stopped it, until the card next starts.
                "blockedNote": r.get::<Option<String>, _>("blocked_note"),
            })
        })
        .collect();
    Ok(Json(json!({ "tasks": tasks })))
}

/// Cards default to Medium, unchanged. Auto is opt-in: a router that turned
/// itself on for everyone would be making the choice it exists to surface.
fn default_tier_choice() -> TierChoice {
    TierChoice::Medium
}

#[derive(Deserialize)]
struct CreateTask {
    project_id: Uuid,
    title: String,
    prompt: String,
    /// `auto` included: the card stores the *choice*, and which tier that
    /// becomes is decided per run, once the phase and the card's shape are
    /// known.
    #[serde(default = "default_tier_choice")]
    model_tier: TierChoice,
    /// Absent means "use the workspace default", which is not the same as
    /// asking for Reviewed — `#[serde(default)]` here would silently force
    /// prompts on every client that doesn't name a mode.
    permission_mode: Option<PermissionMode>,
    #[serde(default)]
    start: bool,
    agent_id: Option<Uuid>,
    /// How this job gets done here. Composes with the agent: one is who, the
    /// other is how.
    skill_id: Option<Uuid>,
    /// Hand the whole task to a team instead of a single agent.
    team_id: Option<Uuid>,
    /// Engine id from `/api/engines`. Omitted means the machine default.
    engine: Option<String>,
    /// Write a plan and stop, so a person can confirm or rewrite it before any
    /// work happens.
    #[serde(default)]
    plan_first: bool,
    /// Omitted inherits: the bound agent's budget if it has one, else the
    /// machine default, resolved when the run dispatches.
    effort: Option<ReasoningEffort>,
    /// Knowledge-base articles the agent should read before starting.
    #[serde(default)]
    article_ids: Vec<Uuid>,
    /// Ids from POST /api/projects/{id}/attachments, bound to this task on
    /// create. Defaulted so existing clients keep working.
    #[serde(default)]
    attachment_ids: Vec<Uuid>,
    /// Start by itself once every card blocking it has landed.
    #[serde(default)]
    start_when_unblocked: bool,
    /// The person saw the forecast and starts anyway.
    #[serde(default)]
    acknowledge_forecast: bool,
    /// The goal this card serves.
    #[serde(default)]
    goal_id: Option<Uuid>,
}

async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<CreateTask>,
) -> Result<Json<Value>, ApiError> {
    // The card's project, and everything the card is bound to. The articles
    // and attachments need nothing here: `link_articles` holds each to the
    // card's workspace and `attachments::claim` to its project.
    caller
        .require(&state, Owned::Project(body.project_id))
        .await?;
    caller
        .require_opt(&state, body.agent_id, Owned::Agent)
        .await?;
    caller
        .require_opt(&state, body.skill_id, Owned::Skill)
        .await?;
    caller
        .require_opt(&state, body.team_id, Owned::Team)
        .await?;
    caller
        .require_opt(&state, body.goal_id, Owned::Goal)
        .await?;
    if let Some(agent_id) = body.agent_id {
        assignable(&state, agent_id).await?;
    }
    let tier = body.model_tier.as_str();
    // Store NULL when the caller didn't choose, so the card inherits whatever
    // the default is *when it runs* rather than freezing today's value.
    let mode: Option<String> = body.permission_mode.map(|m| {
        serde_json::to_value(m)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    });
    let row = sqlx::query(
        "INSERT INTO tasks (project_id, title, prompt, model_tier, permission_mode, engine, agent_id, skill_id, team_id, board_column, plan_first, effort, start_when_unblocked, goal_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'backlog',$10,$11,$12,
                 -- Only a goal of the project's own workspace.
                 (SELECT g.id FROM goals g JOIN projects p ON p.workspace_id = g.workspace_id
                   WHERE g.id = $13 AND p.id = $1)) RETURNING id",
    )
    .bind(body.project_id)
    .bind(&body.title)
    .bind(&body.prompt)
    .bind(tier)
    .bind(mode.as_deref())
    .bind(body.engine.clone().unwrap_or_else(|| state.orchestrator.default_engine()))
    .bind(body.agent_id)
    .bind(body.skill_id)
    .bind(body.team_id)
    .bind(body.plan_first)
    .bind(body.effort.map(|e| e.as_str().to_string()))
    .bind(body.start_when_unblocked)
    .bind(body.goal_id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;
    let task_id: Uuid = row.get("id");
    // Handed to an agent that pulls its own work, and not started here: the
    // agent hears about it now rather than at its next beat.
    if let (Some(agent), false) = (body.agent_id, body.start) {
        eren_core::heartbeat::wake(&state.db, agent, "assigned", Some(task_id)).await;
    }
    // Before the run is enqueued, for the same reason attachments are: the
    // prompt is assembled from whatever is bound when the run is picked up.
    link_articles(&state, task_id, &body.article_ids).await?;

    // Must happen before the run is enqueued: the orchestrator assembles the
    // prompt from whatever is bound at the time it picks the run up.
    attachments::claim(
        &state.db,
        &body.attachment_ids,
        attachments::Home::Project(body.project_id),
        attachments::Owner::Task(task_id),
    )
    .await?;

    let run_id = if body.start {
        // Same gate as `start`: a card created with start=true must not slip
        // past the capability check.
        vet_task(&state, task_id).await?;
        // The card exists now, in the backlog. Say which, so "start anyway"
        // starts this one rather than making a second.
        if let Err((code, message)) =
            ask_about_cost(&state, task_id, body.acknowledge_forecast).await
        {
            let mut ask: Value =
                serde_json::from_str(&message).unwrap_or_else(|_| json!({ "message": message }));
            ask["taskId"] = json!(task_id);
            return Err((code, ask.to_string()));
        }
        let id = state
            .orchestrator
            .enqueue_task(task_id)
            .await
            .map_err(start_refused)?;
        sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
            .bind(task_id)
            .execute(&state.db.pool)
            .await
            .map_err(internal)?;
        Some(id)
    } else {
        None
    };
    Ok(Json(json!({ "id": task_id, "runId": run_id })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct StartBody {
    /// The person saw the forecast and starts anyway.
    pub(crate) acknowledge_forecast: bool,
}

pub(crate) async fn start(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<StartBody>>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    vet_task(&state, id).await?;
    ask_about_cost(
        &state,
        id,
        body.is_some_and(|Json(b)| b.acknowledge_forecast),
    )
    .await?;
    let run_id = state
        .orchestrator
        .enqueue_task(id)
        .await
        .map_err(start_refused)?;
    sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "runId": run_id })))
}

/// Ask first when similar runs say this start could overrun a budget. The
/// 409's body is JSON, so the client can show the numbers and offer "start
/// anyway", which re-sends with `acknowledge_forecast`.
async fn ask_about_cost(
    state: &AppState,
    task_id: Uuid,
    acknowledged: bool,
) -> Result<(), ApiError> {
    match eren_core::budgets::forecast_check(&state.db, task_id, acknowledged)
        .await
        .map_err(internal)?
    {
        Ok(()) => Ok(()),
        Err(ask) => {
            let mut body = serde_json::to_value(&ask).map_err(internal)?;
            body["kind"] = json!("forecast");
            body["message"] = json!(ask.to_string());
            Err((StatusCode::CONFLICT, body.to_string()))
        }
    }
}

/// Refuse to queue a card its engine cannot honour, or that is already being
/// worked on as part of an epic.
///
/// The same check runs again at dispatch, but doing it here means the user
/// sees the reason on the click that caused it rather than as a failed run.
pub(crate) async fn vet_task(state: &AppState, task_id: Uuid) -> Result<(), ApiError> {
    if step_is_live(state, task_id).await? {
        return Err((
            StatusCode::CONFLICT,
            "a teammate is already working on this sub-task as part of its epic".into(),
        ));
    }
    // The orchestrator refuses this too — it is the last line — but a clean
    // 409 with the blockers' names beats a 500 wrapping the same sentence.
    let blockers: Vec<String> = sqlx::query_scalar(
        "SELECT b.title FROM task_deps d JOIN tasks b ON b.id = d.blocked_by
         WHERE d.task_id = $1 AND b.board_column <> 'done' ORDER BY b.title",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    if !blockers.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "blocked by {} — land {} first",
                blockers.join(", "),
                if blockers.len() == 1 {
                    "that card"
                } else {
                    "those cards"
                }
            ),
        ));
    }
    match state.orchestrator.vet_card(task_id).await {
        Ok(Some(reason)) => Err((StatusCode::CONFLICT, reason)),
        Ok(None) => Ok(()),
        Err(e) if matches!(e.downcast_ref(), Some(sqlx::Error::RowNotFound)) => {
            Err((StatusCode::NOT_FOUND, "no such task".to_string()))
        }
        Err(e) => Err(internal(e)),
    }
}

async fn diff(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let row = sqlx::query(
        "SELECT t.worktree_path, p.default_branch FROM tasks t
         JOIN projects p ON p.id = t.project_id WHERE t.id=$1",
    )
    .bind(id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;
    let Some(worktree): Option<String> = row.get("worktree_path") else {
        return Ok(Json(json!({ "diff": "" })));
    };
    let base: String = row.get("default_branch");
    let diff = state
        .orchestrator
        .worktrees
        .diff(std::path::Path::new(&worktree), &base)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "diff": diff })))
}

/// Merge anyway, past what the project's review policy still wants — with a
/// note saying why, which goes on the card and into the audit log.
#[derive(Deserialize, Default, Debug)]
struct MergeBody {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    note: String,
}

const MAX_OVERRIDE_NOTE: usize = 500;

/// The dashboard's Merge sends no body at all (with a JSON content type,
/// which `Option<Json<_>>` refuses as malformed), so an empty body is the
/// ordinary merge and only a non-empty one is read.
fn merge_body(raw: &[u8]) -> Result<MergeBody, ApiError> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(MergeBody::default());
    }
    serde_json::from_slice(raw)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("not a merge request: {e}")))
}

async fn merge(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let body = merge_body(&body)?;
    let row = sqlx::query(
        "SELECT t.title, t.worktree_path, t.branch, t.project_id, p.path AS project_path,
                p.default_branch, p.vcs
         FROM tasks t JOIN projects p ON p.id = t.project_id WHERE t.id=$1",
    )
    .bind(id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?;
    // Say which of the two situations this is: a project with no version
    // control can never have a worktree, and "not yet" would be misleading.
    if row.get::<String, _>("vcs") != "git" {
        return Err((
            StatusCode::BAD_REQUEST,
            "this project has no version control, so its tasks edit the folder \
             directly — there is nothing to merge"
                .into(),
        ));
    }
    let (Some(worktree), Some(branch)): (Option<String>, Option<String>) =
        (row.get("worktree_path"), row.get("branch"))
    else {
        return Err((StatusCode::BAD_REQUEST, "task has no worktree yet".into()));
    };
    // Landing commits whatever is in the worktree and then deletes it. With a
    // run still writing there — a follow-up, a resume, an epic's step — that
    // squash-merged half a change and pulled the directory out from under the
    // agent mid-edit.
    state
        .orchestrator
        .supersede_summary(id)
        .await
        .map_err(internal)?;
    if any_run_is_live(&state, id).await? || step_is_live(&state, id).await? {
        return Err((
            StatusCode::CONFLICT,
            "an agent is still working on this card — wait for it to finish or cancel it, \
             then merge"
                .into(),
        ));
    }
    // What the project's review policy asks of this click. The pull request
    // is asked about once more first, so the gate reads GitHub's word now and
    // not whenever the card was last synced.
    if eren_core::review::policy(&state.db, row.get("project_id"))
        .await
        .map_err(internal)?
        .require_pr_green
    {
        super::pull_requests::sync_for_gate(&state, id).await;
    }
    let unmet = eren_core::review::gate(&state.db, id)
        .await
        .map_err(internal)?;
    let override_note = body.note.trim();
    if !unmet.is_empty() {
        if !body.force {
            return Err((
                StatusCode::CONFLICT,
                json!({ "kind": "gate", "unmet": unmet }).to_string(),
            ));
        }
        if override_note.is_empty() || override_note.chars().count() > MAX_OVERRIDE_NOTE {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "merging past the review policy needs a note saying why, under \
                     {MAX_OVERRIDE_NOTE} characters"
                ),
            ));
        }
    }
    let wt = eren_core::worktrees::manager::Worktree {
        path: worktree.into(),
        branch,
    };
    let title: String = row.get("title");
    state
        .orchestrator
        .worktrees
        .squash_merge(
            std::path::Path::new(&row.get::<String, _>("project_path")),
            &wt,
            &row.get::<String, _>("default_branch"),
            &format!("eren: {title}"),
        )
        .await
        .map_err(merge_refused)?;
    if !unmet.is_empty() {
        // Said where the next person reading the card will look, and kept
        // where nobody can edit it away.
        let said = format!(
            "Merged past the review policy, which still wanted: {}\n\nWhy: {override_note}",
            unmet
                .iter()
                .map(|u| u.message.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Err(e) = eren_core::runs::report::post_system(&state.db, id, None, &said).await {
            tracing::warn!(%id, error = %e, "could not note the override on the card");
        }
        eren_core::audit::record(
            &state.db,
            eren_core::audit::Entry::new(eren_core::audit::Actor::Api, "merge past review policy")
                .on("tasks", id)
                .summary(override_note.chars().take(200).collect::<String>())
                .detail(json!({ "unmet": unmet.iter().map(|u| u.kind).collect::<Vec<_>>() })),
        )
        .await;
    }
    sqlx::query("UPDATE tasks SET board_column='done' WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    // Checks still waiting or going would run against a worktree about to be
    // deleted, and report on code that has already landed.
    eren_core::checks::cancel_for_task(&state.db, id)
        .await
        .map_err(internal)?;
    // The work is on the base branch now, so a card waiting on it can branch
    // from there and find it.
    state.orchestrator.landed(id).await;

    // The card has landed, so the checkout it was built in is finished with.
    //
    // Until this line, merging was the one way a card could end without ever
    // giving anything back: `drop_worktree` was reached only from Delete and
    // from a fresh Retry, so *every card anyone ever merged* kept a worktree
    // and an `eren/*` branch permanently. On this machine that was 2.9 GB
    // across 22 directories before anybody noticed.
    //
    // Safe precisely here and not in general: the operation that just
    // succeeded is what put this branch into the base, so there is no
    // unmerged work to lose. The pull request survives — `tasks.pr_number`
    // and friends are columns for exactly this reason, so nothing needs the
    // branch name to find it again.
    //
    // Best-effort: a merge that landed must not report failure because a
    // directory could not be removed.
    if let Err(e) = drop_worktree(&state, id).await {
        tracing::warn!(task = %id, error = ?e, "merged, but could not reclaim the worktree");
    }
    Ok(Json(json!({ "merged": true })))
}

async fn run_events(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(id)).await?;
    let rows = sqlx::query(
        "SELECT seq, type, payload, ts, step_id FROM events WHERE run_id=$1 ORDER BY seq ASC",
    )
    .bind(id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    let events: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "seq": r.get::<i64, _>("seq"),
                "ts": r.get::<chrono::DateTime<chrono::Utc>, _>("ts"),
                // Which step produced it — the only way a multi-agent run can
                // attribute an action to a teammate.
                "stepId": r.get::<Option<Uuid>, _>("step_id"),
                "event": r.get::<Value, _>("payload"),
            })
        })
        .collect();
    Ok(Json(json!({ "events": events })))
}

/// Permission requests live in memory while the engine's MCP call blocks on
/// them, so a dashboard refresh needs to re-fetch anything still pending.
async fn pending_permissions(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(id)).await?;
    let pending: Vec<Value> = state
        .permissions
        .pending_for_run(id)
        .into_iter()
        .map(|(request_id, tool_name, input)| {
            json!({ "requestId": request_id, "toolName": tool_name, "input": input })
        })
        .collect();
    Ok(Json(json!({ "pending": pending })))
}

/// The route: the caller's run, then the stop. `cancel_run` itself is also
/// called by research and the assistant's tools, which check on their own.
async fn cancel_run_route(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(id)).await?;
    cancel_run(State(state), Path(id)).await
}

/// Stop a run, whatever state it is in.
///
/// A run that is executing gets its step interrupted and its intent
/// recorded, so a multi-step workflow or organization stops rather than
/// rolling on to the next assignment. A run that is merely queued, or
/// parked waiting for plan approval, has no process to interrupt — it is
/// taken off the queue and closed out here instead. This used to answer
/// `{"canceled": true}` no matter what, including when it had done nothing.
pub(crate) async fn cancel_run(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let status: String = sqlx::query("SELECT status FROM runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such run".to_string()))?
        .get("status");

    if matches!(status.as_str(), "completed" | "failed" | "canceled") {
        return Ok(Json(json!({
            "canceled": false,
            "status": status,
            "detail": format!("this run already {status}"),
        })));
    }

    let interrupted = state.orchestrator.cancel(id);

    // Nothing was executing: close it out directly, or the run would sit
    // "queued" forever with a cancel nobody ever reads. Through `finish`, like
    // every other ending, so its steps settle the same way and an executor
    // that was mid-preparation finds the run ended and starts nothing.
    if !interrupted {
        state.orchestrator.cancel_idle(id).await.map_err(internal)?;
    }

    Ok(Json(json!({
        "canceled": true,
        "wasRunning": interrupted,
        "detail": if interrupted {
            "stopping — the current step is being interrupted"
        } else {
            "canceled before it started"
        },
    })))
}

#[derive(Deserialize)]
struct Resolve {
    allowed: bool,
}

async fn resolve_permission(
    State(state): State<AppState>,
    caller: Caller,
    Path(request_id): Path<String>,
    Json(body): Json<Resolve>,
) -> Result<Json<Value>, ApiError> {
    // The broker holds requests by id alone; whose run one belongs to is on
    // its row, written (`RunGate::record`) before anyone is told it exists.
    let run: Option<Uuid> =
        sqlx::query_scalar("SELECT run_id FROM permission_requests WHERE id = $1")
            .bind(&request_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
    match run {
        Some(run) => caller.require(&state, Owned::Run(run)).await?,
        None if !matches!(caller, Caller::Local) => {
            return Err((StatusCode::NOT_FOUND, "no such pending permission".into()))
        }
        None => {}
    }
    if state.permissions.resolve(&request_id, body.allowed) {
        Ok(Json(json!({ "resolved": true })))
    } else {
        Err((StatusCode::NOT_FOUND, "no such pending permission".into()))
    }
}

// ---------------------------------------------------------------------------
// Kanban: card movement, the comment thread, attaching files after creation.

/// `Default` so `chat_tools` can set one field and leave the rest alone —
/// the nested options already mean "absent", which is what a partial move
/// wants, and the field-by-field semantics are documented below.
#[derive(Deserialize, Debug, Default)]
pub(crate) struct MoveTask {
    board_column: Option<String>,
    position: Option<f64>,
    /// Who should do this card. Three distinct requests, which is why this is
    /// a nested option: the field absent means "leave the assignee alone",
    /// an explicit `null` means "unassign", and an id means "reassign". A
    /// plain `Option` collapses the first two, so `{"board_column":"done"}`
    /// would silently unassign the card.
    #[serde(default, deserialize_with = "present")]
    agent_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "present")]
    team_id: Option<Option<Uuid>>,
    /// How this job gets done. Nested for the same three states as the
    /// assignee — and it is not grouped with the reassignment refusal below,
    /// because a skill carries no memory: it only shapes the next run's
    /// prompt, which is the `plan_first` case rather than the agent one.
    #[serde(default, deserialize_with = "present")]
    skill_id: Option<Option<Uuid>>,
    /// Which CLI runs this card. Absent leaves it alone; a card always has
    /// one, so unlike the assignee there is no "clear it" case.
    #[serde(default)]
    engine: Option<String>,
    /// Absent leaves it alone.
    #[serde(default)]
    plan_first: Option<bool>,
    /// Absent leaves it alone. Cards always have a tier, so unlike the assignee
    /// there is no "clear it" case.
    #[serde(default)]
    model_tier: Option<TierChoice>,
    /// Nested, because all three states are meaningful: absent means "leave it",
    /// an explicit null means "go back to inheriting", and a value pins one.
    #[serde(default, deserialize_with = "present")]
    effort: Option<Option<ReasoningEffort>>,
    /// The card's brief. Absent leaves it alone; an empty string is refused
    /// rather than stored — a card with nothing to ask for cannot run.
    #[serde(default)]
    prompt: Option<String>,
    /// Start by itself once every card blocking it has landed. Absent leaves
    /// it alone.
    #[serde(default)]
    start_when_unblocked: Option<bool>,
    /// Dropping it into In Progress after seeing the forecast.
    #[serde(default)]
    acknowledge_forecast: bool,
    /// The goal it serves. Absent leaves it; null clears it.
    #[serde(default, deserialize_with = "present")]
    goal_id: Option<Option<Uuid>>,
    /// With a new agent on a card that is running: stop the running agent and
    /// hand the work over, this note being the new agent's brief.
    #[serde(default)]
    handoff_note: Option<String>,
}

impl MoveTask {
    /// File a card in a column and change nothing else.
    ///
    /// A named constructor rather than public fields: every other field here
    /// means "leave it alone" only because it is a nested option, and a caller
    /// building one by hand is one `..Default::default()` away from silently
    /// unassigning a card.
    pub(crate) fn to_column(column: &str) -> Self {
        Self {
            board_column: Some(column.to_string()),
            ..Default::default()
        }
    }

    /// Hand the card to an agent, leaving everything else alone.
    pub(crate) fn assign(agent_id: Uuid) -> Self {
        Self {
            agent_id: Some(Some(agent_id)),
            team_id: Some(None),
            ..Default::default()
        }
    }
}

/// Distinguish "field was present and null" from "field was absent".
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The route, and any caller acting for a person: the caller's card, then
/// the move. The agent, team, skill and goal it may name are held to the
/// card's own workspace inside `move_task`, which stays caller-free for the
/// assistant's tools — an MCP call carries no session, only its run.
pub(crate) async fn move_task_route(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<MoveTask>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    move_task(State(state), Path(id), Json(body)).await
}

/// Drag a card. Dropping a backlog card into "running" is the drag-native way
/// to start it; every other move is bookkeeping. A card whose run is still
/// active refuses to leave "running" — cancel the run first.
pub(crate) async fn move_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<MoveTask>,
) -> Result<Json<Value>, ApiError> {
    // Only the changes a live run would refuse; reordering a column is not
    // a reason to drop anything.
    if body.board_column.is_some()
        || body.agent_id.is_some()
        || body.team_id.is_some()
        || body.prompt.is_some()
    {
        state
            .orchestrator
            .supersede_summary(id)
            .await
            .map_err(internal)?;
    }
    let row = sqlx::query(
        "SELECT t.board_column, t.parent_id,
                (SELECT status FROM runs WHERE task_id = t.id
                 ORDER BY created_at DESC LIMIT 1) AS run_status
         FROM tasks t WHERE t.id=$1",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such task".to_string()))?;
    let current: String = row.get("board_column");
    // Two ways a card can be busy, and a sub-ticket is only ever the second.
    // Its work happens under a step in the *epic's* run, so it has no run of its
    // own and `run_status` says nothing about it — which used to leave every
    // guard below wide open for exactly the cards the system is writing to.
    let run_active = matches!(
        row.get::<Option<String>, _>("run_status").as_deref(),
        Some("queued" | "starting" | "running" | "waiting_permission" | "rate_limited")
    ) || step_is_live(&state, id).await?;

    if let Some(column) = &body.board_column {
        if !["backlog", "running", "review", "done"].contains(&column.as_str()) {
            return Err((StatusCode::BAD_REQUEST, format!("unknown column {column}")));
        }
        if run_active && column != "running" {
            return Err((
                StatusCode::CONFLICT,
                "the agent is still working on this card — cancel the run first".into(),
            ));
        }
    }

    // Changing hands mid-run would leave the running agent finishing work the
    // card no longer says is theirs, and the next run would start from a
    // different agent's memory. Cancel first.
    // Rewriting the brief mid-run would leave the agent working from words
    // the card no longer says — the next reader would blame the agent for
    // ignoring instructions it was never given. Same shape as reassignment.
    if body.prompt.is_some() && run_active {
        return Err((
            StatusCode::CONFLICT,
            "this card is being worked on — cancel the run before rewriting its description".into(),
        ));
    }
    if body.prompt.as_deref().is_some_and(|p| p.trim().is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "the description can't be empty".into(),
        ));
    }

    let reassigning = body.agent_id.is_some() || body.team_id.is_some();
    if reassigning && run_active {
        // To an agent, with a note: a handoff — the running agent is stopped
        // and the new one continues in the same worktree. Anything else still
        // waits for the run to be cancelled.
        if let (Some(note), Some(Some(to)), None | Some(None)) =
            (&body.handoff_note, body.agent_id, body.team_id)
        {
            require_same_workspace(&state, id, "agents", to).await?;
            eren_core::handoff::request(&state.orchestrator, id, to, note)
                .await
                .map_err(super::answer_refused)?;
            tokio::spawn(state.orchestrator.clone().settle_handoff_soon(id));
            return Ok(Json(json!({ "id": id, "handingOff": true })));
        }
        return Err((
            StatusCode::CONFLICT,
            "this card is being worked on — cancel the run before reassigning it, or hand it \
             over with a note"
                .into(),
        ));
    }

    // One level of hierarchy, deliberately. A sub-ticket handed to a team would
    // become an epic of its own: its grandchildren's worktrees would nest inside
    // its parent's, the epic's progress count would double-count it, and a
    // single goal could fan out without bound.
    // `.flatten()`, because the field is a nested option: an explicit null means
    // "take the team off", which is never the thing to refuse.
    if body.team_id.flatten().is_some() && row.get::<Option<Uuid>, _>("parent_id").is_some() {
        return Err((
            StatusCode::CONFLICT,
            "a sub-task can't be handed to a team — split the epic instead".into(),
        ));
    }

    // An agent and a team are alternatives, not a pair: `enqueue_task` hands
    // the whole card to the team when one is set, so leaving both would mean
    // the agent shown on the card never runs.
    let (agent_id, team_id) = match (body.agent_id, body.team_id) {
        (Some(Some(agent)), _) => (Some(Some(agent)), Some(None)),
        (_, Some(Some(team))) => (Some(None), Some(Some(team))),
        other => other,
    };

    if let Some(Some(agent_id)) = agent_id {
        require_same_workspace(&state, id, "agents", agent_id).await?;
        assignable(&state, agent_id).await?;
    }
    if let Some(Some(team_id)) = team_id {
        require_same_workspace(&state, id, "teams", team_id).await?;
    }
    if let Some(Some(skill_id)) = body.skill_id {
        require_skill_for_card(&state, id, skill_id).await?;
    }
    if let Some(Some(goal)) = body.goal_id {
        super::goals::vet_card_goal(&state, id, goal).await?;
    }

    // A move into "running" is a start, and the start must be vetted BEFORE
    // the column is written. Vet-after-update was tried and left a refused
    // card stranded in In Progress with no run behind it — the column said
    // working, the board said nothing was.
    let starting =
        body.board_column.as_deref() == Some("running") && current == "backlog" && !run_active;
    if starting {
        vet_task(&state, id).await?;
        // Every refusal the start could meet is met here, before the column
        // moves: a spent budget found by `enqueue_task` after the write would
        // leave the card In Progress with nothing running.
        let scope = eren_core::budgets::scope_of_task(&state.db, id)
            .await
            .map_err(internal)?;
        eren_core::budgets::check(&state.db, &scope, true)
            .await
            .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
        ask_about_cost(&state, id, body.acknowledge_forecast).await?;
    }

    sqlx::query(
        "UPDATE tasks SET board_column = coalesce($2, board_column),
                          position = coalesce($3, position),
                          agent_id = CASE WHEN $4 THEN $5 ELSE agent_id END,
                          team_id  = CASE WHEN $6 THEN $7 ELSE team_id  END,
                          engine   = coalesce($8, engine),
                          plan_first = coalesce($9, plan_first),
                          model_tier = coalesce($10, model_tier),
                          effort = CASE WHEN $11 THEN $12 ELSE effort END,
                          skill_id = CASE WHEN $13 THEN $14 ELSE skill_id END,
                          prompt = coalesce($15, prompt),
                          start_when_unblocked = coalesce($16, start_when_unblocked),
                          goal_id = CASE WHEN $17 THEN $18 ELSE goal_id END
         WHERE id = $1",
    )
    .bind(id)
    .bind(&body.board_column)
    .bind(body.position)
    .bind(agent_id.is_some())
    .bind(agent_id.flatten())
    .bind(team_id.is_some())
    .bind(team_id.flatten())
    .bind(body.engine.as_deref().filter(|e| !e.is_empty()))
    .bind(body.plan_first)
    .bind(body.model_tier.map(|t| t.as_str()))
    .bind(body.effort.is_some())
    .bind(body.effort.flatten().map(|e| e.as_str().to_string()))
    .bind(body.skill_id.is_some())
    .bind(body.skill_id.flatten())
    .bind(body.prompt.as_deref().map(str::trim))
    .bind(body.start_when_unblocked)
    .bind(body.goal_id.is_some())
    .bind(body.goal_id.flatten())
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;
    // A card handed to an agent that pulls its own work: it hears now.
    if let Some(Some(agent)) = agent_id {
        eren_core::heartbeat::wake(&state.db, agent, "assigned", Some(id)).await;
    }
    // Filing a card in done is how a person says its work landed by hand.
    if body.board_column.as_deref() == Some("done") {
        state.orchestrator.landed(id).await;
    }

    // Dropping into "running" from backlog means "go": start a run unless one
    // is already active or the task already did its work. The vet already
    // happened above, before the column changed.
    let mut run_id: Option<Uuid> = None;
    if starting {
        run_id = Some(
            state
                .orchestrator
                .enqueue_task(id)
                .await
                .map_err(start_refused)?,
        );
    }
    Ok(Json(json!({ "moved": true, "runId": run_id })))
}

/// Refuse a retired agent as an assignee. A paused one is fine — handing it
/// work for when it is resumed is half of what pausing is for.
async fn assignable(state: &AppState, agent_id: Uuid) -> Result<(), ApiError> {
    eren_core::agents::assert_assignable(&state.db, agent_id)
        .await
        .map_err(start_refused)
}

/// Refuse an assignee from another workspace.
///
/// Workspaces are the boundary the rest of the app is built around — the
/// agents list, the mention picker, the team roster are all scoped to one —
/// and a cross-workspace id would produce a card whose assignee is invisible
/// everywhere it should appear.
async fn require_same_workspace(
    state: &AppState,
    task_id: Uuid,
    table: &str,
    assignee_id: Uuid,
) -> Result<(), ApiError> {
    // `table` is a literal from the call sites, never user input.
    let ok: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT 1 FROM {table} x
         JOIN projects p ON p.workspace_id = x.workspace_id
         JOIN tasks t ON t.project_id = p.id
         WHERE t.id = $1 AND x.id = $2"
    ))
    .bind(task_id)
    .bind(assignee_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?;

    ok.map(|_| ()).ok_or((
        StatusCode::BAD_REQUEST,
        format!(
            "that {} is not in this card's workspace",
            table.trim_end_matches('s')
        ),
    ))
}

/// A skill a card may use: one its workspace can name — its own, or a
/// personal skill of the workspace's owner (`skill_in_workspace`).
async fn require_skill_for_card(
    state: &AppState,
    task_id: Uuid,
    skill_id: Uuid,
) -> Result<(), ApiError> {
    let ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM skills s, tasks t JOIN projects p ON p.id = t.project_id
          WHERE t.id = $1 AND s.id = $2
            AND skill_in_workspace(s.workspace_id, s.owner_id, p.workspace_id)",
    )
    .bind(task_id)
    .bind(skill_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?;
    ok.map(|_| ()).ok_or((
        StatusCode::BAD_REQUEST,
        "that skill is not in this card's workspace".into(),
    ))
}

/// Which agents does a comment speak to?
///
/// The same rule the chat composer uses — `mentions::mentioned`, which reads
/// `mention_cases.json` — rather than a second one. It was a bare substring
/// check, which has no notion of where a token starts or ends, so
/// `dev@Frontend.example` mentioned an agent called Frontend and this route
/// answers a mention by *starting a run*: a false positive here spends money.
/// An agent called `Front` matching inside `@Frontend` was the same bug facing
/// the other way.
fn mentioned_agents(content: &str, agents: &[(Uuid, String)]) -> Vec<Uuid> {
    let names: Vec<String> = agents.iter().map(|(_, name)| name.clone()).collect();
    mentions::mentioned(content, &names)
        .into_iter()
        .filter_map(|name| agents.iter().find(|(_, n)| *n == name).map(|(id, _)| *id))
        .collect()
}

async fn comments(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let rows = sqlx::query(
        "SELECT c.id, c.author, c.agent_id, c.content, c.run_id, c.created_at,
                c.file_path, c.line, c.hunk,
                a.name AS agent_name, a.color AS agent_color,
                r.status AS run_status
         FROM task_comments c
         LEFT JOIN agents a ON a.id = c.agent_id
         LEFT JOIN runs r ON r.id = c.run_id
         WHERE c.task_id=$1 ORDER BY c.created_at",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    // Replies still being written show as typing indicators, not comments.
    let pending: i64 = sqlx::query(
        "SELECT count(*) AS n FROM runs
         WHERE comment_id IN (SELECT id FROM task_comments WHERE task_id=$1)
           AND status NOT IN ('completed','failed','canceled')",
    )
    .bind(task_id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?
    .get("n");

    Ok(Json(json!({
        "comments": rows.iter().map(|r| json!({
            "id": r.get::<Uuid, _>("id"),
            "author": r.get::<String, _>("author"),
            "agentId": r.get::<Option<Uuid>, _>("agent_id"),
            "agentName": r.get::<Option<String>, _>("agent_name"),
            "agentColor": r.get::<Option<String>, _>("agent_color"),
            "content": r.get::<String, _>("content"),
            "runId": r.get::<Option<Uuid>, _>("run_id"),
            "filePath": r.get::<Option<String>, _>("file_path"),
            "line": r.get::<Option<i32>, _>("line"),
            "hunk": r.get::<Option<String>, _>("hunk"),
            "ts": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        })).collect::<Vec<_>>(),
        "pendingReplies": pending,
    })))
}

#[derive(Deserialize)]
struct PostComment {
    content: String,
    /// Engine id from `/api/engines`. Omitted means the machine default.
    engine: Option<String>,
    /// Anchor to a line of the diff. Present when the comment was written
    /// from the diff view rather than the card.
    file_path: Option<String>,
    line: Option<i32>,
    /// The hunk as it looked when the note was written — snapshotted because
    /// the fix run changes the very diff the line number refers to.
    hunk: Option<String>,
    /// Act on it, rather than just record it. Spawns a scoped run in the
    /// task's existing worktree.
    fix: Option<bool>,
    /// Knowledge-base articles referenced by this comment alone. Scoped to the
    /// comment rather than pinned to the card: "see #runbook" is context for
    /// this reply, not a permanent property of the work.
    #[serde(default)]
    article_ids: Vec<Uuid>,
}

async fn post_comment(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
    Json(body): Json<PostComment>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    // An article's text goes into the reply's prompt, so naming one is a read.
    for article in &body.article_ids {
        caller.require(&state, Owned::KbArticle(*article)).await?;
    }
    let content = body.content.trim();
    if content.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "comment is empty".into()));
    }
    // The task's workspace bounds who can be mentioned.
    let agents: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT a.id, a.name FROM agents a
         JOIN projects p ON p.workspace_id = a.workspace_id
         JOIN tasks t ON t.project_id = p.id
         WHERE t.id = $1 AND a.status <> 'retired'",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    if agents.is_empty() {
        // Distinguish "no such task" from "no agents to mention".
        let exists = sqlx::query("SELECT 1 AS ok FROM tasks WHERE id=$1")
            .bind(task_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
        if exists.is_none() {
            return Err((StatusCode::NOT_FOUND, "no such task".into()));
        }
    }

    let comment_id: Uuid = sqlx::query(
        "INSERT INTO task_comments (task_id, author, content, file_path, line, hunk)
         VALUES ($1,'user',$2,$3,$4,$5) RETURNING id",
    )
    .bind(task_id)
    .bind(content)
    .bind(
        body.file_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty()),
    )
    .bind(body.line)
    .bind(body.hunk.as_deref())
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)?
    .get("id");

    if !body.article_ids.is_empty() {
        sqlx::query(
            "INSERT INTO comment_articles (comment_id, article_id)
             SELECT $1, unnest($2::uuid[]) ON CONFLICT DO NOTHING",
        )
        .bind(comment_id)
        .bind(&body.article_ids)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    }

    // "Fix this" is its own path: a comment reply is read-only by design, so
    // acting on review feedback needs a run that can actually edit, in the
    // worktree the diff came from.
    if body.fix.unwrap_or(false) {
        // The note is kept either way; a refusal says why nothing acted on it.
        let run_id = state
            .orchestrator
            .enqueue_follow_up(task_id, FollowUp::ReviewNote { comment_id })
            .await
            .map_err(start_refused)?;
        return Ok(Json(
            json!({ "id": comment_id, "runIds": [run_id], "fixRunId": run_id }),
        ));
    }

    // Every mentioned agent replies, capped so one comment can't fan out a
    // whole roster of runs.
    let default_engine = state.orchestrator.default_engine();
    let engine = body.engine.as_deref().unwrap_or(&default_engine);
    let mut run_ids: Vec<Uuid> = vec![];
    for agent_id in mentioned_agents(content, &agents).into_iter().take(3) {
        match state
            .orchestrator
            .enqueue_comment_reply(comment_id, agent_id, engine)
            .await
        {
            Ok(run_id) => run_ids.push(run_id),
            // The comment is posted and the other agents still answer; the
            // thread says why this one does not.
            Err(e)
                if e.is::<eren_core::agents::Unavailable>()
                    || e.is::<eren_core::budgets::OverBudget>() =>
            {
                eren_core::runs::report::post_system(&state.db, task_id, None, &e.to_string())
                    .await
                    .map_err(internal)?;
            }
            Err(e) => return Err(start_refused(e)),
        }
    }
    Ok(Json(json!({ "id": comment_id, "runIds": run_ids })))
}

#[derive(Deserialize)]
struct BakeoffBody {
    variants: Vec<VariantBody>,
}

#[derive(Deserialize)]
struct VariantBody {
    label: String,
    agent_id: Option<Uuid>,
    /// "easy" | "medium" | "complex". Absent means the agent's own tier.
    tier: Option<String>,
    /// Absent means the card's engine.
    engine: Option<String>,
}

/// Run the same brief several ways at once.
async fn start_bakeoff(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
    Json(body): Json<BakeoffBody>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    for v in &body.variants {
        caller.require_opt(&state, v.agent_id, Owned::Agent).await?;
    }
    let variants: Vec<Variant> = body
        .variants
        .into_iter()
        .map(|v| Variant {
            label: v.label,
            agent_id: v.agent_id,
            tier: v.tier,
            engine: v.engine,
        })
        .collect();

    let run_ids = state
        .orchestrator
        .enqueue_bakeoff(task_id, &variants)
        .await
        .map_err(|e| match e.is::<eren_core::agents::Unavailable>() {
            true => (StatusCode::CONFLICT, e.to_string()),
            false => (StatusCode::BAD_REQUEST, e.to_string()),
        })?;
    Ok(Json(json!({ "runIds": run_ids })))
}

/// The variants of a task, with their diffs, so they can be read side by side.
///
/// The diff is the comparison — cost and duration matter, but nobody picks a
/// winner on a number. Each is fetched from that variant's own worktree.
async fn bakeoff(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let rows = sqlx::query(
        "SELECT r.id, r.variant_label, r.status, r.cost_usd, r.model, r.worktree_path, r.engine,
                r.started_at, r.finished_at, r.error_reason,
                a.name AS agent_name, p.default_branch
         FROM runs r
         JOIN tasks t ON t.id = r.task_id
         JOIN projects p ON p.id = t.project_id
         LEFT JOIN agents a ON a.id = r.agent_id
         WHERE r.task_id = $1 AND r.variant_label IS NOT NULL
         ORDER BY r.created_at",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    let mut variants = vec![];
    for r in &rows {
        let diff = match r.get::<Option<String>, _>("worktree_path") {
            Some(path) => state
                .orchestrator
                .worktrees
                .diff(
                    std::path::Path::new(&path),
                    &r.get::<String, _>("default_branch"),
                )
                .await
                .unwrap_or_default(),
            None => String::new(),
        };
        let started = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at");
        let finished = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at");
        variants.push(json!({
            "runId": r.get::<Uuid, _>("id"),
            "label": r.get::<String, _>("variant_label"),
            "status": r.get::<String, _>("status"),
            "agentName": r.get::<Option<String>, _>("agent_name"),
            "model": r.get::<Option<String>, _>("model"),
            "engine": r.get::<String, _>("engine"),
            "costUsd": r.get::<Option<f64>, _>("cost_usd"),
            "error": r.get::<Option<String>, _>("error_reason"),
            "seconds": match (started, finished) {
                (Some(a), Some(b)) => Some((b - a).num_seconds()),
                _ => None,
            },
            // Cheap, comparable signal to sit beside the diff itself.
            "linesChanged": diff
                .lines()
                .filter(|l| (l.starts_with('+') || l.starts_with('-'))
                    && !l.starts_with("+++") && !l.starts_with("---"))
                .count(),
            "diff": diff,
        }));
    }
    Ok(Json(json!({ "variants": variants })))
}

/// A refused merge as a JSON 409 the dashboard can act on: `kind` says which
/// refusal, `files` which files, and `error` keeps the sentence it always had,
/// so a client that only reads the text still reads the same words.
fn merge_refused(e: anyhow::Error) -> ApiError {
    use eren_core::worktrees::manager::MergeRefusal;
    match e.downcast_ref::<MergeRefusal>() {
        Some(r) => (
            StatusCode::CONFLICT,
            json!({ "kind": r.kind(), "error": r.to_string(), "files": r.files() }).to_string(),
        ),
        None => (StatusCode::CONFLICT, e.to_string()),
    }
}

/// The card's worktree and branch, or the reason there are none.
async fn card_worktree(
    state: &AppState,
    id: Uuid,
) -> Result<(eren_core::worktrees::manager::Worktree, String, String), ApiError> {
    let row = sqlx::query(
        "SELECT t.title, t.worktree_path, t.branch, p.default_branch
           FROM tasks t JOIN projects p ON p.id = t.project_id WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such task".to_string()))?;
    let (Some(path), Some(branch)): (Option<String>, Option<String>) =
        (row.get("worktree_path"), row.get("branch"))
    else {
        return Err((StatusCode::CONFLICT, "this card has no worktree".into()));
    };
    if !std::path::Path::new(&path).is_dir() {
        return Err((
            StatusCode::CONFLICT,
            "this card's worktree is gone from disk".into(),
        ));
    }
    Ok((
        eren_core::worktrees::manager::Worktree {
            path: path.into(),
            branch,
        },
        row.get("default_branch"),
        row.get("title"),
    ))
}

/// How the card's branch stands against the base: how far behind, and
/// whether a merge of the base is waiting to be resolved in it.
async fn base_status(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let Ok((wt, base, _)) = card_worktree(&state, id).await else {
        return Ok(Json(json!({ "behind": null, "merging": null })));
    };
    let worktrees = &state.orchestrator.worktrees;
    Ok(Json(json!({
        "base": base,
        "behind": worktrees.behind_base(&wt.path, &base).await.ok(),
        "merging": worktrees.merge_in_progress(&wt.path).await,
    })))
}

/// Bring the base into the card's branch; on conflict, have an agent resolve
/// it in the worktree. The way out of a merge that was refused as a conflict.
async fn update_from_base(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    state
        .orchestrator
        .supersede_summary(id)
        .await
        .map_err(internal)?;
    if any_run_is_live(&state, id).await? || step_is_live(&state, id).await? {
        return Err((
            StatusCode::CONFLICT,
            "an agent is still working on this card — wait for it to finish first".into(),
        ));
    }
    let (wt, base, title) = card_worktree(&state, id).await?;
    let update = state
        .orchestrator
        .worktrees
        .update_from_base(&wt, &base, &format!("eren: {title}"))
        .await
        .map_err(merge_refused)?;
    use eren_core::worktrees::manager::BaseUpdate;
    Ok(Json(match update {
        BaseUpdate::UpToDate => json!({ "outcome": "up_to_date" }),
        BaseUpdate::Merged => json!({ "outcome": "merged" }),
        BaseUpdate::Conflicted { files } => {
            let run_id = state
                .orchestrator
                .enqueue_follow_up(
                    id,
                    FollowUp::MergeConflict {
                        files: files.clone(),
                        base,
                    },
                )
                .await
                .map_err(start_refused)?;
            json!({ "outcome": "conflicted", "files": files, "runId": run_id })
        }
    }))
}

/// Every run of a card, newest first: the history behind the one run the
/// board shows.
///
/// Everything here was already stored — tokens, durations, sessions, what a
/// run resumed — and only the newest run's status and dollars ever reached a
/// screen. Earlier attempts, and the reasons they failed, were invisible.
async fn task_runs(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let rows = sqlx::query(
        "SELECT r.id, r.trigger, r.status, r.engine, r.model, r.tier_resolved,
                r.cost_usd, r.input_tokens, r.output_tokens, r.cache_read_tokens,
                r.cache_creation_tokens, r.tokens_provisional, r.created_at,
                r.started_at, r.finished_at, r.error_reason, r.session_id,
                r.session_engine, r.resumed_from, r.rate_limit_attempts,
                r.variant_label, r.review_comment_id, r.plan_approval,
                COALESCE(r.worktree_path, t.worktree_path) AS worktree,
                p.path AS project_path, p.vcs, a.name AS agent_name,
                -- What the run said it did, as posted on this card: the
                -- newest of its comments, because the report is written when
                -- the run ends — after any note it left along the way.
                (SELECT left(c.content, 600) FROM task_comments c
                  WHERE c.run_id = r.id AND c.task_id = r.task_id AND c.author = 'agent'
                  ORDER BY c.created_at DESC LIMIT 1) AS report
           FROM runs r
           JOIN tasks t ON t.id = r.task_id
           JOIN projects p ON p.id = t.project_id
           LEFT JOIN agents a ON a.id = r.agent_id
          WHERE r.task_id = $1
          ORDER BY r.created_at DESC",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;

    // A terminal command is offered only while nothing is running on the card:
    // a person resuming a session the board is also driving would be two
    // writers in one worktree, the thing the rest of Eren refuses.
    let quiet = !any_run_is_live(&state, task_id).await?;

    let runs: Vec<Value> = rows
        .iter()
        .map(|r| {
            let started = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("started_at");
            let finished = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at");
            let resume_command = quiet.then(|| resume_command(&state, r)).flatten();
            json!({
                "runId": r.get::<Uuid, _>("id"),
                "trigger": r.get::<String, _>("trigger"),
                "status": r.get::<String, _>("status"),
                "engine": r.get::<String, _>("engine"),
                "model": r.get::<Option<String>, _>("model"),
                "tierResolved": r.get::<Option<String>, _>("tier_resolved"),
                "agentName": r.get::<Option<String>, _>("agent_name"),
                "variantLabel": r.get::<Option<String>, _>("variant_label"),
                "planFirst": r.get::<bool, _>("plan_approval"),
                "costUsd": r.get::<Option<f64>, _>("cost_usd"),
                "inputTokens": r.get::<i64, _>("input_tokens"),
                "outputTokens": r.get::<i64, _>("output_tokens"),
                "cacheReadTokens": r.get::<i64, _>("cache_read_tokens"),
                "cacheCreationTokens": r.get::<i64, _>("cache_creation_tokens"),
                "tokensProvisional": r.get::<bool, _>("tokens_provisional"),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                "startedAt": started,
                "finishedAt": finished,
                "seconds": match (started, finished) {
                    (Some(a), Some(b)) => Some((b - a).num_seconds()),
                    _ => None,
                },
                "error": r.get::<Option<String>, _>("error_reason"),
                "sessionId": r.get::<Option<String>, _>("session_id"),
                "resumedFrom": r.get::<Option<Uuid>, _>("resumed_from"),
                "rateLimitAttempts": r.get::<i32, _>("rate_limit_attempts"),
                "reviewCommentId": r.get::<Option<Uuid>, _>("review_comment_id"),
                "resumeCommand": resume_command,
                "report": r.get::<Option<String>, _>("report"),
            })
        })
        .collect();
    Ok(Json(json!({ "runs": runs })))
}

/// `cd <dir> && <engine's resume argv>`, when there is a session, an engine
/// that says how, and a directory still on disk to run it in.
///
/// The directory matters as much as the id: a CLI finds its sessions per
/// working directory, so the command has to start where the run did — the
/// run's worktree, or the project itself when it edits in place.
fn resume_command(state: &AppState, r: &sqlx::postgres::PgRow) -> Option<String> {
    let session = r.get::<Option<String>, _>("session_id")?;
    let engine_id = r.get::<Option<String>, _>("session_engine")?;
    let argv = state
        .orchestrator
        .engine(&engine_id)?
        .interactive_resume_argv(&session)?;
    let dir = if r.get::<String, _>("vcs") == "git" {
        r.get::<Option<String>, _>("worktree")?
    } else {
        r.get::<String, _>("project_path")
    };
    std::path::Path::new(&dir)
        .is_dir()
        .then(|| shell_line(&dir, &argv))
}

/// One line a POSIX shell runs as written: every word single-quoted.
fn shell_line(dir: &str, argv: &[String]) -> String {
    let quote = |w: &str| format!("'{}'", w.replace('\'', "'\\''"));
    let command: Vec<String> = argv.iter().map(|w| quote(w)).collect();
    format!("cd {} && {}", quote(dir), command.join(" "))
}

/// Adopt a variant's work as the task's and discard the rest.
async fn keep_variant(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    state
        .orchestrator
        .keep_variant(run_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({ "kept": run_id })))
}

#[derive(Deserialize)]
struct AttachToTask {
    attachment_ids: Vec<Uuid>,
}

/// Bind already-uploaded files to an existing card — the drawer's attach
/// button. The next run of the task will see them.
async fn attach_to_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(task_id): Path<Uuid>,
    Json(body): Json<AttachToTask>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let row = sqlx::query("SELECT project_id FROM tasks WHERE id=$1")
        .bind(task_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such task".to_string()))?;
    attachments::claim(
        &state.db,
        &body.attachment_ids,
        attachments::Home::Project(row.get("project_id")),
        attachments::Owner::Task(task_id),
    )
    .await?;
    Ok(Json(json!({ "attached": body.attachment_ids.len() })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddBlocker {
    blocked_by: Uuid,
}

/// Declare that this card cannot start until another card lands. The rules
/// are `landing::add_blocker`'s, shared with an agent's `report_blocker`.
async fn add_blocker(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<AddBlocker>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    use eren_core::landing::{add_blocker, BlockerRefusal};
    // The blocker needs no check of its own: `add_blocker` refuses one from
    // another board.
    match add_blocker(&state.db, id, body.blocked_by).await {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(e) => match e.downcast_ref::<BlockerRefusal>() {
            Some(BlockerRefusal::Cycle) => Err((StatusCode::CONFLICT, e.to_string())),
            Some(_) => Err((StatusCode::BAD_REQUEST, e.to_string())),
            None => Err(internal(e)),
        },
    }
}

async fn remove_blocker(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, blocker_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    // The blocker needs no check: the delete is of this card's own rows.
    caller.require(&state, Owned::Task(id)).await?;
    sqlx::query("DELETE FROM task_deps WHERE task_id = $1 AND blocked_by = $2")
        .bind(id)
        .bind(blocker_id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::{mentioned_agents, merge_body, shell_line, MoveTask};
    use axum::http::StatusCode;
    use uuid::Uuid;

    #[test]
    fn an_empty_merge_body_is_the_ordinary_merge() {
        // What the dashboard's Merge sends: nothing, under a JSON content type.
        let plain = merge_body(b"").unwrap();
        assert!(!plain.force && plain.note.is_empty());
        assert!(!merge_body(b"  \n").unwrap().force);
        let forced = merge_body(br#"{"force":true,"note":"why"}"#).unwrap();
        assert!(forced.force && forced.note == "why");
        assert_eq!(merge_body(b"{nope").unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    /// The resume command is pasted into a shell, so a worktree path with a
    /// space — or a quote — must arrive as one word, not as two commands.
    #[test]
    fn a_resume_command_survives_awkward_paths() {
        let argv = [
            "claude".to_string(),
            "--resume".to_string(),
            "abc-123".to_string(),
        ];
        assert_eq!(
            shell_line("/home/me/my repo", &argv),
            "cd '/home/me/my repo' && 'claude' '--resume' 'abc-123'"
        );
        assert_eq!(
            shell_line("/tmp/it's", &argv[..1]),
            "cd '/tmp/it'\\''s' && 'claude'"
        );
    }

    /// The distinction the whole reassignment feature rests on. If an absent
    /// field deserialized the same as an explicit null, then dragging a card
    /// between columns — which sends only `board_column` — would quietly
    /// unassign it.
    #[test]
    fn an_absent_assignee_is_not_the_same_as_a_null_one() {
        let drag: MoveTask = serde_json::from_str(r#"{"board_column":"done"}"#).unwrap();
        assert_eq!(drag.agent_id, None, "absent must mean leave it alone");
        assert_eq!(drag.team_id, None);

        let unassign: MoveTask = serde_json::from_str(r#"{"agent_id":null}"#).unwrap();
        assert_eq!(unassign.agent_id, Some(None), "null must mean clear it");

        let id = Uuid::new_v4();
        let reassign: MoveTask =
            serde_json::from_str(&format!(r#"{{"agent_id":"{id}"}}"#)).unwrap();
        assert_eq!(reassign.agent_id, Some(Some(id)));
    }

    /// The client always sends both ids, so "give it to this team" arrives as
    /// a team id plus a null agent — and must not be read as ambiguous.
    #[test]
    fn handing_a_card_to_a_team_clears_the_agent_in_the_same_request() {
        let team = Uuid::new_v4();
        let body: MoveTask =
            serde_json::from_str(&format!(r#"{{"agent_id":null,"team_id":"{team}"}}"#)).unwrap();
        assert_eq!(body.agent_id, Some(None));
        assert_eq!(body.team_id, Some(Some(team)));
    }

    #[test]
    fn an_empty_patch_touches_nothing() {
        let body: MoveTask = serde_json::from_str("{}").unwrap();
        assert!(body.board_column.is_none());
        assert!(body.position.is_none());
        assert_eq!(body.agent_id, None);
        assert_eq!(body.team_id, None);
    }

    #[test]
    fn mentions_match_case_insensitively_and_allow_spaces_in_names() {
        let rex = Uuid::new_v4();
        let ada = Uuid::new_v4();
        let agents = vec![(rex, "Rex".to_string()), (ada, "Ada Lovelace".to_string())];
        assert_eq!(
            mentioned_agents("hey @rex, look at this", &agents),
            vec![rex]
        );
        assert_eq!(
            mentioned_agents("@Ada Lovelace what do you think?", &agents),
            vec![ada]
        );
        assert_eq!(
            mentioned_agents("@rex and @ada lovelace both", &agents),
            vec![rex, ada]
        );
        assert!(mentioned_agents("mail me at rex@example.com", &agents).is_empty());
        assert!(mentioned_agents("no mentions here", &agents).is_empty());
    }

    #[test]
    fn an_address_that_ends_in_an_agents_name_does_not_start_a_run() {
        // The old substring check said yes to this, and answering a mention
        // here dispatches a paid run.
        let rex = Uuid::new_v4();
        let agents = vec![(rex, "Rex".to_string())];
        assert!(mentioned_agents("mail me at dev@Rex.example", &agents).is_empty());
        assert!(mentioned_agents("ship it (@Rex)", &agents).is_empty());
    }

    #[test]
    fn a_shorter_name_does_not_claim_half_of_a_longer_word() {
        let front = Uuid::new_v4();
        let agents = vec![(front, "Front".to_string())];
        assert!(mentioned_agents("@Frontend please", &agents).is_empty());
        assert_eq!(mentioned_agents("@Front please", &agents), vec![front]);
    }
}

// ---------------------------------------------------------------------------
// Deleting and retrying a card.

/// True while an org run owns this card through a live assignment.
///
/// The companion to `run_is_active`, and necessary because a sub-ticket has no
/// run of its own: an epic's work happens under steps of the *epic's* run. Every
/// "is this card busy" check needs both, or the one class of card the system
/// writes to is the one class nothing protects.
async fn step_is_live(state: &AppState, task_id: Uuid) -> Result<bool, ApiError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM steps s JOIN runs r ON r.id = s.run_id
              WHERE s.task_id = $1
                AND s.status IN ('queued','starting','running','waiting_permission','rate_limited')
                AND r.status NOT IN ('completed','failed','canceled'))",
    )
    .bind(task_id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)
}

/// True when the task's latest run is still live.
/// A refusal to start work on a card, as the status it means: a card that is
/// already running, or a follow-up with nothing to follow up on, is a conflict
/// the person can act on, not a server error.
fn start_refused(e: anyhow::Error) -> ApiError {
    super::run_refused(e)
}

/// Is *any* run of this card still live — not just the newest one?
///
/// `run_is_active` reads the latest run only, which is the right question for
/// "what is this card doing"; it is the wrong one before touching the worktree,
/// where an older run still writing there matters just as much.
async fn any_run_is_live(state: &AppState, task_id: Uuid) -> Result<bool, ApiError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE task_id = $1
                           AND status NOT IN ('completed','failed','canceled'))",
    )
    .bind(task_id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(internal)
}

async fn run_is_active(state: &AppState, task_id: Uuid) -> Result<bool, ApiError> {
    let row =
        sqlx::query("SELECT status FROM runs WHERE task_id=$1 ORDER BY created_at DESC LIMIT 1")
            .bind(task_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(internal)?;
    Ok(matches!(
        row.map(|r| r.get::<String, _>("status")).as_deref(),
        Some("queued" | "starting" | "running" | "waiting_permission" | "rate_limited")
    ))
}

/// Drop a task's worktree and its branch, and forget them on the row.
///
/// This is the only production caller of `WorktreeManager::remove` — without
/// it, every task ever run leaves a worktree and an `eren/*` branch behind
/// forever.
async fn drop_worktree(state: &AppState, task_id: Uuid) -> Result<(), ApiError> {
    let row = sqlx::query(
        "SELECT t.worktree_path, t.branch, p.path AS project_path
         FROM tasks t JOIN projects p ON p.id = t.project_id WHERE t.id=$1",
    )
    .bind(task_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such task".to_string()))?;

    if let (Some(path), Some(branch)) = (
        row.get::<Option<String>, _>("worktree_path"),
        row.get::<Option<String>, _>("branch"),
    ) {
        // An epic and its sub-tickets share one checkout, so removing it because
        // one of them is being deleted would pull the ground out from under the
        // others. Forget it on this row and leave the directory alone.
        let shared: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE worktree_path = $1 AND id <> $2)",
        )
        .bind(&path)
        .bind(task_id)
        .fetch_one(&state.db.pool)
        .await
        .map_err(internal)?;
        if shared {
            sqlx::query("UPDATE tasks SET worktree_path=NULL, branch=NULL WHERE id=$1")
                .bind(task_id)
                .execute(&state.db.pool)
                .await
                .map_err(internal)?;
            return Ok(());
        }

        let wt = eren_core::worktrees::manager::Worktree {
            path: path.into(),
            branch,
        };
        // Best effort: a worktree the user already deleted by hand must not
        // block deleting the card.
        if let Err(e) = state
            .orchestrator
            .worktrees
            .remove(
                std::path::Path::new(&row.get::<String, _>("project_path")),
                &wt,
            )
            .await
        {
            tracing::warn!(%task_id, error = %e, "could not remove worktree");
        }
        sqlx::query("UPDATE tasks SET worktree_path=NULL, branch=NULL WHERE id=$1")
            .bind(task_id)
            .execute(&state.db.pool)
            .await
            .map_err(internal)?;
    }
    Ok(())
}

/// Delete a card: its comments, runs, and attachment rows go with it (FK
/// cascade), its worktree and branch are removed, and the attachment bytes are
/// reclaimed by the sweeper. The agent's memory of the work survives —
/// `agent_memories.task_id` is SET NULL, because what an agent learned
/// shouldn't vanish when a card is tidied away.
async fn delete_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    state
        .orchestrator
        .supersede_summary(id)
        .await
        .map_err(internal)?;
    if run_is_active(&state, id).await? || step_is_live(&state, id).await? {
        return Err((
            StatusCode::CONFLICT,
            "the agent is still working on this card — cancel the run first".into(),
        ));
    }
    // Sub-tickets share the epic's checkout, so they must stop pointing at it
    // before it is removed. Without this, deleting an epic leaves a column of
    // cards whose "diff" is a directory that no longer exists.
    //
    // The rows themselves survive: `parent_id` is ON DELETE SET NULL, because a
    // sub-ticket is real work with its own comments and history, and tidying the
    // epic away should not take it with them.
    sqlx::query("UPDATE tasks SET worktree_path=NULL, branch=NULL WHERE parent_id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    // Before the row goes: the preview row cascades away with the task, and a
    // cascaded row is one nothing will ever look for again — the container
    // would sit there holding its port until the next restart swept it.
    if let Err(e) = eren_core::previews::stop(&state.db, id).await {
        tracing::warn!(task=%id, error=%e, "could not stop this card's preview before deleting it");
    }
    drop_worktree(&state, id).await?;
    let done = sqlx::query("DELETE FROM tasks WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "no such task".into()));
    }
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct Retry {
    /// Start from a clean checkout (default). False continues in the existing
    /// worktree, keeping whatever the previous attempt left behind.
    #[serde(default = "yes")]
    fresh: bool,
}

fn yes() -> bool {
    true
}

/// Run a card again.
///
/// A fresh retry throws away the previous attempt's worktree and branch, so
/// the agent starts from the base branch rather than silently inheriting its
/// own half-finished work. That discards an unmerged diff, which is the point
/// of retrying — but it is destructive, so the UI confirms it for cards
/// sitting in review.
async fn retry(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<Retry>>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    state
        .orchestrator
        .supersede_summary(id)
        .await
        .map_err(internal)?;
    if run_is_active(&state, id).await? || step_is_live(&state, id).await? {
        return Err((
            StatusCode::CONFLICT,
            "this card is already running — cancel it before retrying".into(),
        ));
    }
    let fresh = body.map(|Json(b)| b.fresh).unwrap_or(true);
    if fresh {
        drop_worktree(&state, id).await?;
    }
    let run_id = state
        .orchestrator
        .enqueue_task(id)
        .await
        .map_err(start_refused)?;
    sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
        .bind(id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "runId": run_id, "fresh": fresh })))
}

/// Pick a dead run back up where it stopped.
///
/// The counterpart to Retry, and the opposite trade: Retry throws the worktree
/// away and starts from the base branch, Resume keeps both the worktree and
/// the engine's own session so the agent can see what it already did.
///
/// Every refusal is a 409 carrying `Refusal::message()`, which the drawer
/// shows in its error banner. Refusing rather than quietly starting over is
/// the same rule CLAUDE.md states for OpenCode and `Reviewed`: a button that
/// silently does a different, more expensive thing is worse than one that
/// says why it can't.
pub(crate) async fn resume_run(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    let (new_run, _) = eren_core::runs::resume::resume_dead_run(&state.orchestrator, run_id)
        .await
        .map_err(super::answer_refused)?;
    Ok(Json(json!({ "runId": new_run, "resumedFrom": run_id })))
}

// ── Plan-first cards ────────────────────────────────────────────────────────
//
// A card can ask the agent to write down what it means to do before it does
// it. The run parks at `awaiting_approval` with the plan stored as a step, and
// these routes are how a person confirms it, rewrites it, or sends it back.
//
// Everything here refuses on a run that isn't parked. A plan being edited
// while work is already underway would describe a decision nobody gets to
// make, which is worse than no plan at all.

/// Only a parked run's plan is editable.
async fn assert_parked(state: &AppState, run_id: Uuid) -> Result<(), ApiError> {
    let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE id=$1")
        .bind(run_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such run".to_string()))?;
    if status != "awaiting_approval" {
        return Err((
            StatusCode::CONFLICT,
            format!("this run is {status}, so its plan is no longer editable"),
        ));
    }
    Ok(())
}

async fn plan(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    let row = sqlx::query(
        "SELECT r.status, r.plan_edited, s.output_text, s.finished_at
         FROM runs r
         LEFT JOIN steps s ON s.run_id = r.id AND s.step_key = 'plan'
         WHERE r.id = $1",
    )
    .bind(run_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such run".to_string()))?;

    let status: String = row.get("status");
    Ok(Json(json!({
        "runId": run_id,
        "content": row.get::<Option<String>, _>("output_text"),
        // Only while parked can it be answered; afterwards it's a record of
        // what was agreed, which is still worth showing.
        "awaitingApproval": status == "awaiting_approval",
        "edited": row.get::<bool, _>("plan_edited"),
        "writtenAt": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at"),
    })))
}

#[derive(Deserialize)]
struct PlanEdit {
    content: String,
}

/// Rewrite the plan by hand. The work pass is told the text was edited, so it
/// follows what's in front of it rather than what it remembers proposing.
async fn edit_plan(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
    Json(body): Json<PlanEdit>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    assert_parked(&state, run_id).await?;
    if body.content.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "an empty plan approves nothing — delete the card instead".into(),
        ));
    }
    let updated =
        sqlx::query("UPDATE steps SET output_text = $2 WHERE run_id = $1 AND step_key = 'plan'")
            .bind(run_id)
            .bind(body.content.trim())
            .execute(&state.db.pool)
            .await
            .map_err(internal)?;
    if updated.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "this run has no plan".into()));
    }
    sqlx::query("UPDATE runs SET plan_edited = TRUE WHERE id = $1")
        .bind(run_id)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "saved": true })))
}

/// Start the work, from whatever the plan says now.
async fn approve_plan(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    eren_core::approvals::approve_task_plan(&state.orchestrator, run_id)
        .await
        .map_err(super::answer_refused)?;
    Ok(Json(json!({ "approved": true })))
}

#[derive(Deserialize)]
struct Revise {
    note: String,
}

/// Send the plan back for another pass, saying what was wrong with it.
async fn revise_plan(
    State(state): State<AppState>,
    caller: Caller,
    Path(run_id): Path<Uuid>,
    Json(body): Json<Revise>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Run(run_id)).await?;
    eren_core::approvals::revise_task_plan(&state.orchestrator, run_id, &body.note)
        .await
        .map_err(super::answer_refused)?;
    Ok(Json(json!({ "revising": true })))
}

// ── Knowledge-base articles on a card ───────────────────────────────────────

async fn link_articles(state: &AppState, task_id: Uuid, ids: &[Uuid]) -> Result<(), ApiError> {
    // A tagged page's full text is injected into the run's prompt, so tagging
    // is a read grant. Without this, a page from another workspace could be
    // attached to this card and handed to an agent working in it.
    for id in ids {
        require_same_workspace(state, task_id, "kb_articles", *id).await?;
    }
    sqlx::query("DELETE FROM task_articles WHERE task_id = $1 AND NOT (article_id = ANY($2))")
        .bind(task_id)
        .bind(ids)
        .execute(&state.db.pool)
        .await
        .map_err(internal)?;
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO task_articles (task_id, article_id)
         SELECT $1, unnest($2::uuid[]) ON CONFLICT DO NOTHING",
    )
    .bind(task_id)
    .bind(ids)
    .execute(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(())
}

async fn task_articles(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    let rows = sqlx::query(
        "SELECT a.id, a.title, a.summary, a.status, a.origin
         FROM task_articles ta JOIN kb_articles a ON a.id = ta.article_id
         WHERE ta.task_id = $1 ORDER BY a.title",
    )
    .bind(id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "articles": rows.iter().map(|r| json!({
            "id": r.get::<Uuid, _>("id"),
            "title": r.get::<String, _>("title"),
            "summary": r.get::<String, _>("summary"),
            "status": r.get::<String, _>("status"),
            "origin": r.get::<String, _>("origin"),
        })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct ArticleLinks {
    article_ids: Vec<Uuid>,
}

/// Replace the set of articles tagged onto a card.
///
/// A full replacement rather than add/remove endpoints: the UI holds the whole
/// list anyway, and two endpoints invite the state where the client and the
/// server disagree about what is attached.
async fn set_task_articles(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<ArticleLinks>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(id)).await?;
    link_articles(&state, id, &body.article_ids).await?;
    Ok(Json(json!({ "linked": body.article_ids.len() })))
}
