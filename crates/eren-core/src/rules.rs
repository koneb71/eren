//! A person's rules for agents, carried into each new project.
//!
//! Written once (Settings → Your rules) and, when a repository project is
//! added or cloned, written into it as `AGENTS.md` — the file Codex, OpenCode,
//! Cursor and Amp read — plus a one-line `CLAUDE.md` that imports it, so
//! Claude Code reads the same text without a second copy to drift. Both are
//! committed, because a file that is only in the checkout never reaches an
//! agent's worktree, which is cut from the branch.
//!
//! From then on the files are the repository's: edited, reviewed and
//! versioned like anything else in it. Changing your rules later does not
//! reach projects already made — that is the trade this shape makes for living
//! in the repo. A file the repository already has is never overwritten.
//!
//! Whose rules: the workspace owner's (`users.rules`), or with accounts off the
//! one local person's (`settings` key `rules`), which the first admin adopts.

use crate::db::Db;
use std::path::Path;
use uuid::Uuid;

/// Long enough for a real house style; short enough to stay a file people
/// read rather than one they scroll past.
pub const MAX_CHARS: usize = 20_000;

const SETTINGS_KEY: &str = "rules";
const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
/// Claude Code's import syntax: the file reads as if AGENTS.md were pasted in.
const CLAUDE_IMPORT: &str = "@AGENTS.md\n";

/// The rules of `owner` (`None`: accounts off, the local person).
pub async fn get(db: &Db, owner: Option<Uuid>) -> anyhow::Result<String> {
    Ok(match owner {
        Some(user) => sqlx::query_scalar("SELECT rules FROM users WHERE id = $1")
            .bind(user)
            .fetch_optional(&db.pool)
            .await?
            .unwrap_or_default(),
        None => {
            sqlx::query_scalar::<_, serde_json::Value>("SELECT value FROM settings WHERE key = $1")
                .bind(SETTINGS_KEY)
                .fetch_optional(&db.pool)
                .await?
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_default()
        }
    })
}

pub async fn set(db: &Db, owner: Option<Uuid>, text: &str) -> anyhow::Result<()> {
    if text.chars().count() > MAX_CHARS {
        anyhow::bail!("rules are at most {MAX_CHARS} characters");
    }
    match owner {
        Some(user) => {
            sqlx::query("UPDATE users SET rules = $2 WHERE id = $1")
                .bind(user)
                .bind(text)
                .execute(&db.pool)
                .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(SETTINGS_KEY)
            .bind(serde_json::json!({ "text": text }))
            .execute(&db.pool)
            .await?;
        }
    }
    Ok(())
}

/// Give the first admin what the local person had before accounts existed.
/// Called inside `users::create_admin`'s transaction.
pub(crate) async fn adopt_local(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    admin: Uuid,
) -> anyhow::Result<()> {
    let local: Option<serde_json::Value> =
        sqlx::query_scalar("DELETE FROM settings WHERE key = $1 RETURNING value")
            .bind(SETTINGS_KEY)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(text) =
        local.and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
    {
        sqlx::query("UPDATE users SET rules = $2 WHERE id = $1")
            .bind(admin)
            .bind(text)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("UPDATE skills SET owner_id = $1 WHERE workspace_id IS NULL AND owner_id IS NULL")
        .bind(admin)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// What seeding a new project did, for the person who added it.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Seeded {
    /// Files written and committed.
    pub written: Vec<String>,
    /// Why nothing was written, when something stopped it.
    pub skipped: Option<String>,
}

/// Write the workspace owner's rules into a new repository project at `path`.
///
/// Never fails the thing that called it: a project that could not be seeded is
/// still a project, and the reason comes back in [`Seeded::skipped`].
pub async fn seed_project(db: &Db, workspace: Uuid, path: &Path) -> Seeded {
    match try_seed(db, workspace, path).await {
        Ok(seeded) => seeded,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not write rules into the project");
            Seeded {
                written: vec![],
                skipped: Some(format!("could not write your rules: {e}")),
            }
        }
    }
}

async fn try_seed(db: &Db, workspace: Uuid, path: &Path) -> anyhow::Result<Seeded> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT owner_id FROM workspaces WHERE id = $1")
        .bind(workspace)
        .fetch_optional(&db.pool)
        .await?
        .flatten();
    let rules = get(db, owner).await?;
    let plan = plan(
        &rules,
        path.join(AGENTS_FILE).exists(),
        path.join(CLAUDE_FILE).exists(),
    );
    let Some(files) = plan.files else {
        return Ok(Seeded {
            written: vec![],
            skipped: plan.skipped,
        });
    };
    for (name, body) in &files {
        tokio::fs::write(path.join(name), body).await?;
    }
    let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    let mut add = vec!["add", "--"];
    add.extend(&names);
    crate::worktrees::manager::git(path, &add).await?;
    // Only these paths, whatever else is staged: the person's own work in
    // progress is not Eren's to commit. `-c` identity for this one commit,
    // as everywhere Eren commits — never written into the repository's config.
    let mut commit = vec![
        "-c",
        "user.name=eren",
        "-c",
        "user.email=eren@localhost",
        "commit",
        "-m",
        "Add the agent rules this project starts with",
        "--only",
        "--",
    ];
    commit.extend(&names);
    crate::worktrees::manager::git(path, &commit).await?;
    Ok(Seeded {
        written: names.iter().map(|n| n.to_string()).collect(),
        skipped: plan.skipped,
    })
}

struct Plan {
    files: Option<Vec<(&'static str, String)>>,
    skipped: Option<String>,
}

/// Which files to write, decided from what the repository already has. Pure,
/// for the tests.
fn plan(rules: &str, has_agents: bool, has_claude: bool) -> Plan {
    let rules = rules.trim();
    if rules.is_empty() {
        return Plan {
            files: None,
            skipped: None,
        };
    }
    if has_agents {
        // Their AGENTS.md is theirs; and a CLAUDE.md importing it would bring
        // in their text, not these rules — so nothing, said plainly.
        return Plan {
            files: None,
            skipped: Some(format!(
                "this repository already has {AGENTS_FILE}, so your rules were not added"
            )),
        };
    }
    let mut files = vec![(AGENTS_FILE, format!("{rules}\n"))];
    let skipped = if has_claude {
        Some(format!(
            "this repository already has {CLAUDE_FILE}; add the line `{}` to it for Claude Code to read your rules",
            CLAUDE_IMPORT.trim()
        ))
    } else {
        files.push((CLAUDE_FILE, CLAUDE_IMPORT.to_string()));
        None
    };
    Plan {
        files: Some(files),
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rules_writes_nothing_and_says_nothing() {
        let p = plan("  \n", false, false);
        assert!(p.files.is_none() && p.skipped.is_none());
    }

    #[test]
    fn a_fresh_repository_gets_both_files_and_claude_imports_the_other() {
        let p = plan("Use tabs.", false, false);
        let files = p.files.unwrap();
        assert_eq!(files[0], (AGENTS_FILE, "Use tabs.\n".to_string()));
        assert_eq!(files[1], (CLAUDE_FILE, "@AGENTS.md\n".to_string()));
        assert!(p.skipped.is_none());
    }

    #[test]
    fn a_file_the_repository_has_is_never_overwritten() {
        let p = plan("Use tabs.", true, false);
        assert!(p.files.is_none());
        assert!(p.skipped.unwrap().contains(AGENTS_FILE));

        let p = plan("Use tabs.", false, true);
        assert_eq!(p.files.unwrap().len(), 1);
        assert!(p.skipped.unwrap().contains("@AGENTS.md"));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    #[tokio::test]
    async fn the_local_rules_become_the_first_admins() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        set(db, None, "Prefer small diffs.").await.unwrap();
        assert_eq!(get(db, None).await.unwrap(), "Prefer small diffs.");
        let admin = crate::users::create_admin(db, "admin", "a long password")
            .await
            .unwrap();
        assert_eq!(
            get(db, Some(admin.id)).await.unwrap(),
            "Prefer small diffs."
        );
        assert_eq!(get(db, None).await.unwrap(), "");
        t.finish().await;
    }

    #[tokio::test]
    async fn a_new_repository_is_seeded_and_committed() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let dir = tempfile::tempdir().unwrap();
        crate::worktrees::manager::git(dir.path(), &["init", "-q"])
            .await
            .unwrap();
        // Something of the person's own, staged: it must not be committed.
        tokio::fs::write(dir.path().join("wip.txt"), "mine")
            .await
            .unwrap();
        crate::worktrees::manager::git(dir.path(), &["add", "wip.txt"])
            .await
            .unwrap();

        let ws: Uuid = sqlx::query_scalar("SELECT id FROM workspaces LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        set(db, None, "Prefer small diffs.").await.unwrap();
        let seeded = seed_project(db, ws, dir.path()).await;
        assert_eq!(seeded.written, vec![AGENTS_FILE, CLAUDE_FILE]);

        let committed =
            crate::worktrees::manager::git(dir.path(), &["show", "--name-only", "--format="])
                .await
                .unwrap();
        assert!(committed.contains(AGENTS_FILE) && committed.contains(CLAUDE_FILE));
        assert!(!committed.contains("wip.txt"), "{committed}");
        t.finish().await;
    }
}
