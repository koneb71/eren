//! Which workspace a thing belongs to — the question every request asks
//! once accounts are on (see `crate::users`), because a workspace is whose it
//! is: `workspaces.owner_id`.
//!
//! A closed set of kinds, each with one fixed query from its row to its
//! workspace. Nothing here is built from request text: the id is bound, the
//! SQL is chosen by the enum — the same rule `apps::query` keeps.
//!
//! Runs are the awkward one. A run hangs off whichever of nine parents
//! started it (a card, a workflow, a project, a knowledge-base page or
//! project, a research, a chat, a team, an agent), so its workspace is the
//! first of those that answers — read live, so a run can never disagree with
//! the thing it belongs to.

use crate::db::Db;
use uuid::Uuid;

/// Something that lives in a workspace, by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owned {
    Workspace(Uuid),
    Project(Uuid),
    Task(Uuid),
    Run(Uuid),
    Agent(Uuid),
    AgentMemory(Uuid),
    Team(Uuid),
    Routine(Uuid),
    Goal(Uuid),
    Skill(Uuid),
    KbArticle(Uuid),
    KbAsset(Uuid),
    Chat(Uuid),
    ChatMessage(Uuid),
    Research(Uuid),
    App(Uuid),
    Workflow(Uuid),
    Preview(Uuid),
    McpServer(Uuid),
    Attachment(Uuid),
    Comment(Uuid),
    Decision(Uuid),
    /// A budget policy. A machine-wide one belongs to no workspace — only
    /// the admin may touch it.
    Budget(Uuid),
}

/// The workspace of the run whose id is `$1`.
const RUN_WORKSPACE: &str = "SELECT COALESCE(
    (SELECT p.workspace_id FROM tasks t JOIN projects p ON p.id = t.project_id WHERE t.id = r.task_id),
    (SELECT p.workspace_id FROM workflows w JOIN projects p ON p.id = w.project_id WHERE w.id = r.workflow_id),
    (SELECT workspace_id FROM projects WHERE id = r.project_id),
    (SELECT workspace_id FROM kb_articles WHERE id = r.kb_article_id),
    (SELECT workspace_id FROM projects WHERE id = r.kb_project_id),
    (SELECT COALESCE(x.workspace_id, p.workspace_id) FROM researches x
       LEFT JOIN projects p ON p.id = x.project_id WHERE x.id = r.research_id),
    (SELECT COALESCE(c.workspace_id, p.workspace_id) FROM chats c
       LEFT JOIN projects p ON p.id = c.project_id WHERE c.id = r.chat_id),
    (SELECT workspace_id FROM teams WHERE id = r.team_id),
    (SELECT workspace_id FROM agents WHERE id = r.agent_id)
) FROM runs r WHERE r.id = $1";

impl Owned {
    /// One query, `$1` the id, selecting the workspace id (or nothing).
    fn sql(self) -> &'static str {
        match self {
            Owned::Workspace(_) => "SELECT id FROM workspaces WHERE id = $1",
            Owned::Project(_) => "SELECT workspace_id FROM projects WHERE id = $1",
            Owned::Task(_) => {
                "SELECT p.workspace_id FROM tasks t JOIN projects p ON p.id = t.project_id
                  WHERE t.id = $1"
            }
            Owned::Run(_) => RUN_WORKSPACE,
            Owned::Agent(_) => "SELECT workspace_id FROM agents WHERE id = $1",
            Owned::AgentMemory(_) => {
                "SELECT a.workspace_id FROM agent_memories m JOIN agents a ON a.id = m.agent_id
                  WHERE m.id = $1"
            }
            Owned::Team(_) => "SELECT workspace_id FROM teams WHERE id = $1",
            Owned::Routine(_) => "SELECT workspace_id FROM routines WHERE id = $1",
            Owned::Goal(_) => "SELECT workspace_id FROM goals WHERE id = $1",
            Owned::Skill(_) => "SELECT workspace_id FROM skills WHERE id = $1",
            Owned::KbArticle(_) => "SELECT workspace_id FROM kb_articles WHERE id = $1",
            Owned::KbAsset(_) => "SELECT workspace_id FROM kb_assets WHERE id = $1",
            Owned::Chat(_) => {
                "SELECT COALESCE(c.workspace_id, p.workspace_id) FROM chats c
                   LEFT JOIN projects p ON p.id = c.project_id WHERE c.id = $1"
            }
            Owned::ChatMessage(_) => {
                "SELECT COALESCE(c.workspace_id, p.workspace_id) FROM chat_messages m
                   JOIN chats c ON c.id = m.chat_id
                   LEFT JOIN projects p ON p.id = c.project_id WHERE m.id = $1"
            }
            Owned::Research(_) => {
                "SELECT COALESCE(x.workspace_id, p.workspace_id) FROM researches x
                   LEFT JOIN projects p ON p.id = x.project_id WHERE x.id = $1"
            }
            Owned::App(_) => "SELECT workspace_id FROM apps WHERE id = $1",
            Owned::Workflow(_) => {
                "SELECT p.workspace_id FROM workflows w JOIN projects p ON p.id = w.project_id
                  WHERE w.id = $1"
            }
            Owned::Preview(_) => {
                "SELECT p.workspace_id FROM previews v JOIN projects p ON p.id = v.project_id
                  WHERE v.id = $1"
            }
            Owned::McpServer(_) => "SELECT workspace_id FROM mcp_servers WHERE id = $1",
            Owned::Attachment(_) => {
                "SELECT COALESCE(a.workspace_id, p.workspace_id) FROM attachments a
                   LEFT JOIN projects p ON p.id = a.project_id WHERE a.id = $1"
            }
            Owned::Comment(_) => {
                "SELECT p.workspace_id FROM task_comments c
                   JOIN tasks t ON t.id = c.task_id
                   JOIN projects p ON p.id = t.project_id WHERE c.id = $1"
            }
            Owned::Decision(_) => "SELECT workspace_id FROM decisions WHERE id = $1",
            Owned::Budget(_) => unreachable!("budgets are resolved through their scope"),
        }
    }

    fn id(self) -> Uuid {
        match self {
            Owned::Workspace(id)
            | Owned::Project(id)
            | Owned::Task(id)
            | Owned::Run(id)
            | Owned::Agent(id)
            | Owned::AgentMemory(id)
            | Owned::Team(id)
            | Owned::Routine(id)
            | Owned::Goal(id)
            | Owned::Skill(id)
            | Owned::KbArticle(id)
            | Owned::KbAsset(id)
            | Owned::Chat(id)
            | Owned::ChatMessage(id)
            | Owned::Research(id)
            | Owned::App(id)
            | Owned::Workflow(id)
            | Owned::Preview(id)
            | Owned::McpServer(id)
            | Owned::Attachment(id)
            | Owned::Comment(id)
            | Owned::Decision(id)
            | Owned::Budget(id) => id,
        }
    }
}

/// The workspace `what` lives in, or `None` when it does not exist (or, for
/// a machine-wide budget, belongs to no workspace).
pub async fn workspace_of(db: &Db, what: Owned) -> anyhow::Result<Option<Uuid>> {
    if let Owned::Budget(id) = what {
        let row: Option<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT scope_kind, scope_id FROM budget_policies WHERE id = $1")
                .bind(id)
                .fetch_optional(&db.pool)
                .await?;
        let Some((kind, Some(scope))) = row else {
            return Ok(None);
        };
        return match budget_scope(&kind, scope) {
            Some(inner) => Box::pin(workspace_of(db, inner)).await,
            None => Ok(None),
        };
    }
    Ok(sqlx::query_scalar::<_, Option<Uuid>>(what.sql())
        .bind(what.id())
        .fetch_optional(&db.pool)
        .await?
        .flatten())
}

/// What a budget policy's scope names, as something with a workspace.
/// `machine` names none.
pub fn budget_scope(kind: &str, id: Uuid) -> Option<Owned> {
    match kind {
        "workspace" => Some(Owned::Workspace(id)),
        "project" => Some(Owned::Project(id)),
        "agent" => Some(Owned::Agent(id)),
        "team" => Some(Owned::Team(id)),
        "routine" => Some(Owned::Routine(id)),
        _ => None,
    }
}

/// Whether `user` owns the workspace `what` lives in — or, for a personal
/// skill, which lives in none, whether it is `user`'s.
pub async fn owned_by(db: &Db, what: Owned, user: Uuid) -> anyhow::Result<bool> {
    if let Owned::Skill(id) = what {
        let personal: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT owner_id FROM skills WHERE id = $1 AND workspace_id IS NULL",
        )
        .bind(id)
        .fetch_optional(&db.pool)
        .await?;
        if let Some(owner) = personal {
            return Ok(owner == Some(user));
        }
    }
    let Some(ws) = workspace_of(db, what).await? else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspaces WHERE id = $1 AND owner_id = $2)",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(&db.pool)
    .await?)
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    /// Every kind's query runs, finds its row's workspace, and finds nothing
    /// for an id that does not exist.
    #[tokio::test]
    async fn every_kind_finds_its_workspace() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let q = |sql: &'static str| sqlx::query_scalar::<_, Uuid>(sql);
        let ws: Uuid = q("INSERT INTO workspaces (name) VALUES ('w') RETURNING id")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let one = |sql: &'static str, bind: Uuid| async move {
            sqlx::query_scalar::<_, Uuid>(sql)
                .bind(bind)
                .fetch_one(&db.pool)
                .await
                .unwrap()
        };
        let project = one(
            "INSERT INTO projects (name, path, workspace_id) VALUES ('p', '/tmp/p', $1) RETURNING id",
            ws,
        )
        .await;
        let task = one(
            "INSERT INTO tasks (project_id, title, prompt) VALUES ($1, 't', 'p') RETURNING id",
            project,
        )
        .await;
        let agent = one(
            "INSERT INTO agents (name, workspace_id) VALUES ('a', $1) RETURNING id",
            ws,
        )
        .await;
        let team = one(
            "INSERT INTO teams (name, pattern, workspace_id) VALUES ('t', 'pipeline', $1) RETURNING id",
            ws,
        )
        .await;
        let chat = one(
            "INSERT INTO chats (workspace_id) VALUES ($1) RETURNING id",
            ws,
        )
        .await;
        let project_chat = one(
            "INSERT INTO chats (project_id) VALUES ($1) RETURNING id",
            project,
        )
        .await;
        let card_run = one("INSERT INTO runs (task_id) VALUES ($1) RETURNING id", task).await;
        let chat_run = one(
            "INSERT INTO runs (chat_id) VALUES ($1) RETURNING id",
            project_chat,
        )
        .await;
        let agent_run = one(
            "INSERT INTO runs (agent_id) VALUES ($1) RETURNING id",
            agent,
        )
        .await;
        // A general chat's upload belongs to the workspace, not a project.
        let general_upload = one(
            "INSERT INTO attachments (workspace_id, filename, mime, kind, size_bytes, disk_path)
             VALUES ($1, 'a.png', 'image/png', 'image', 1, '') RETURNING id",
            ws,
        )
        .await;
        let project_upload = one(
            "INSERT INTO attachments (project_id, filename, mime, kind, size_bytes, disk_path)
             VALUES ($1, 'b.png', 'image/png', 'image', 1, '') RETURNING id",
            project,
        )
        .await;
        let budget = one(
            "INSERT INTO budget_policies (name, scope_kind, scope_id, cap_runs)
             VALUES ('b', 'project', $1, 1) RETURNING id",
            project,
        )
        .await;

        for what in [
            Owned::Workspace(ws),
            Owned::Project(project),
            Owned::Task(task),
            Owned::Agent(agent),
            Owned::Team(team),
            Owned::Chat(chat),
            Owned::Chat(project_chat),
            Owned::Run(card_run),
            Owned::Run(chat_run),
            Owned::Run(agent_run),
            Owned::Budget(budget),
            Owned::Attachment(general_upload),
            Owned::Attachment(project_upload),
        ] {
            assert_eq!(workspace_of(db, what).await.unwrap(), Some(ws), "{what:?}");
        }

        // Every query is valid SQL, whatever the kind: a missing id is None.
        let nobody = Uuid::new_v4();
        for what in [
            Owned::Workspace(nobody),
            Owned::Project(nobody),
            Owned::Task(nobody),
            Owned::Run(nobody),
            Owned::Agent(nobody),
            Owned::AgentMemory(nobody),
            Owned::Team(nobody),
            Owned::Routine(nobody),
            Owned::Goal(nobody),
            Owned::Skill(nobody),
            Owned::KbArticle(nobody),
            Owned::KbAsset(nobody),
            Owned::Chat(nobody),
            Owned::ChatMessage(nobody),
            Owned::Research(nobody),
            Owned::App(nobody),
            Owned::Workflow(nobody),
            Owned::Preview(nobody),
            Owned::McpServer(nobody),
            Owned::Attachment(nobody),
            Owned::Comment(nobody),
            Owned::Decision(nobody),
            Owned::Budget(nobody),
        ] {
            assert_eq!(workspace_of(db, what).await.unwrap(), None, "{what:?}");
        }
        t.finish().await;
    }

    #[tokio::test]
    async fn only_the_owner_owns() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let admin = crate::users::create_admin(db, "admin", "a long password")
            .await
            .unwrap();
        let bea = crate::users::sign_up(db, "bea", "another password")
            .await
            .unwrap();
        let mine = crate::users::workspaces(db, admin.id).await.unwrap()[0];
        let hers = crate::users::workspaces(db, bea.id).await.unwrap()[0];
        assert!(owned_by(db, Owned::Workspace(mine), admin.id)
            .await
            .unwrap());
        assert!(!owned_by(db, Owned::Workspace(mine), bea.id).await.unwrap());
        assert!(owned_by(db, Owned::Workspace(hers), bea.id).await.unwrap());
        assert!(!owned_by(db, Owned::Workspace(Uuid::new_v4()), bea.id)
            .await
            .unwrap());
        t.finish().await;
    }
}
