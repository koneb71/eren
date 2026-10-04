//! Who reports to whom.
//!
//! `agents.reports_to` makes a workspace's agents a tree. Delegation flows
//! down it — a manager agent's pass is told its reports, and may hand cards
//! only to agents in its subtree — and trouble flows up it: a wake raised
//! about an agent's card also wakes its manager's routine.
//!
//! The tree is kept a tree here, by the one function that sets a manager:
//! the same workspace, no cycle, a bounded depth, and no retired manager. The
//! shapes are copied from `kb::tree`, which keeps knowledge pages a tree.

use crate::db::Db;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

/// Levels below the top. Deep enough for any real org; shallow enough that a
/// walk up or down is always cheap and a mistake is visible.
pub const MAX_DEPTH: i32 = 8;

/// Why a manager cannot be set, said to the person setting it.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refused {
    #[error("an agent cannot report to itself")]
    Itself,
    #[error("that manager is in another workspace")]
    OtherWorkspace,
    #[error("{0} is retired and cannot manage anyone")]
    Retired(String),
    #[error("{0} already reports, directly or not, to this agent — that would be a loop")]
    Cycle(String),
    #[error("that would make the chart deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("no such agent")]
    Missing,
}

/// Can `agent` report to `manager`? Every rule, read from the database.
pub async fn vet_manager(
    db: &Db,
    agent: Uuid,
    manager: Uuid,
) -> anyhow::Result<Result<(), Refused>> {
    if agent == manager {
        return Ok(Err(Refused::Itself));
    }
    let rows = sqlx::query("SELECT id, workspace_id, name, status FROM agents WHERE id = ANY($1)")
        .bind([agent, manager].as_slice())
        .fetch_all(&db.pool)
        .await?;
    let find = |id: Uuid| rows.iter().find(|r| r.get::<Uuid, _>("id") == id);
    let (Some(a), Some(m)) = (find(agent), find(manager)) else {
        return Ok(Err(Refused::Missing));
    };
    if a.get::<Option<Uuid>, _>("workspace_id") != m.get::<Option<Uuid>, _>("workspace_id") {
        return Ok(Err(Refused::OtherWorkspace));
    }
    let manager_name: String = m.get("name");
    if m.get::<String, _>("status") == "retired" {
        return Ok(Err(Refused::Retired(manager_name)));
    }
    if subtree(db, agent).await?.contains(&manager) {
        return Ok(Err(Refused::Cycle(manager_name)));
    }
    if depth_of(db, manager).await? + 1 + height_of(db, agent).await? > MAX_DEPTH {
        return Ok(Err(Refused::TooDeep));
    }
    Ok(Ok(()))
}

/// Set (or clear) an agent's manager. Changes to one workspace's chart are
/// serialised on an advisory lock, so two moves made at once — A under B,
/// B under A — cannot each pass the cycle check against the other's
/// not-yet-written row.
pub async fn set_manager(
    db: &Db,
    agent: Uuid,
    manager: Option<Uuid>,
) -> anyhow::Result<Result<(), Refused>> {
    let mut tx = db.pool.begin().await?;
    if let Err(why) = set_manager_in(&mut tx, db, agent, manager).await? {
        return Ok(Err(why));
    }
    tx.commit().await?;
    Ok(Ok(()))
}

/// [`set_manager`] inside the caller's transaction, so a manager change and
/// the rest of an edit land together or not at all. The lock is held until
/// the caller commits.
pub async fn set_manager_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    db: &Db,
    agent: Uuid,
    manager: Option<Uuid>,
) -> anyhow::Result<Result<(), Refused>> {
    lock_chart(tx, agent).await?;
    if let Some(manager) = manager {
        // Read through the pool, which sees every committed move: any other
        // move in this workspace is serialised behind the lock just taken.
        if let Err(why) = vet_manager(db, agent, manager).await? {
            return Ok(Err(why));
        }
    }
    sqlx::query("UPDATE agents SET reports_to = $2 WHERE id = $1")
        .bind(agent)
        .bind(manager)
        .execute(&mut **tx)
        .await?;
    Ok(Ok(()))
}

/// An agent and everyone under it, the agent first. A retired agent is no
/// longer anyone's report, whatever its row still says.
pub async fn subtree(db: &Db, root: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE down AS (
             SELECT id, 0 AS depth FROM agents WHERE id = $1
             UNION ALL
             SELECT a.id, down.depth + 1 FROM agents a JOIN down ON a.reports_to = down.id
              WHERE down.depth < 16 AND a.status <> 'retired')
         SELECT id FROM down ORDER BY depth",
    )
    .bind(root)
    .fetch_all(&db.pool)
    .await?)
}

/// Levels above an agent: 0 at the top.
async fn depth_of(db: &Db, id: Uuid) -> anyhow::Result<i32> {
    Ok(sqlx::query_scalar::<_, Option<i32>>(
        "WITH RECURSIVE up AS (
             SELECT id, reports_to, 0 AS depth FROM agents WHERE id = $1
             UNION ALL
             SELECT a.id, a.reports_to, up.depth + 1 FROM agents a JOIN up ON a.id = up.reports_to
              WHERE up.depth < 16)
         SELECT max(depth) FROM up",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await?
    .unwrap_or(0))
}

/// Levels below an agent: 0 for one with no reports still working.
async fn height_of(db: &Db, id: Uuid) -> anyhow::Result<i32> {
    Ok(sqlx::query_scalar::<_, Option<i32>>(
        "WITH RECURSIVE down AS (
             SELECT id, 0 AS depth FROM agents WHERE id = $1
             UNION ALL
             SELECT a.id, down.depth + 1 FROM agents a JOIN down ON a.reports_to = down.id
              WHERE down.depth < 16 AND a.status <> 'retired')
         SELECT max(depth) FROM down",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await?
    .unwrap_or(0))
}

/// Hold the agent's workspace's chart for the rest of the transaction. Every
/// write of `reports_to` takes it, or a move vetted against the chart could
/// interleave with a lift that changes the chart under it.
async fn lock_chart(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('eren.org_chart:' || workspace_id::text))
           FROM agents WHERE id = $1",
    )
    .bind(agent)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Retiring a manager lifts its reports to its own manager, so nobody is
/// left reporting to someone who will never manage again. Under the chart's
/// lock, like [`set_manager`]: unlocked, a move made at the same moment — a
/// report placed under the retiring manager, its manager moved under one of
/// its reports — was vetted against a chart this then changed, and could
/// leave a report under a retiree or close a cycle.
pub async fn lift_reports(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    retiring: Uuid,
) -> anyhow::Result<u64> {
    lock_chart(tx, retiring).await?;
    Ok(sqlx::query(
        "UPDATE agents SET reports_to = (SELECT reports_to FROM agents WHERE id = $1)
          WHERE reports_to = $1",
    )
    .bind(retiring)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

/// [`lift_reports`] on its own, for the delete path.
pub async fn lift(db: &Db, leaving: Uuid) -> anyhow::Result<u64> {
    let mut tx = db.pool.begin().await?;
    let n = lift_reports(&mut tx, leaving).await?;
    tx.commit().await?;
    Ok(n)
}

/// One box on the chart.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: Uuid,
    pub name: String,
    pub title: Option<String>,
    pub icon: String,
    pub color: String,
    pub engine: Option<String>,
    pub model_tier: String,
    pub status: String,
    pub reports_to: Option<Uuid>,
    /// The card it is working on right now, if any.
    pub live_card: Option<LiveCard>,
    pub runs_today: i64,
    pub spend_today_usd: f64,
    pub heartbeat_secs: Option<i32>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveCard {
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub title: String,
}

/// Every agent of a workspace that is not retired, with what it is doing.
pub async fn chart(db: &Db, workspace: Uuid) -> anyhow::Result<Vec<Node>> {
    let rows = sqlx::query(
        "SELECT a.id, a.name, a.title, a.icon, a.color, a.engine, a.model_tier, a.status,
                -- A manager who is retired is drawn as no manager.
                CASE WHEN m.status = 'retired' THEN NULL ELSE a.reports_to END AS reports_to,
                a.heartbeat_secs, a.last_heartbeat_at,
                live.task_id, live.project_id, live.title AS live_title,
                COALESCE(today.runs, 0) AS runs_today, COALESCE(today.spend, 0)::float8 AS spend
           FROM agents a
           LEFT JOIN agents m ON m.id = a.reports_to
           LEFT JOIN LATERAL (
                SELECT t.id AS task_id, t.project_id, t.title FROM runs r JOIN tasks t ON t.id = r.task_id
                 WHERE COALESCE(r.agent_id, t.agent_id) = a.id
                   AND r.status IN ('starting','running','waiting_permission')
                 ORDER BY r.created_at DESC LIMIT 1) live ON TRUE
           LEFT JOIN LATERAL (
                SELECT count(*) AS runs, sum(r.cost_usd) AS spend FROM runs r
                  LEFT JOIN tasks t ON t.id = r.task_id
                 WHERE COALESCE(r.agent_id, t.agent_id) = a.id
                   AND r.created_at >= date_trunc('day', now())) today ON TRUE
          WHERE a.workspace_id = $1 AND a.status <> 'retired'
          ORDER BY a.name",
    )
    .bind(workspace)
    .fetch_all(&db.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Node {
            id: r.get("id"),
            name: r.get("name"),
            title: r.get("title"),
            icon: r.get("icon"),
            color: r.get("color"),
            engine: r.get("engine"),
            model_tier: r.get("model_tier"),
            status: r.get("status"),
            reports_to: r.get("reports_to"),
            live_card: r.get::<Option<Uuid>, _>("task_id").map(|task_id| LiveCard {
                task_id,
                project_id: r.get("project_id"),
                title: r.get("live_title"),
            }),
            runs_today: r.get("runs_today"),
            spend_today_usd: r.get("spend"),
            heartbeat_secs: r.get("heartbeat_secs"),
            last_heartbeat_at: r.get("last_heartbeat_at"),
        })
        .collect())
}

/// May a manager pass hand a card to `agent`? Only within the pass's own
/// agent's subtree — when that agent manages anyone at all. A manager with
/// no reports is not restricted: the chart is opt-in, and a workspace that
/// never drew one keeps working as it did. `Err` carries the refusal, naming
/// who it may hand work to.
pub async fn may_delegate(
    db: &Db,
    pass_id: Uuid,
    agent: Uuid,
) -> anyhow::Result<Result<(), String>> {
    let manager: Option<Uuid> = sqlx::query_scalar(
        "SELECT rt.agent_id FROM routine_runs rr JOIN routines rt ON rt.id = rr.routine_id
          WHERE rr.id = $1",
    )
    .bind(pass_id)
    .fetch_optional(&db.pool)
    .await?
    .flatten();
    let Some(manager) = manager else {
        return Ok(Ok(()));
    };
    let team = subtree(db, manager).await?;
    if team.len() <= 1 || team.contains(&agent) {
        return Ok(Ok(()));
    }
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM agents WHERE id = ANY($1) AND status <> 'retired' ORDER BY name",
    )
    .bind(&team)
    .fetch_all(&db.pool)
    .await?;
    Ok(Err(format!(
        "that agent does not report to you. You may hand work to: {}",
        names.join(", ")
    )))
}

/// An agent's direct reports, for its manager pass: name and title.
pub async fn reports(db: &Db, manager: Uuid) -> anyhow::Result<Vec<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT name, title FROM agents WHERE reports_to = $1 AND status <> 'retired' ORDER BY name",
    )
    .bind(manager)
    .fetch_all(&db.pool)
    .await?)
}

/// The "your team" section of a manager pass. Pure, for the tests.
pub fn render_reports(reports: &[(String, Option<String>)]) -> Option<String> {
    if reports.is_empty() {
        return None;
    }
    let lines: Vec<String> = reports
        .iter()
        .map(
            |(name, title)| match title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(t) => format!("- @{name} — {t}"),
                None => format!("- @{name}"),
            },
        )
        .collect();
    Some(format!(
        "## Your reports\n\nHand work to these agents — and only these, or the agents under \
         them; assigning anyone else is refused.\n\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manager_pass_lists_its_reports() {
        assert_eq!(render_reports(&[]), None);
        let text = render_reports(&[
            ("Ada".into(), Some("Backend lead".into())),
            ("Bo".into(), Some("  ".into())),
        ])
        .unwrap();
        assert!(text.contains("- @Ada — Backend lead"));
        assert!(text.contains("- @Bo\n") || text.ends_with("- @Bo"));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    async fn agent(t: &testdb::TestDb, ws: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO agents (workspace_id, name) VALUES ($1, $2) RETURNING id")
            .bind(ws)
            .bind(name)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    }

    async fn manager_of(t: &testdb::TestDb, id: Uuid) -> Option<Uuid> {
        sqlx::query_scalar("SELECT reports_to FROM agents WHERE id = $1")
            .bind(id)
            .fetch_one(&t.db.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_chart_stays_a_tree_in_one_workspace() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = t.project(dir.path(), false).await;
        let (other_ws, _) = t.project(&dir.path().join("other"), false).await;
        let ceo = agent(&t, ws, "Ceo").await;
        let lead = agent(&t, ws, "Lead").await;
        let dev = agent(&t, ws, "Dev").await;
        let stranger = agent(&t, other_ws, "Stranger").await;

        set_manager(&t.db, lead, Some(ceo)).await.unwrap().unwrap();
        set_manager(&t.db, dev, Some(lead)).await.unwrap().unwrap();
        assert_eq!(
            set_manager(&t.db, ceo, Some(dev)).await.unwrap(),
            Err(Refused::Cycle("Dev".into()))
        );
        assert_eq!(
            set_manager(&t.db, ceo, Some(ceo)).await.unwrap(),
            Err(Refused::Itself)
        );
        assert_eq!(
            set_manager(&t.db, dev, Some(stranger)).await.unwrap(),
            Err(Refused::OtherWorkspace)
        );
        assert_eq!(manager_of(&t, ceo).await, None, "a refusal writes nothing");

        // Too deep: a chain of MAX_DEPTH under the CEO, then one more.
        let mut above = dev;
        for i in 0..(MAX_DEPTH - 2) {
            let next = agent(&t, ws, &format!("L{i}")).await;
            set_manager(&t.db, next, Some(above))
                .await
                .unwrap()
                .unwrap();
            above = next;
        }
        let one_too_many = agent(&t, ws, "Deep").await;
        assert_eq!(
            set_manager(&t.db, one_too_many, Some(above)).await.unwrap(),
            Err(Refused::TooDeep)
        );
        t.finish().await;
    }

    #[tokio::test]
    async fn a_manager_set_inside_an_edit_that_fails_is_not_set() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (ws, _) = t.project(dir.path(), false).await;
        let lead = agent(&t, ws, "Lead").await;
        let dev = agent(&t, ws, "Dev").await;
        let mut tx = t.db.pool.begin().await.unwrap();
        set_manager_in(&mut tx, &t.db, dev, Some(lead))
            .await
            .unwrap()
            .unwrap();
        // The rest of the edit fails — a duplicate name, say — and rolls back.
        drop(tx);
        assert_eq!(manager_of(&t, dev).await, None);
        // And a refusal inside one writes nothing either.
        let mut tx = t.db.pool.begin().await.unwrap();
        assert_eq!(
            set_manager_in(&mut tx, &t.db, dev, Some(dev))
                .await
                .unwrap(),
            Err(Refused::Itself)
        );
        t.finish().await;
    }

    #[tokio::test]
    async fn retiring_a_manager_lifts_its_reports_and_it_manages_no_one_after() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let orch = t.orchestrator(dir.path());
        let (ws, _) = t.project(dir.path(), false).await;
        let ceo = agent(&t, ws, "Ceo").await;
        let lead = agent(&t, ws, "Lead").await;
        let dev = agent(&t, ws, "Dev").await;
        set_manager(&t.db, lead, Some(ceo)).await.unwrap().unwrap();
        set_manager(&t.db, dev, Some(lead)).await.unwrap().unwrap();

        orch.retire_agent(lead).await.unwrap();
        assert_eq!(
            manager_of(&t, dev).await,
            Some(ceo),
            "lifted to the manager's manager"
        );
        // And it has left the chart itself: its old manager's tree is just
        // the people still in it.
        assert_eq!(manager_of(&t, lead).await, None);
        assert_eq!(subtree(&t.db, ceo).await.unwrap(), vec![ceo, dev]);
        assert_eq!(
            set_manager(&t.db, dev, Some(lead)).await.unwrap(),
            Err(Refused::Retired("Lead".into()))
        );
        t.finish().await;
    }

    #[tokio::test]
    async fn a_manager_with_reports_delegates_down_its_own_tree() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (ws, project) = t.project(dir.path(), false).await;
        let lead = agent(&t, ws, "Lead").await;
        let dev = agent(&t, ws, "Dev").await;
        let outsider = agent(&t, ws, "Outsider").await;
        let routine: Uuid = sqlx::query_scalar(
            "INSERT INTO routines (workspace_id, name, kind, project_id, prompt, cron_expr, agent_id)
             VALUES ($1, 'm', 'manage', $2, '', '0 9 * * *', $3) RETURNING id",
        )
        .bind(ws)
        .bind(project)
        .bind(lead)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let pass: Uuid = sqlx::query_scalar(
            "INSERT INTO routine_runs (routine_id, trigger) VALUES ($1, 'schedule') RETURNING id",
        )
        .bind(routine)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();

        // No reports yet: nothing is restricted.
        assert!(may_delegate(&t.db, pass, outsider).await.unwrap().is_ok());
        set_manager(&t.db, dev, Some(lead)).await.unwrap().unwrap();
        assert!(may_delegate(&t.db, pass, dev).await.unwrap().is_ok());
        assert!(
            may_delegate(&t.db, pass, lead).await.unwrap().is_ok(),
            "itself"
        );
        let refused = may_delegate(&t.db, pass, outsider)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            refused.contains("Dev") && refused.contains("Lead") && !refused.contains("Outsider")
        );

        // Once its only report has retired, a manager has no reports and is
        // not restricted — even if the retiree's row still names it, as one
        // retired before retiring cleared the link would.
        sqlx::query("UPDATE agents SET status = 'retired' WHERE id = $1")
            .bind(dev)
            .execute(&t.db.pool)
            .await
            .unwrap();
        assert!(may_delegate(&t.db, pass, outsider).await.unwrap().is_ok());
        assert_eq!(height_of(&t.db, lead).await.unwrap(), 0);
        t.finish().await;
    }

    #[tokio::test]
    async fn trouble_on_a_card_goes_up_to_its_agents_manager() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (ws, project) = t.project(dir.path(), false).await;
        let (_, elsewhere) = t.project(&dir.path().join("elsewhere"), false).await;
        sqlx::query("UPDATE projects SET workspace_id = $2 WHERE id = $1")
            .bind(elsewhere)
            .bind(ws)
            .execute(&t.db.pool)
            .await
            .unwrap();
        let lead = agent(&t, ws, "Lead").await;
        let dev = agent(&t, ws, "Dev").await;
        set_manager(&t.db, dev, Some(lead)).await.unwrap().unwrap();
        // The lead manages a different board, and asked to hear of failures.
        let routine: Uuid = sqlx::query_scalar(
            "INSERT INTO routines (workspace_id, name, kind, project_id, prompt, cron_expr, agent_id, on_events)
             VALUES ($1, 'm', 'manage', $2, '', '0 9 * * *', $3, '{failed,landed}') RETURNING id",
        )
        .bind(ws)
        .bind(elsewhere)
        .bind(lead)
        .fetch_one(&t.db.pool)
        .await
        .unwrap();
        let card = t.card(project, "dev's card").await;
        sqlx::query("UPDATE tasks SET agent_id = $2 WHERE id = $1")
            .bind(card)
            .bind(dev)
            .execute(&t.db.pool)
            .await
            .unwrap();
        crate::wake::raise(&t.db, card, None, crate::wake::Kind::Failed, "").await;
        crate::wake::raise(&t.db, card, None, crate::wake::Kind::Landed, "").await;
        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM wakeups WHERE routine_id = $1")
                .bind(routine)
                .fetch_all(&t.db.pool)
                .await
                .unwrap();
        assert_eq!(
            kinds,
            ["failed"],
            "trouble goes up; good news stays on its board"
        );

        // And the lead's pass is told where it happened, and that it cannot
        // act on it from there.
        let news = crate::wake::pending(&t.db, routine).await.unwrap();
        let project_name: String = sqlx::query_scalar("SELECT name FROM projects WHERE id = $1")
            .bind(project)
            .fetch_one(&t.db.pool)
            .await
            .unwrap();
        assert_eq!(news[0].elsewhere.as_deref(), Some(project_name.as_str()));
        let said = crate::wake::render(&news).unwrap();
        assert!(said.contains(&format!("On {project_name}, your report's card")));
        assert!(said.contains("must not re-create them"));
        t.finish().await;
    }
}
