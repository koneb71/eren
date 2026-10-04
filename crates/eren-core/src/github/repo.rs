//! Which GitHub repository a project is.
//!
//! A project's identity has always been its local path, so every GitHub
//! feature re-derived the repository by running `git remote get-url origin` —
//! on every drawer render and every poll tick. This resolves it once and keeps
//! the answer on the row.

use super::GhError;
use crate::db::Db;
use uuid::Uuid;

/// A repository, as `gh` addresses one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    /// `github.com`, or an enterprise host. Lower-cased.
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    /// `owner/repo` — what `gh -R` takes and what the column stores.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// GitHub's own limit on a login.
const MAX_OWNER: usize = 39;
/// GitHub's own limit on a repository name.
const MAX_NAME: usize = 100;

/// Read whatever somebody pasted.
///
/// Accepts `owner/repo`, `https://host/owner/repo[.git][/anything]`,
/// `git@host:owner/repo.git`, and `ssh://git@host/owner/repo.git`.
///
/// ## Why a bare name is refused
///
/// `gh repo view cli` resolves against the *active account* — verified: it
/// answered about `koneb71/cli`. So the same field would mean different
/// repositories depending on who is signed in, which is the kind of ambiguity
/// that is only ever discovered by cloning the wrong thing.
///
/// ## Why the `url` crate is not used here
///
/// It would be worse, not merely unnecessary. `Url::parse` percent-decodes, so
/// `%2e%2e%2f` comes back as `../` — a traversal handed over after this
/// function had already refused the literal form. The defence is the charset,
/// as everywhere else identifiers travel in this codebase: `%` is simply not in
/// it, so an encoded path is refused rather than normalised. There is a test
/// that exists to say so.
pub fn parse_repo_ref(raw: &str) -> Result<RepoRef, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("paste a repository, like owner/repo or its URL".into());
    }

    // Strip the scheme and any credentials, leaving `host/owner/repo…` or, for
    // the scp-like git@host:owner/repo form, `host:owner/repo…`.
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .or_else(|| text.strip_prefix("ssh://"))
        .or_else(|| text.strip_prefix("git://"))
        .unwrap_or(text);
    let rest = rest.split_once('@').map_or(rest, |(_, after)| after);

    // `git@github.com:owner/repo` — the one form where the separator after the
    // host is a colon rather than a slash.
    let rest = if let Some((host, path)) = rest.split_once(':') {
        // Not a port: `host:owner/repo`. A numeric segment would be a port and
        // is not a shape GitHub URLs take, so treat it as unsupported rather
        // than guessing.
        if path.starts_with(|c: char| c.is_ascii_digit()) {
            return Err("a port is not part of a repository address".into());
        }
        format!("{host}/{path}")
    } else {
        rest.to_string()
    };

    let mut segments = rest.split('/').filter(|s| !s.is_empty());
    let (host, owner, name) = match (segments.next(), segments.next(), segments.next()) {
        // A full address: host, then owner, then repo.
        (Some(a), Some(b), Some(c)) => (a.to_ascii_lowercase(), b.to_string(), c.to_string()),
        // Just `owner/repo`, which means github.com.
        (Some(a), Some(b), None) => ("github.com".to_string(), a.to_string(), b.to_string()),
        _ => {
            return Err(format!(
                "\"{text}\" names one thing — a repository needs an owner too, like owner/repo"
            ))
        }
    };

    // Everything after `owner/repo` is ignored: a paste of `…/tree/trunk` or
    // `…/issues/42` means that repository, and refusing it is a papercut with
    // nothing behind it.

    // Exactly one `.git`, so `repo.git.git` keeps its odd but legal name.
    let name = name.strip_suffix(".git").unwrap_or(&name).to_string();

    // The host is checked too, and this is not ceremony. Without it
    // `--upload-pack=touch /tmp/x` parsed as host `--upload-pack=touch `,
    // owner `tmp`, name `x` — three segments, two of which are innocent — and
    // the flag rode through in the field nobody was looking at.
    check_host(&host)?;
    check(&owner, MAX_OWNER, false, "owner")?;
    check(&name, MAX_NAME, true, "repository name")?;
    Ok(RepoRef { host, owner, name })
}

/// A hostname, and nothing that could be read as anything else.
fn check_host(host: &str) -> Result<(), String> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && !host.starts_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(format!("\"{host}\" is not a host name"))
    }
}

/// The charset, which is the whole defence.
///
/// `dots_ok` because a repository may be called `.github` while an owner may
/// not. A leading `-` is refused in both: it is how an argument becomes a flag
/// (`-oProxyCommand=…`), and every one of these ends up in an argv.
fn check(part: &str, max: usize, dots_ok: bool, what: &str) -> Result<(), String> {
    let ok = !part.is_empty()
        && part.len() <= max
        && !part.starts_with('-')
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || (dots_ok && (c == '.' || c == '_')));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "\"{part}\" is not a {what} GitHub would accept — letters, digits and \
             dashes{}, at most {max} characters, and not starting with a dash",
            if dots_ok {
                ", dots and underscores"
            } else {
                ""
            }
        ))
    }
}

/// The repository this project is, resolving and remembering it if need be.
///
/// Lazily, at the point of use, rather than when a folder is added: a `gh`
/// spawn on every folder load would be a cost everybody pays for a feature most
/// projects do not use, and the answer would be wrong forever for a project
/// that gains a remote later. Once resolved it is a column read.
pub async fn resolve(db: &Db, project_id: Uuid) -> Option<String> {
    let row = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT github_repo, path FROM projects WHERE id = $1",
    )
    .bind(project_id)
    .fetch_optional(&db.pool)
    .await
    .ok()??;
    let (stored, path) = row;
    if stored.is_some() {
        return stored;
    }

    let url = crate::worktrees::manager::remote_url(std::path::Path::new(&path), "origin").await?;
    let slug = parse_repo_ref(&url).ok()?.slug();

    // A claim rather than a plain write: two callers resolving at once should
    // agree, and neither should overwrite an answer somebody set deliberately.
    let _ =
        sqlx::query("UPDATE projects SET github_repo = $2 WHERE id = $1 AND github_repo IS NULL")
            .bind(project_id)
            .bind(&slug)
            .execute(&db.pool)
            .await;
    Some(slug)
}

/// What `gh repo view --json` says about a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFacts {
    /// Canonical `owner/repo`, which resolves renames and redirects — so a
    /// stale URL stores the name the repository has now.
    pub slug: String,
    pub default_branch: String,
    /// No commits. Cloning one and handing it to `ensure_repo_state` would
    /// write an empty commit authored by Eren into somebody's repository.
    pub is_empty: bool,
    /// Anyone on the internet can open an issue here.
    pub public: bool,
    pub archived: bool,
}

pub const VIEW_FIELDS: &str = "nameWithOwner,defaultBranchRef,isEmpty,visibility,isArchived";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewJson {
    #[serde(default)]
    name_with_owner: String,
    #[serde(default)]
    default_branch_ref: Option<BranchRef>,
    #[serde(default)]
    is_empty: bool,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    is_archived: bool,
}

#[derive(serde::Deserialize)]
struct BranchRef {
    #[serde(default)]
    name: String,
}

/// Read `gh repo view --json …`.
///
/// `visibility` missing counts as **public**, which is the fail-closed choice:
/// the public path is the one that warns somebody that anyone can file an
/// issue, and defaulting to private would silently drop that warning.
pub fn parse_repo_view(json: &str) -> Result<RepoFacts, String> {
    let raw: ViewJson = serde_json::from_str(json)
        .map_err(|e| format!("could not read what gh reported about this repository: {e}"))?;
    if raw.name_with_owner.trim().is_empty() {
        return Err("gh reported a repository with no name".into());
    }
    Ok(RepoFacts {
        slug: raw.name_with_owner,
        // An empty repository has no default branch ref at all.
        default_branch: raw
            .default_branch_ref
            .map(|b| b.name)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "main".into()),
        is_empty: raw.is_empty,
        public: !raw.visibility.eq_ignore_ascii_case("PRIVATE")
            && !raw.visibility.eq_ignore_ascii_case("INTERNAL"),
        archived: raw.is_archived,
    })
}

/// Ask GitHub about a repository.
pub async fn view(repo: &RepoRef) -> Result<RepoFacts, GhError> {
    let slug = repo.slug();
    let out = super::gh(None, &["repo", "view", &slug, "--json", VIEW_FIELDS]).await?;
    parse_repo_view(&out).map_err(GhError::Failed)
}

// ── Cloning ─────────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokio::process::Child;

/// A clone still running.
struct Cloning {
    child: Child,
    /// Where it is being written, which is *not* where it will end up.
    temp: PathBuf,
    /// Where it will end up.
    destination: PathBuf,
    started: std::time::Instant,
    facts: RepoFacts,
    workspace_id: Uuid,
}

/// Long enough for a large repository on a slow line, short enough that a
/// wedged process is not tracked forever.
const CLONE_EXPIRES_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn clones() -> &'static Mutex<HashMap<Uuid, Cloning>> {
    static R: OnceLock<Mutex<HashMap<Uuid, Cloning>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How a clone is going.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
// `rename_all` renames the *variants*; the fields inside them need their own
// rule, or `project_id` reaches a browser expecting `projectId` and the clone
// appears to succeed into nothing.
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "state"
)]
pub enum CloneProgress {
    Cloning,
    /// Cloned, and the project row exists.
    Done {
        project_id: Uuid,
        path: String,
        name: String,
        github_repo: String,
        default_branch: String,
    },
    Failed {
        reason: String,
    },
}

/// The name a repository's folder should have.
///
/// Separate from `parse_repo_ref` because the two constrain different things:
/// `.github` is a legal repository name and an illegal folder name here, since
/// the browser hides dotfiles and the person would never find it again. The
/// refusal is shown rather than silently rewritten — a folder that quietly
/// became `github` is one they cannot correlate with anything.
pub fn destination_name(repo: &RepoRef) -> Result<&str, String> {
    let name = repo.name.as_str();
    if name.starts_with('.') {
        return Err(format!(
            "\"{name}\" would be a hidden folder, which the browser will not show \
             you — choose a different name for it"
        ));
    }
    Ok(name)
}

/// The folder a clone is written into before it is anything.
///
/// A killed login is harmless and you retry it; a killed clone leaves half a
/// repository on disk, which the folder browser would then offer as a project.
/// So the work happens under a dotted temporary name — hidden from the browser,
/// which already skips dotfiles — and the finished tree is moved into place in
/// one step. A rename within a directory is atomic, so the destination only
/// ever exists complete.
fn temp_for(parent: &Path, id: Uuid) -> PathBuf {
    parent.join(format!(".eren-cloning-{id}"))
}

/// Begin a clone. Returns as soon as it has started, never when it finishes.
///
/// `gh repo clone`, not `git clone`, and the reason is the same one
/// `connect.rs` documents: `worktrees::manager::git` sets no stdin, so an
/// HTTPS clone of a private repository prompts for a password against a
/// console that does not exist and hangs forever. `gh` is already signed in,
/// already sets `stdin(null)`, and honours the person's own git protocol.
pub async fn start_clone(
    repo: &RepoRef,
    parent: &Path,
    name: &str,
    workspace_id: Uuid,
) -> Result<(Uuid, PathBuf), String> {
    let facts = view(repo).await.map_err(|e| e.to_string())?;

    // Refused here rather than after cloning, because the repair is worse than
    // the refusal: `ensure_repo_state` writes an *empty* commit authored by
    // Eren into a repository that has a remote, which is Eren inventing
    // history in somebody's GitHub repository.
    if facts.is_empty {
        return Err(format!(
            "{} has no commits yet, so there is nothing to clone — push a first \
             commit and try again",
            facts.slug
        ));
    }

    let destination = parent.join(name);
    if destination.exists() {
        return Err(format!("\"{name}\" already exists here"));
    }

    let id = Uuid::new_v4();
    let temp = temp_for(parent, id);
    let child = super::spawn_gh(&[
        "repo",
        "clone",
        &facts.slug,
        temp.to_str()
            .ok_or("that folder's name is not valid text")?,
    ])
    .map_err(|e| e.to_string())?;

    clones().lock().unwrap().insert(
        id,
        Cloning {
            child,
            temp,
            destination: destination.clone(),
            started: std::time::Instant::now(),
            facts,
            workspace_id,
        },
    );
    Ok((id, destination))
}

/// Where a clone has got to, finishing it if it is done.
pub async fn poll_clone(db: &Db, id: Uuid) -> CloneProgress {
    let finished = {
        let mut reg = clones().lock().unwrap();
        let Some(pending) = reg.get_mut(&id) else {
            return CloneProgress::Failed {
                reason: "eren is no longer tracking this clone — it may have been \
                         cancelled, or the server restarted"
                    .into(),
            };
        };
        if pending.started.elapsed() > CLONE_EXPIRES_AFTER {
            let mut done = reg.remove(&id).unwrap();
            let _ = done.child.start_kill();
            let _ = std::fs::remove_dir_all(&done.temp);
            return CloneProgress::Failed {
                reason: "this clone took longer than half an hour and was given up on".into(),
            };
        }
        match pending.child.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) => None,
            Err(e) => {
                let done = reg.remove(&id).unwrap();
                let _ = std::fs::remove_dir_all(&done.temp);
                return CloneProgress::Failed {
                    reason: format!("could not tell whether the clone finished: {e}"),
                };
            }
        }
    };
    let Some(status) = finished else {
        return CloneProgress::Cloning;
    };

    let done = clones().lock().unwrap().remove(&id);
    let Some(done) = done else {
        // Another poll won the race and already settled it.
        return CloneProgress::Cloning;
    };

    if !status.success() {
        let _ = tokio::fs::remove_dir_all(&done.temp).await;
        return CloneProgress::Failed {
            reason: format!(
                "gh could not clone {} — check the name and that you have access to it",
                done.facts.slug
            ),
        };
    }

    settle(db, done).await
}

/// Move the finished clone into place and record the project.
async fn settle(db: &Db, done: Cloning) -> CloneProgress {
    // Belt and braces for the empty repository. `gh repo view` said it was not
    // empty, but that answer is a moment old and this is the check that keeps
    // Eren from writing a commit into somebody's repository — cheap, local,
    // and already `pub`.
    if crate::worktrees::manager::head(&done.temp).await.is_none() {
        let _ = tokio::fs::remove_dir_all(&done.temp).await;
        return CloneProgress::Failed {
            reason: format!(
                "{} came back with no commits, so Eren has not kept it — a project \
                 needs something to branch from",
                done.facts.slug
            ),
        };
    }

    if let Err(e) = tokio::fs::rename(&done.temp, &done.destination).await {
        let _ = tokio::fs::remove_dir_all(&done.temp).await;
        return CloneProgress::Failed {
            reason: format!("the clone finished but could not be moved into place: {e}"),
        };
    }

    let path = done.destination.to_string_lossy().to_string();
    let name = done
        .destination
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| done.facts.slug.clone());

    // Recorded here rather than by the browser, so somebody who navigates away
    // mid-clone does not end up with a folder Eren made and nothing knows
    // about. `ON CONFLICT` because two polls can see the same exit.
    //
    // Deliberately not through `POST /api/projects`: that route has no sandbox,
    // and it calls `ensure_repo_state`, which is the very thing that would
    // write a commit into a fresh clone. A clone already knows it is a git
    // repository and has the real default branch from `gh` — better
    // information than that function could produce.
    let inserted = sqlx::query_as::<_, (Uuid, bool)>(
        "INSERT INTO projects (path, name, default_branch, workspace_id, vcs, github_repo)
         VALUES ($1, $2, $3, $4, 'git', $5)
         ON CONFLICT (path) DO UPDATE
            SET name = EXCLUDED.name, github_repo = EXCLUDED.github_repo
         RETURNING id, (xmax = 0) AS fresh",
    )
    .bind(&path)
    .bind(&name)
    .bind(&done.facts.default_branch)
    .bind(done.workspace_id)
    .bind(&done.facts.slug)
    .fetch_one(&db.pool)
    .await;

    match inserted {
        Ok((project_id, fresh)) => {
            // The owner's rules, into a project that is new — once, on the
            // poll that created the row, not on every poll that sees the exit.
            if fresh {
                let seeded =
                    crate::rules::seed_project(db, done.workspace_id, &done.destination).await;
                if !seeded.written.is_empty() || seeded.skipped.is_some() {
                    tracing::info!(path = %path, written = ?seeded.written, skipped = ?seeded.skipped, "rules for the new project");
                }
            }
            CloneProgress::Done {
                project_id,
                path,
                name,
                github_repo: done.facts.slug,
                default_branch: done.facts.default_branch,
            }
        }
        Err(e) => CloneProgress::Failed {
            // The folder is left alone: it is a complete clone, and the person
            // can add it by hand. Deleting somebody's freshly cloned code
            // because a database write failed would be the wrong repair.
            reason: format!(
                "cloned into {path}, but the project could not be recorded: {e} — \
                 add the folder by hand"
            ),
        },
    }
}

/// Give up on a clone and leave nothing behind.
pub async fn cancel_clone(id: Uuid) {
    let done = clones().lock().unwrap().remove(&id);
    if let Some(mut done) = done {
        let _ = done.child.start_kill();
        let _ = done.child.wait().await;
        let _ = tokio::fs::remove_dir_all(&done.temp).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> RepoRef {
        parse_repo_ref(raw).unwrap_or_else(|e| panic!("{raw} should parse: {e}"))
    }

    #[test]
    fn the_shapes_a_person_actually_pastes_all_work() {
        let want = RepoRef {
            host: "github.com".into(),
            owner: "cli".into(),
            name: "cli".into(),
        };
        assert_eq!(ok("cli/cli"), want);
        assert_eq!(ok("https://github.com/cli/cli"), want);
        assert_eq!(ok("https://github.com/cli/cli.git"), want);
        assert_eq!(ok("https://github.com/cli/cli/"), want);
        assert_eq!(ok("git@github.com:cli/cli.git"), want);
        assert_eq!(ok("ssh://git@github.com/cli/cli.git"), want);
        // A paste from the address bar while reading an issue.
        assert_eq!(ok("https://github.com/cli/cli/issues/42"), want);
        assert_eq!(ok("https://github.com/cli/cli/tree/trunk"), want);
        // Whitespace from a copy.
        assert_eq!(ok("  cli/cli  "), want);
        // The host is normalised, since it is compared against gh's accounts.
        assert_eq!(ok("https://GitHub.com/cli/cli").host, "github.com");
    }

    #[test]
    fn a_bare_name_is_refused_because_gh_would_guess_an_owner() {
        // Verified against gh 2.96.0: `gh repo view cli` answered about
        // `koneb71/cli` — the signed-in account's namespace. Accepting this
        // would make the same input mean different repositories per login.
        let e = parse_repo_ref("cli").unwrap_err();
        assert!(e.contains("owner"), "{e}");
        assert!(parse_repo_ref("https://github.com/cli").is_err());
    }

    #[test]
    fn an_enterprise_host_is_carried_rather_than_assumed_away() {
        let r = ok("https://ghe.example.com/team/thing");
        assert_eq!(r.host, "ghe.example.com");
        assert_eq!(r.slug(), "team/thing");
    }

    /// Every one of these ends up in an argv. The charset is the defence.
    #[test]
    fn nothing_that_could_be_a_path_or_a_flag_gets_through() {
        for hostile in [
            "../..",
            "https://github.com/../..",
            "..%2f..",
            "-oProxyCommand=curl evil.example",
            "--upload-pack=touch /tmp/pwned",
            "cli/-repo",
            "-cli/cli",
            "cli/re po",
            "cli/repo;rm -rf /",
            "cli/repo$(id)",
            "",
            "   ",
            "/",
            "//",
        ] {
            assert!(
                parse_repo_ref(hostile).is_err(),
                "{hostile:?} was accepted, and it reaches an argv"
            );
        }
    }

    /// The test that says why the `url` crate is not a dependency.
    #[test]
    fn percent_encoding_is_refused_rather_than_decoded() {
        // `Url::parse` would turn this back into `../`, handing over a
        // traversal that the literal form was just refused for. `%` is not in
        // the charset, so it never becomes anything.
        assert!(parse_repo_ref("https://github.com/%2e%2e/%2e%2e").is_err());
        assert!(parse_repo_ref("cli/%2e%2e").is_err());
    }

    #[test]
    fn a_repository_may_be_called_dot_github_even_though_an_owner_may_not() {
        // `cli/.github` is a real repository.
        assert_eq!(ok("cli/.github").name, ".github");
        assert!(
            parse_repo_ref(".cli/thing").is_err(),
            "an owner may not start with a dot"
        );
    }

    #[test]
    fn only_one_dot_git_comes_off() {
        assert_eq!(ok("cli/repo.git.git").name, "repo.git");
        assert_eq!(ok("cli/repo.git").name, "repo");
        // A name that merely contains "git" is untouched.
        assert_eq!(ok("cli/gitignore").name, "gitignore");
    }

    #[test]
    fn a_name_longer_than_github_allows_is_refused_here_rather_than_there() {
        assert!(parse_repo_ref(&format!("{}/repo", "a".repeat(40))).is_err());
        assert!(parse_repo_ref(&format!("owner/{}", "a".repeat(101))).is_err());
        assert!(parse_repo_ref(&format!("{}/repo", "a".repeat(39))).is_ok());
    }

    /// Real output, captured from gh 2.96.0.
    #[test]
    fn what_gh_says_about_a_repository_is_read_as_it_meant_it() {
        let json = r#"{"defaultBranchRef":{"name":"trunk"},"isArchived":false,
            "isEmpty":false,"nameWithOwner":"cli/cli","visibility":"PUBLIC"}"#;
        let facts = parse_repo_view(json).unwrap();
        assert_eq!(facts.slug, "cli/cli");
        // Not "main". This is the case a hardcoded default gets wrong.
        assert_eq!(facts.default_branch, "trunk");
        assert!(!facts.is_empty);
        assert!(facts.public);
        assert!(!facts.archived);
    }

    #[test]
    fn an_empty_repository_says_so_and_has_no_branch_to_report() {
        let json = r#"{"defaultBranchRef":null,"isArchived":false,"isEmpty":true,
            "nameWithOwner":"me/fresh","visibility":"PRIVATE"}"#;
        let facts = parse_repo_view(json).unwrap();
        assert!(facts.is_empty);
        assert!(!facts.public);
        assert_eq!(facts.default_branch, "main", "a fallback, not a claim");
    }

    #[test]
    fn a_repository_whose_visibility_is_missing_is_treated_as_public() {
        // Fail-closed: the public path is the one that warns somebody anyone
        // can file an issue here, so silence must not drop the warning.
        let json = r#"{"nameWithOwner":"a/b","defaultBranchRef":{"name":"main"}}"#;
        assert!(parse_repo_view(json).unwrap().public);
    }

    #[test]
    fn nonsense_is_an_error_rather_than_a_repository_that_looks_fine() {
        assert!(parse_repo_view("not json").is_err());
        assert!(parse_repo_view(r#"{"isEmpty":false}"#).is_err());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    /// The browser reads these keys by name. `rename_all` renames variants and
    /// not their fields, so without `rename_all_fields` a clone answered with
    /// `project_id` to a page looking for `projectId` — succeeding into
    /// nothing, which is the worst way to fail.
    #[test]
    fn progress_speaks_the_shape_the_browser_reads() {
        let json = serde_json::to_string(&CloneProgress::Done {
            project_id: Uuid::nil(),
            path: "/tmp/x".into(),
            name: "x".into(),
            github_repo: "cli/cli".into(),
            default_branch: "trunk".into(),
        })
        .unwrap();
        assert!(json.contains("\"state\":\"done\""), "{json}");
        assert!(json.contains("\"projectId\""), "{json}");
        assert!(json.contains("\"githubRepo\""), "{json}");
        assert!(json.contains("\"defaultBranch\""), "{json}");
        assert!(!json.contains("project_id"), "{json}");

        let json = serde_json::to_string(&CloneProgress::Cloning).unwrap();
        assert_eq!(json, "{\"state\":\"cloning\"}");
        let json = serde_json::to_string(&CloneProgress::Failed {
            reason: "no".into(),
        })
        .unwrap();
        assert!(json.contains("\"state\":\"failed\"") && json.contains("\"reason\":\"no\""));
    }
}
