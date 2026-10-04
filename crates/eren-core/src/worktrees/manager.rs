//! Git worktree lifecycle for task isolation. Worktrees live under
//! `~/.eren/worktrees/<project-hash>/<task-id>` — outside the user's repo,
//! so agent runs never dirty the main checkout. All git invocations use
//! explicit arg vectors (never shell strings).

use eren_shared::env_guard;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct WorktreeManager {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// One file's share of a diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileStat {
    pub path: String,
    pub added: i64,
    pub removed: i64,
    pub binary: bool,
}

/// `added<TAB>removed<TAB>path`, with `-` for both counts on a binary file.
fn parse_numstat(line: &str) -> Option<FileStat> {
    let mut parts = line.splitn(3, '\t');
    let added = parts.next()?;
    let removed = parts.next()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(FileStat {
        path: path.to_string(),
        added: added.parse().unwrap_or(0),
        removed: removed.parse().unwrap_or(0),
        binary: added == "-" && removed == "-",
    })
}

/// Every branch Eren creates starts with this, and nothing else it deletes
/// may be missing it — or the prefix it had before the rename (see
/// [`eren_shared::brand::is_card_branch`]).
pub use eren_shared::brand::BRANCH_PREFIX;

impl WorktreeManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn default_root() -> PathBuf {
        eren_shared::brand::home().join("worktrees")
    }

    /// True when `path` is inside this manager's root — the gate FullAuto
    /// runs must pass.
    pub fn manages(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// True when this worktree is one Eren made: under its root, on its
    /// branch prefix. Both, because either alone can be true of a person's
    /// own worktree by coincidence, and the answer decides what gets deleted.
    ///
    /// Compared canonically as well as literally. git reports the path it
    /// resolved, and on macOS a root under `/var` comes back as `/private/var`
    /// — the literal comparison alone silently matches nothing.
    fn owns(&self, path: &Path, branch: &str) -> bool {
        if !eren_shared::brand::is_card_branch(branch) {
            return false;
        }
        if self.manages(path) {
            return true;
        }
        match (
            std::fs::canonicalize(&self.root),
            std::fs::canonicalize(path),
        ) {
            (Ok(root), Ok(path)) => path.starts_with(root),
            _ => false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory a project's worktrees live in, by name.
    ///
    /// Exposed so the sweep can work out which directories under the root
    /// belong to a project that still exists — the only way to recognise the
    /// leftovers of an uninstalled app, whose repository is gone along with any
    /// chance of asking git about them.
    pub fn project_dir_name(repo_path: &str) -> String {
        short_hash(repo_path)
    }

    pub async fn create(
        &self,
        repo: &Path,
        base_branch: &str,
        task_id: Uuid,
        slug: &str,
    ) -> anyhow::Result<Worktree> {
        let project_hash = short_hash(&repo.to_string_lossy());
        let path = self.root.join(project_hash).join(task_id.to_string());
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        let branch = format!("{BRANCH_PREFIX}{slug}-{}", &task_id.to_string()[..8]);

        // A repository added before we checked for commits — or one the user
        // init'd themselves — has an unborn HEAD and nothing to branch from.
        // Writing the root commit is the same thing we do when we create a
        // repo, and the only alternative is failing every run here.
        if !has_commits(repo).await {
            tracing::warn!(
                repo = %repo.display(),
                "repository has no commits; writing an initial commit so runs can be isolated"
            );
            commit_everything(repo).await?;
        }
        let base = resolve_base(repo, base_branch).await?;
        git(
            repo,
            &[
                "worktree",
                "add",
                path.to_str().unwrap(),
                "-b",
                &branch,
                &base,
            ],
        )
        .await?;
        Ok(Worktree { path, branch })
    }

    /// Unified diff of everything the agent changed (committed + uncommitted)
    /// since the card's branch left the base — see `diff_base`.
    pub async fn diff(&self, worktree: &Path, base_branch: &str) -> anyhow::Result<String> {
        intend_to_add(worktree).await?;
        let base = diff_base(worktree, base_branch).await?;
        git(worktree, &["diff", &base]).await
    }

    /// One line per changed file, with counts.
    ///
    /// `--numstat` rather than parsing a unified diff: it gives the counts
    /// directly and reports a binary file as `-` for both, so "binary" is read
    /// from git rather than guessed. Telling a reader a PNG changed by zero
    /// lines is worse than telling them nothing.
    ///
    /// Like `diff`, this runs `git add -N .` so untracked files count — which
    /// writes the *worktree's* index. Never the user's checkout, and no
    /// different from what viewing a diff already does.
    pub async fn diff_stat(
        &self,
        worktree: &Path,
        base_branch: &str,
    ) -> anyhow::Result<Vec<FileStat>> {
        intend_to_add(worktree).await?;
        let base = diff_base(worktree, base_branch).await?;
        let out = git(worktree, &["diff", "--numstat", &base]).await?;
        Ok(out.lines().filter_map(parse_numstat).collect())
    }

    /// One file's diff. The path is a separate argument, never interpolated —
    /// `git` takes an argument vector and builds no shell string.
    pub async fn diff_file(
        &self,
        worktree: &Path,
        base_branch: &str,
        path: &str,
    ) -> anyhow::Result<String> {
        intend_to_add(worktree).await?;
        let base = diff_base(worktree, base_branch).await?;
        git(worktree, &["diff", &base, "--", path]).await
    }

    /// Squash-merge the worktree branch back onto the base branch in the
    /// main repo. Returns Err with the git output on conflicts.
    pub async fn squash_merge(
        &self,
        repo: &Path,
        worktree: &Worktree,
        base_branch: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        // Commit any uncommitted agent work in the worktree first.
        commit_worktree(worktree, message).await?;

        // Conflict markers committed into the branch — by an agent resolving
        // a conflict that ran `git commit` itself, or by hand — would land on
        // the base as text. `commit_worktree` catches the ones still unmerged;
        // this catches the ones that are not.
        let marked = conflict_markers(&self.diff(&worktree.path, base_branch).await?);
        if !marked.is_empty() {
            return Err(MergeRefusal::Markers { files: marked }.into());
        }

        // Everything below this line runs in the **user's own checkout**, so
        // it has to find one it is allowed to move.
        //
        // Without this check, `git checkout <base>` either refused — a 409 the
        // user could do nothing with, because nothing said which files — or,
        // worse, carried their uncommitted edits across onto the base branch
        // and folded them into this card's commit. `merge --squash` stages
        // into the same index those files sit in, so the message
        // `eren: {title}` ended up on work an agent never touched. Already
        // being on the base branch with something staged is the same bug
        // wearing a different hat: `checkout` is a no-op, the squash piles on
        // top, and the `diff --cached` below sees the person's own work.
        //
        // Untracked files are deliberately not counted. `checkout` does not
        // carry them and `merge --squash` does not stage them, so they are
        // harmless — while counting them would make any repository with
        // untracked build output permanently unmergeable.
        let dirty = git(repo, &["status", "--porcelain", "--untracked-files=no"]).await?;
        if !dirty.trim().is_empty() {
            return Err(MergeRefusal::Dirty {
                message: format!(
                    "your checkout at {} has uncommitted changes, and merging would \
                     check out {base_branch} over them and fold them into this card's \
                     commit — commit or stash them and merge again:\n{}",
                    repo.display(),
                    describe_dirty(&dirty)
                ),
            }
            .into());
        }

        // Where the person was standing, so we can put them back.
        //
        // Everything from here on moves their checkout, and the two ways out of
        // this function used to leave it somewhere else: a success left them on
        // the base branch with no notice, and a conflict left them there *and*
        // holding conflict markers and a conflicted index. `git merge --abort`
        // does not undo a `--squash` — there is no `MERGE_HEAD` — so the
        // recovery is `reset --merge`, which nothing told them. The next Merge
        // click then hit the dirty guard and reported the debris as if it were
        // their own work.
        let was_on = current_branch(repo).await;
        let base = resolve_base(repo, base_branch).await?;
        git(repo, &["checkout", &base]).await?;

        let merged = git(repo, &["merge", "--squash", &worktree.branch]).await;

        // A squash that staged nothing means this branch is already in the
        // base — a second click on Merge, or work that was merged by hand.
        // `git commit` exits non-zero for that, so committing unconditionally
        // turned a successful, already-done merge into an error alert while
        // the change sat safely on the branch. Merging twice should be quiet.
        let outcome = match merged {
            Err(e) => Err(e),
            Ok(_) if git(repo, &["diff", "--cached", "--quiet"]).await.is_ok() => {
                tracing::info!(
                    branch = %worktree.branch,
                    "nothing to merge — the branch is already in {base}"
                );
                Ok(())
            }
            Ok(_) => git(repo, &["commit", "-m", message]).await.map(|_| ()),
        };

        match outcome {
            Ok(()) => {
                restore_checkout(repo, was_on.as_deref(), &base, false).await;
                Ok(())
            }
            Err(e) => {
                restore_checkout(repo, was_on.as_deref(), &base, true).await;
                // The recovery is named in the error, not just performed, so a
                // person who goes to look at their repository knows what was
                // already done on their behalf. The files, parsed out of git's
                // own report, are what lets the dashboard offer to bring the
                // base into the branch and have the conflict resolved there.
                let detail = e.to_string();
                Err(MergeRefusal::Conflict {
                    files: conflicted_in(&detail),
                    message: format!(
                        "{detail}\n\nYour checkout was put back on {} and the half-finished \
                         merge was cleared, so nothing is left staged. Bring {base} into the \
                         card's branch to resolve the conflict there, then merge again.",
                        was_on.as_deref().unwrap_or(&base)
                    ),
                }
                .into())
            }
        }
    }

    /// Bring the base branch into the card's branch, inside the card's own
    /// worktree.
    ///
    /// This is the way out of a merge conflict that `squash_merge` refuses: the
    /// conflict is met on the card's branch, where an agent can resolve it and
    /// a person can read the resolution in the same diff, instead of in the
    /// person's checkout. It decides no question of "whose history wins" —
    /// the rule `pull_ff` keeps — because the branch is Eren's own
    /// (`eren/…`) and is squashed onto the base when it lands.
    ///
    /// A conflicted merge is left in progress, on purpose: the conflict markers
    /// are the brief. `commit_worktree` refuses to commit while any remain.
    pub async fn update_from_base(
        &self,
        worktree: &Worktree,
        base_branch: &str,
        message: &str,
    ) -> anyhow::Result<BaseUpdate> {
        if merging(&worktree.path).await {
            return Ok(BaseUpdate::Conflicted {
                files: unmerged_files(&worktree.path).await?,
            });
        }
        commit_worktree(worktree, message).await?;
        let base = resolve_base(&worktree.path, base_branch).await?;
        if behind(&worktree.path, &base).await? == 0 {
            return Ok(BaseUpdate::UpToDate);
        }
        match git(&worktree.path, &["merge", "--no-edit", &base]).await {
            Ok(_) => Ok(BaseUpdate::Merged),
            Err(e) => {
                let files = unmerged_files(&worktree.path).await?;
                if files.is_empty() {
                    // Failed for some other reason; leave nothing half-done.
                    let _ = git(&worktree.path, &["merge", "--abort"]).await;
                    return Err(e);
                }
                Ok(BaseUpdate::Conflicted { files })
            }
        }
    }

    /// Commits on the base branch that the card's branch does not have yet.
    pub async fn behind_base(&self, worktree: &Path, base_branch: &str) -> anyhow::Result<u32> {
        let base = resolve_base(worktree, base_branch).await?;
        behind(worktree, &base).await
    }

    /// Finish a merge an agent resolved: commit it, unless markers remain.
    /// `Ok(false)` when there was no merge in progress.
    pub async fn conclude_merge(&self, worktree: &Worktree, message: &str) -> anyhow::Result<bool> {
        if !merging(&worktree.path).await {
            return Ok(false);
        }
        commit_worktree(worktree, message).await?;
        Ok(true)
    }

    /// Is a merge of the base into this worktree's branch still in progress,
    /// and in which files?
    pub async fn merge_in_progress(&self, worktree: &Path) -> Option<Vec<String>> {
        if merging(worktree).await {
            Some(unmerged_files(worktree).await.unwrap_or_default())
        } else {
            None
        }
    }

    /// Send this card's branch to `origin`, so a pull request has something to
    /// point at.
    ///
    /// Note what this does **not** take: a path to the user's repository. It
    /// runs entirely in the worktree, which is why the dirty-checkout guard on
    /// [`Self::squash_merge`] neither applies here nor should. Opening a pull
    /// request while you have unrelated work in progress is the ordinary state
    /// of a working checkout; merging over it is not. Anyone tempted to hoist
    /// that guard into a shared precondition would be removing the difference.
    ///
    /// Uncommitted agent work is committed first, for the reason it is on the
    /// merge path too: a card in review can hold changes the diff showed and
    /// nothing has committed, and a pull request opened without them shows a
    /// reviewer something other than what was approved.
    ///
    /// `--set-upstream` so a later `gh pr create --head` finds the branch, and
    /// `origin` by name rather than "the first remote", which in a fork would
    /// silently pick `upstream` and try to push to somebody else's repository.
    pub async fn push(
        &self,
        worktree: &Worktree,
        message: &str,
        force: bool,
    ) -> anyhow::Result<()> {
        commit_worktree(worktree, message).await?;

        // Its absence *is* the condition — there is nowhere to push, which is
        // a thing to tell somebody rather than a failure to log.
        if git(&worktree.path, &["remote", "get-url", "origin"])
            .await
            .is_err()
        {
            anyhow::bail!(
                "this project has no `origin` remote, so there is nowhere to push \
                 the branch — add one and try again"
            );
        }

        let mut args = vec!["push", "--set-upstream", "origin", &worktree.branch];
        if force {
            // `--force-with-lease` and never `--force`: it refuses if the
            // remote moved since we last saw it, so the second click can still
            // only overwrite the history it was shown.
            args.insert(1, "--force-with-lease");
        }
        match git(&worktree.path, &args).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let text = e.to_string();
                if let Some(why) = push_rejection(&text) {
                    anyhow::bail!("{why}");
                }
                Err(e)
            }
        }
    }

    /// Remove a worktree known only by its path, taking its branch with it.
    ///
    /// `remove` needs a `Worktree` value; a losing bake-off variant is a row
    /// in the database holding a path, so the branch is read back off disk.
    pub async fn discard(&self, repo: &Path, path: &Path) -> anyhow::Result<()> {
        let branch = current_branch(path).await.unwrap_or_default();
        self.remove(
            repo,
            &Worktree {
                path: path.to_path_buf(),
                branch,
            },
        )
        .await
    }

    pub async fn remove(&self, repo: &Path, worktree: &Worktree) -> anyhow::Result<()> {
        git(
            repo,
            &[
                "worktree",
                "remove",
                "--force",
                worktree.path.to_str().unwrap(),
            ],
        )
        .await?;
        let _ = git(repo, &["branch", "-D", &worktree.branch]).await;
        Ok(())
    }
}

/// Put the user's checkout back where it was.
///
/// Best-effort and logged rather than fallible: a merge that actually landed
/// must not be reported as a failure because a `checkout` afterwards did not
/// work, and a merge that failed already has an error worth more than this one.
///
/// `clear` runs `reset --merge` first, which is the only thing that undoes a
/// conflicted `merge --squash`. It is deliberately not run on the success path
/// — there is a commit there we have no business touching.
async fn restore_checkout(repo: &Path, was_on: Option<&str>, base: &str, clear: bool) {
    if clear {
        if let Err(e) = git(repo, &["reset", "--merge"]).await {
            tracing::warn!(repo = %repo.display(), error = %e, "could not clear the failed merge");
        }
    }
    let Some(branch) = was_on else { return };
    if branch == base {
        return;
    }
    if let Err(e) = git(repo, &["checkout", branch]).await {
        tracing::warn!(
            repo = %repo.display(), %branch, error = %e,
            "merged, but could not put the checkout back on the branch it came from"
        );
    }
}

/// What a worktree should branch from.
///
/// A project row outlives the branch it names — the branch gets renamed, or
/// the row recorded a default before the repo actually had one — and `git
/// worktree add <path> -b <new> <base>` fails outright on a base it can't
/// resolve. Falling back to whatever the repo is actually on beats failing
/// the run over a stale row.
async fn resolve_base(repo: &Path, preferred: &str) -> anyhow::Result<String> {
    if branch_exists(repo, preferred).await {
        return Ok(preferred.to_string());
    }
    if let Some(actual) = current_branch(repo).await {
        if branch_exists(repo, &actual).await {
            tracing::warn!(
                repo = %repo.display(),
                %preferred, %actual,
                "recorded base branch is missing; using the repository's current branch"
            );
            return Ok(actual);
        }
    }
    if has_commits(repo).await {
        return Ok("HEAD".to_string());
    }
    anyhow::bail!(
        "this repository has no commits yet, so there is nothing to branch from — \
         make one commit in it and start the run again"
    )
}

async fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    )
    .await
    .is_ok()
}

/// The commit a repository is on. `None` before the first one.
///
/// Public because an app's build records where its branch stood before the
/// build landed — which is the only thing that makes the undo real. Kept here
/// rather than in `apps` so every `git` invocation still goes through one
/// function with one error format.
pub async fn head(path: &Path) -> Option<String> {
    git(path, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Move the checked-out branch back to `commit`, discarding what came after.
///
/// Genuinely destructive, and only ever called with a commit this repository
/// recorded itself. It exists for exactly one caller: undoing an app build that
/// landed automatically. `--hard` rather than `revert` because the promise made
/// at the point of landing is "put it back how it was", and a revert commit on
/// top leaves the folder holding a history nobody asked to read.
pub async fn reset_hard(repo: &Path, commit: &str) -> anyhow::Result<()> {
    git(repo, &["reset", "--hard", commit]).await?;
    Ok(())
}

/// The checked-out branch, which resolves even before the first commit.
/// `None` when HEAD is detached.
pub async fn current_branch(path: &Path) -> Option<String> {
    git(path, &["symbolic-ref", "--short", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn has_commits(path: &Path) -> bool {
    git(path, &["rev-parse", "--verify", "HEAD"]).await.is_ok()
}

/// How a project directory is (or isn't) version controlled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vcs {
    /// A usable repository — runs get an isolated worktree, diff, and merge.
    Git,
    /// No repository, with the reason. Runs happen in the directory itself.
    None(String),
}

/// Make `path` usable as a project, initializing a repository if it needs one.
///
/// The worktree is what keeps an agent out of the user's files and what makes
/// review possible, so it is worth creating a repo to get one. When that
/// genuinely can't happen we say why rather than refusing the project.
pub async fn ensure_repo(path: &Path, default_branch: &str) -> Vcs {
    ensure_repo_state(path, default_branch).await.vcs
}

/// How a project ended up version controlled, plus the branch its runs
/// should actually branch from — which is the repository's own branch, not
/// whatever default the caller guessed.
pub struct RepoState {
    pub vcs: Vcs,
    pub branch: String,
}

pub async fn ensure_repo_state(path: &Path, default_branch: &str) -> RepoState {
    if path.join(".git").exists() {
        let branch = current_branch(path)
            .await
            .unwrap_or_else(|| default_branch.to_string());

        // An existing repository with no commits has an unborn HEAD, which
        // nothing can branch from. Being a repo isn't enough to be usable.
        if !has_commits(path).await {
            if let Err(e) = commit_everything(path).await {
                return RepoState {
                    vcs: Vcs::None(format!(
                        "the repository here has no commits and one could not be \
                         created ({e}) — commit something and re-add the folder"
                    )),
                    branch,
                };
            }
        }
        return RepoState {
            vcs: Vcs::Git,
            branch,
        };
    }
    ensure_repo_uninitialized(path, default_branch).await
}

async fn ensure_repo_uninitialized(path: &Path, default_branch: &str) -> RepoState {
    let branch = default_branch.to_string();

    // A folder inside another repository is already tracked by it. Running
    // `git init` here would nest a second repo inside the first, which
    // confuses every later git call for no benefit.
    if let Ok(top) = git(path, &["rev-parse", "--show-toplevel"]).await {
        let top = top.trim();
        if !top.is_empty() {
            return RepoState {
                vcs: Vcs::None(format!(
                    "inside the repository at {top} — add that folder instead to get \
                     isolated worktrees and reviewable diffs"
                )),
                branch,
            };
        }
    }

    match init_repo(path, default_branch).await {
        Ok(()) => RepoState {
            vcs: Vcs::Git,
            branch,
        },
        Err(e) => RepoState {
            vcs: Vcs::None(format!("could not initialize a repository here: {e}")),
            branch,
        },
    }
}

/// `git init` plus a real first commit.
///
/// The commit is not optional and must include existing files: `git worktree
/// add <path> -b <branch> <base>` resolves `<base>` to a commit, so an unborn
/// HEAD fails outright and an *empty* first commit would hand the agent a
/// worktree with none of the user's files in it.
async fn init_repo(path: &Path, default_branch: &str) -> anyhow::Result<()> {
    git(path, &["init", "-b", default_branch]).await?;
    commit_everything(path).await
}

/// Stage everything and write a root commit, giving an unborn HEAD something
/// to branch from. Existing files are included deliberately: an empty first
/// commit would hand the agent a worktree with none of the user's work in it.
async fn commit_everything(path: &Path) -> anyhow::Result<()> {
    commit_all(path, "Initial commit").await.map(|_| ())
}

/// Commit whatever is in a repository's working tree.
///
/// Public for apps, whose folder Eren writes to directly: a manifest written
/// but never committed is invisible to `git worktree add`, so the next agent
/// asked to change the app opens an **empty folder** — which is exactly what
/// happened before this existed. Committing on write is what makes the branch
/// and the folder the same thing.
///
/// Returns whether there was anything to commit, so a caller writing the file
/// that is already committed (a landed merge, a revert) is a quiet no-op rather
/// than an empty commit per save.
pub async fn commit_all(path: &Path, message: &str) -> anyhow::Result<bool> {
    git(path, &["add", "-A"]).await?;
    // An unborn HEAD has nothing to diff against, so the first commit is made
    // unconditionally — `--allow-empty` is what lets a repo become branchable
    // before it has any files at all.
    let first = !has_commits(path).await;
    if !first && git(path, &["diff", "--cached", "--quiet"]).await.is_ok() {
        return Ok(false);
    }
    // `-c` rather than `config`: identity is needed for this one commit and
    // writing it into the user's repo config would be presumptuous.
    git(
        path,
        &[
            "-c",
            "user.name=eren",
            "-c",
            "user.email=eren@localhost",
            "commit",
            "--allow-empty",
            "-m",
            message,
        ],
    )
    .await?;
    Ok(true)
}

/// Commit whatever an agent left uncommitted in its own worktree.
///
/// Shared because both landing paths need it and for the same reason: a card
/// in review can hold work the diff already showed but nothing has committed,
/// and either path proceeding without it would act on a branch missing exactly
/// the changes the person just approved.
///
/// Only ever touches Eren's own worktree, never the user's checkout, and is
/// idempotent — a clean worktree commits nothing.
async fn commit_worktree(worktree: &Worktree, message: &str) -> anyhow::Result<()> {
    // Mid-merge, `add -A` would mark every conflicted file resolved with its
    // markers still in it, and the commit would carry them onward to Merge or
    // Push. And when the resolution leaves the tree identical to HEAD there is
    // nothing for `status` to report, so the merge would never be concluded
    // at all. So: refuse while markers remain, and otherwise always commit.
    if merging(&worktree.path).await {
        let marked =
            files_with_markers(&worktree.path, &unmerged_files(&worktree.path).await?).await;
        if !marked.is_empty() {
            return Err(MergeRefusal::Markers { files: marked }.into());
        }
        git(&worktree.path, &["add", "-A"]).await?;
        git(&worktree.path, &["commit", "--no-edit"]).await?;
        return Ok(());
    }
    git(&worktree.path, &["add", "-A"]).await?;
    let status = git(&worktree.path, &["status", "--porcelain"]).await?;
    if !status.trim().is_empty() {
        git(&worktree.path, &["commit", "-m", message]).await?;
    }
    Ok(())
}

/// Why a card could not land, as something the dashboard can act on.
#[derive(Debug, thiserror::Error)]
pub enum MergeRefusal {
    /// The person's own checkout has uncommitted changes in the way.
    #[error("{message}")]
    Dirty { message: String },
    /// The card's branch conflicts with the base.
    #[error("{message}")]
    Conflict { message: String, files: Vec<String> },
    /// Conflict markers are still in the card's work.
    #[error(
        "conflict markers are still in {} — resolve them before this card can land",
        files.join(", ")
    )]
    Markers { files: Vec<String> },
}

impl MergeRefusal {
    /// The word the dashboard switches on.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Dirty { .. } => "dirty",
            Self::Conflict { .. } => "conflict",
            Self::Markers { .. } => "markers",
        }
    }

    pub fn files(&self) -> &[String] {
        match self {
            Self::Dirty { .. } => &[],
            Self::Conflict { files, .. } | Self::Markers { files } => files,
        }
    }
}

/// What bringing the base into a card's branch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUpdate {
    UpToDate,
    Merged,
    /// Left in progress in the worktree, with these files conflicted.
    Conflicted {
        files: Vec<String>,
    },
}

/// Where a card's diff is measured from.
///
/// The point its branch left the base — not the base's tip. Diffing against
/// the tip meant that as soon as anyone landed anything else, every open
/// card's diff showed those commits *reversed*, as if this agent had undone
/// them. Mid-merge it is the commit being merged in, so the diff shows the
/// card's work against the base it is about to sit on.
async fn diff_base(worktree: &Path, base_branch: &str) -> anyhow::Result<String> {
    if merging(worktree).await {
        return Ok("MERGE_HEAD".to_string());
    }
    let base = resolve_base(worktree, base_branch).await?;
    Ok(match git(worktree, &["merge-base", &base, "HEAD"]).await {
        Ok(fork) if !fork.trim().is_empty() => fork.trim().to_string(),
        _ => base,
    })
}

/// `add -N .`, so untracked files show in a diff. Skipped mid-merge: it can
/// replace a conflicted file's unmerged index entries, quietly erasing the
/// record of which files are still in conflict.
async fn intend_to_add(worktree: &Path) -> anyhow::Result<()> {
    if merging(worktree).await {
        return Ok(());
    }
    git(worktree, &["add", "-N", "."]).await.map(|_| ())
}

async fn merging(dir: &Path) -> bool {
    git(dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .await
        .is_ok()
}

async fn unmerged_files(dir: &Path) -> anyhow::Result<Vec<String>> {
    let out = git(
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-only",
            "--diff-filter=U",
        ],
    )
    .await?;
    let mut files: Vec<String> = out.lines().map(str::to_string).collect();
    files.dedup();
    Ok(files)
}

async fn behind(dir: &Path, base: &str) -> anyhow::Result<u32> {
    let out = git(dir, &["rev-list", "--count", &format!("HEAD..{base}")]).await?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// The files among `files` that still hold a conflict marker.
async fn files_with_markers(dir: &Path, files: &[String]) -> Vec<String> {
    let mut marked = vec![];
    for f in files {
        // A file the resolution deleted has no markers to hold.
        if let Ok(text) = tokio::fs::read_to_string(dir.join(f)).await {
            if has_conflict_marker(&text) {
                marked.push(f.clone());
            }
        }
    }
    marked
}

/// Does this text hold a conflict marker? `<<<<<<<` and `>>>>>>>` only:
/// `=======` alone is a Markdown heading underline, and treating it as a
/// marker would refuse every README with a setext heading.
fn has_conflict_marker(text: &str) -> bool {
    text.lines().any(is_marker_line)
}

fn is_marker_line(line: &str) -> bool {
    ["<<<<<<<", ">>>>>>>"].iter().any(|m| {
        line.strip_prefix(m)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

/// Files a unified diff *adds* conflict markers to.
pub fn conflict_markers(diff: &str) -> Vec<String> {
    let mut file = String::new();
    let mut marked: Vec<String> = vec![];
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
        } else if let Some(added) = line.strip_prefix('+') {
            if is_marker_line(added) && marked.last() != Some(&file) {
                marked.push(file.clone());
            }
        }
    }
    marked
}

/// The files git names in "CONFLICT (…): Merge conflict in <file>" lines.
fn conflicted_in(git_output: &str) -> Vec<String> {
    let mut files: Vec<String> = git_output
        .lines()
        .filter_map(|line| line.split_once("Merge conflict in ").map(|(_, f)| f))
        // `git()` joins stderr and stdout with " — " on one line.
        .map(|f| f.split(" — ").next().unwrap_or(f).trim().to_string())
        .filter(|f| !f.is_empty())
        .collect();
    files.dedup();
    files
}

/// A remote's URL, or `None` when there is no such remote.
///
/// Its absence is a condition to report — "there is nowhere to push" — not a
/// failure, which is why this returns an `Option` rather than an error. Lives
/// here so every `git` invocation still goes through one runner; the pull
/// request routes previously spawned their own `Command::new("git")` for this,
/// which was the only exception to that rule.
pub async fn remote_url(repo: &Path, remote: &str) -> Option<String> {
    let out = git(repo, &["remote", "get-url", remote]).await.ok()?;
    let url = out.trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Whether a push was refused because the branch and the remote disagree.
///
/// Worth telling apart from every other push failure, because it is the one
/// with a next step that only a person can authorise. A retry with "start from
/// a clean checkout" deletes the branch and recreates it under the same name
/// from a fresh base, so a card pushed once and retried has a history the
/// remote cannot fast-forward to.
///
/// Force is not the automatic answer to that. A force-push silently discards
/// review comments attached to the commits it replaces — the same category of
/// thing as quietly downgrading a permission, which this codebase refuses at
/// the click. It is offered as a second, explicit click instead.
fn push_rejection(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    if !lower.contains("[rejected]") {
        return None;
    }
    if lower.contains("non-fast-forward") || lower.contains("fetch first") {
        return Some(
            "the branch on GitHub has commits this one does not, so pushing would \
             have to overwrite it — which discards any review comments on what is \
             there now. Retrying a card from a clean checkout rewrites its branch, \
             which is the usual reason. Push again with force if that is what you \
             want."
                .to_string(),
        );
    }
    None
}

/// `git status --porcelain` as something worth putting in an error.
///
/// Capped, because the point is to name the files in the way — a person with
/// four hundred modified files does not need four hundred lines to understand
/// that their tree is dirty, and an alert that long is one nobody reads.
fn describe_dirty(porcelain: &str) -> String {
    const SHOWN: usize = 10;
    let lines: Vec<&str> = porcelain.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out: Vec<String> = lines
        .iter()
        .take(SHOWN)
        .map(|l| format!("  {}", l.trim()))
        .collect();
    if lines.len() > SHOWN {
        out.push(format!("  … and {} more", lines.len() - SHOWN));
    }
    out.join("\n")
}

/// A worktree this repository is still holding, and whether it can go.
#[derive(Debug, Clone)]
pub struct Held {
    pub path: PathBuf,
    pub branch: String,
    pub bytes: u64,
    /// Uncommitted work in the worktree itself. Reclaiming would lose it.
    pub dirty: bool,
    /// Every commit on this branch has an equivalent in the base, so deleting
    /// it loses nothing. See [`landed`] for why this is not `--merged`.
    pub landed: bool,
}

impl Held {
    /// Only when the answer is provable. Everything else is kept and explained.
    pub fn reclaimable(&self) -> bool {
        self.landed && !self.dirty
    }

    /// Why this one is being kept, for a person who expected it to go.
    pub fn kept_because(&self) -> Option<&'static str> {
        match (self.dirty, self.landed) {
            (true, _) => Some("it has uncommitted changes in it"),
            (_, false) => Some("its work is not in the base branch"),
            _ => None,
        }
    }
}

/// Has this branch's work reached the base branch?
///
/// `git cherry`, not `git branch --merged`, and the difference is the whole
/// feature. `--merged` asks an ancestry question, and a *squash* merge — which
/// is the only kind Eren performs — writes a brand-new commit, so the branch
/// it came from is never an ancestor of anything. Every card anyone had ever
/// merged would have read as unlanded, and the sweep would have declined to
/// reclaim the exact directories it exists to reclaim.
///
/// `git cherry <base> <branch>` compares patch-ids instead: `-` for a commit
/// with an equivalent upstream, `+` for one without. Empty output — a branch
/// with nothing the base does not have — counts as landed. Checked against the
/// real repositories on this machine, where it correctly separates a
/// squash-merged card from a 742 MB branch that never landed, and `--merged`
/// gets the first of those wrong.
///
/// An error is `false`. Not knowing must never read as safe.
async fn has_landed(repo: &Path, base: &str, branch: &str) -> bool {
    match git(repo, &["cherry", base, branch]).await {
        Ok(out) => !out.lines().any(|l| l.starts_with('+')),
        Err(_) => false,
    }
}

/// Every worktree **Eren made** in this repository, with the two facts that
/// decide whether it may go.
///
/// Deliberately read from git rather than from the database. A row can be
/// deleted while its directory survives — a bake-off variant whose run
/// cascaded away with its card, a workflow fan-out worktree whose id was never
/// persisted at all — and those are exactly the ones nothing would otherwise
/// ever look at again.
///
/// Ownership is filtered here, not left to callers, because every caller
/// feeds the result to a delete. It used to list *all* of a repository's
/// worktrees, and the boot sweep removed any that were clean and "landed" —
/// which a person's own `git worktree add ../hotfix -b hotfix` is, the moment
/// it has no commits of its own — along with `branch -D` on their branch.
pub async fn inventory(
    manager: &WorktreeManager,
    repo: &Path,
    base_branch: &str,
) -> anyhow::Result<Vec<Held>> {
    let listed = git(repo, &["worktree", "list", "--porcelain"]).await?;
    let base = resolve_base(repo, base_branch).await?;

    let mut out = Vec::new();
    // `skip(1)`, not a path comparison. The first record is always the main
    // checkout — the person's own repository, which is not a worktree in the
    // sense that matters here and must never be offered for removal. Comparing
    // paths looked equivalent and was not: on macOS a project under `/var` is
    // reported by git as `/private/var`, so the comparison silently failed and
    // the repository itself appeared in the list.
    for (path, branch) in parse_worktree_list(&listed).into_iter().skip(1) {
        let Some(branch) = branch else { continue };
        if !manager.owns(&path, &branch) {
            continue;
        }
        let dirty = git(&path, &["status", "--porcelain"])
            .await
            .map(|s| !s.trim().is_empty())
            // A directory git can no longer read is not "clean" — it is
            // unknown, and unknown must not be reclaimable.
            .unwrap_or(true);
        out.push(Held {
            bytes: dir_size(&path).await,
            landed: has_landed(repo, &base, &branch).await,
            dirty,
            path,
            branch,
        });
    }
    Ok(out)
}

/// `git worktree list --porcelain` into (path, branch).
///
/// Pure: the format is a blank-line-separated record per worktree, and a
/// detached one has no `branch` line at all.
pub fn parse_worktree_list(out: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut all = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(prev) = path.take() {
                all.push((prev, branch.take()));
            }
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    if let Some(p) = path {
        all.push((p, branch));
    }
    all
}

/// Bytes on disk. Approximate on purpose — this is a figure to show a person,
/// not an accounting record, and it must never fail the request it is part of.
async fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            // `symlink_metadata`: following a link out of the worktree would
            // count somebody else's disk, and a loop would never finish.
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// One file standing between a person and a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyFile {
    /// Git's staged-state letter. A space means "not staged".
    pub index: char,
    /// Git's working-tree letter.
    pub worktree: char,
    pub path: String,
}

/// What the merge guard is looking at, as data rather than as a sentence.
///
/// The guard has always been able to name these files; nothing could ever
/// *read* them. So the only way to act on "commit or stash them and merge
/// again" was to leave the product, and the only way to find out which files it
/// meant was to squint at an error string. Same query, same flags — the two
/// must agree, or the dashboard would offer to resolve a set the merge does not
/// care about.
/// Every path `git status` reports as changed, untracked ones included.
///
/// For noticing what a command left behind in a worktree, so unlike
/// `checkout_status` it counts untracked files: build output that is not
/// ignored is exactly what a later `add -A` would sweep into a card's diff.
/// Ignored files are not listed, so a `target/` or `node_modules/` that the
/// project ignores costs nothing here.
pub async fn changed_paths(dir: &Path) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let out = git(
        dir,
        &[
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
    )
    .await?;
    Ok(parse_porcelain(&out).into_iter().map(|f| f.path).collect())
}

pub async fn checkout_status(repo: &Path) -> anyhow::Result<(Option<String>, Vec<DirtyFile>)> {
    let out = git(
        repo,
        // `quotePath=false` so a path with an accent in it comes back readable
        // rather than as octal escapes. Genuinely awkward paths — a quote, a
        // backslash, a newline — are still quoted, and `parse_porcelain`
        // unquotes them.
        &[
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ],
    )
    .await?;
    Ok((current_branch(repo).await, parse_porcelain(&out)))
}

/// `git status --porcelain` into rows.
///
/// Pure and separately tested because the format has more corners than it
/// looks: two status columns where either may be a space, a rename written as
/// `old -> new`, and C-quoting for paths git thinks are awkward.
pub fn parse_porcelain(out: &str) -> Vec<DirtyFile> {
    out.lines()
        .filter(|l| l.len() > 3)
        .map(|line| {
            let mut chars = line.chars();
            let index = chars.next().unwrap_or(' ');
            let worktree = chars.next().unwrap_or(' ');
            let rest = line[3..].trim_end();
            // A rename names both ends; the new one is the file that is there
            // now, and the one a person would go and look at.
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            DirtyFile {
                index,
                worktree,
                path: unquote(path),
            }
        })
        .collect()
}

/// Undo git's C-style quoting of an awkward path.
///
/// Left as-is when unquoted, which is almost always. Octal escapes are decoded
/// as bytes and only then read as UTF-8 — a multi-byte character arrives as
/// several `\NNN` in a row, so decoding them one at a time would produce
/// mojibake instead of the name.
fn unquote(path: &str) -> String {
    if !(path.starts_with('"') && path.ends_with('"') && path.len() >= 2) {
        return path.to_string();
    }
    let inner = &path[1..path.len() - 1];
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some(d @ '0'..='7') => {
                let mut octal = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(v) => {
                            octal = octal * 8 + v;
                            chars.next();
                        }
                        None => break,
                    }
                }
                bytes.push(octal as u8);
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Set the checkout's changes aside so a merge can proceed.
///
/// The other half of what the guard's own message tells people to do. Tracked
/// files only, matching the guard exactly — sweeping untracked build output
/// into a stash would be doing something nobody asked for to files that were
/// never in the way.
/// Where the checkout stands against its upstream: (behind, ahead).
///
/// `None` when there is no upstream to stand against — a repo that has never
/// been pushed, or a branch with no tracking ref. That is an answer, not an
/// error: the UI turns it into "publish this branch" rather than a count.
pub async fn ahead_behind(repo: &Path) -> Option<(u32, u32)> {
    let out = git(
        repo,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    )
    .await
    .ok()?;
    let mut parts = out.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    Some((behind, ahead))
}

/// Bring the checkout up to date with its upstream, fast-forward only.
///
/// `--ff-only` is the whole safety story: a pull that would merge or rebase
/// is a decision about *whose history wins*, and a dashboard button must not
/// make it. Diverged branches fail with git's own message, which says exactly
/// that.
pub async fn pull_ff(repo: &Path) -> anyhow::Result<String> {
    git(repo, &["pull", "--ff-only"]).await
}

/// Push the current branch.
///
/// With an upstream this is a plain `git push`. Without one — a branch never
/// published — it pushes to `origin` and sets the tracking ref, because "push
/// this" on an unpublished branch can only mean "publish it". No remote at
/// all is git's own error, passed through.
pub async fn push_current(repo: &Path) -> anyhow::Result<String> {
    if ahead_behind(repo).await.is_some() {
        return git(repo, &["push"]).await;
    }
    let branch = current_branch(repo)
        .await
        .ok_or_else(|| anyhow::anyhow!("the checkout is not on a branch (detached HEAD?)"))?;
    git(repo, &["push", "-u", "origin", &branch]).await
}

pub async fn stash(repo: &Path, message: &str) -> anyhow::Result<()> {
    git(repo, &["stash", "push", "-m", message]).await?;
    Ok(())
}

pub(crate) async fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = env_guard::command("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        // Both streams, because git is inconsistent about which it uses:
        // "nothing to commit, working tree clean" is a *stdout* message on a
        // failing exit, so reporting stderr alone produced the useless
        // "git commit -m … failed:" with nothing after the colon.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = [stderr.trim(), stdout.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" — ");
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            if detail.is_empty() {
                "no output".into()
            } else {
                detail
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn short_hash(s: &str) -> String {
    // Stable, dependency-free path hash (FNV-1a).
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "test@eren.local"],
            vec!["config", "user.name", "eren-test"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            git(dir, &args).await.unwrap();
        }
    }

    /// Commit `content` to `file` on the repo's own `main`.
    async fn commit_on_main(repo: &Path, file: &str, content: &str) {
        tokio::fs::write(repo.join(file), content).await.unwrap();
        git(repo, &["add", "-A"]).await.unwrap();
        git(repo, &["commit", "-m", &format!("main: {file}")])
            .await
            .unwrap();
    }

    /// The bug: diffing against the base's *tip* showed every commit landed
    /// since the card branched as if the agent had deleted it.
    #[tokio::test]
    async fn a_cards_diff_does_not_show_later_base_commits_reversed() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("card.txt"), "mine\n")
            .await
            .unwrap();
        commit_on_main(repo_dir.path(), "landed-elsewhere.txt", "theirs\n").await;

        let diff = mgr.diff(&wt.path, "main").await.unwrap();
        assert!(diff.contains("card.txt"), "{diff}");
        assert!(!diff.contains("landed-elsewhere.txt"), "{diff}");
        assert_eq!(mgr.behind_base(&wt.path, "main").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn bringing_the_base_in_cleanly_merges_and_catches_up() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        assert_eq!(
            mgr.update_from_base(&wt, "main", "card").await.unwrap(),
            BaseUpdate::UpToDate
        );

        tokio::fs::write(wt.path.join("card.txt"), "mine\n")
            .await
            .unwrap();
        commit_on_main(repo_dir.path(), "other.txt", "theirs\n").await;
        assert_eq!(
            mgr.update_from_base(&wt, "main", "card").await.unwrap(),
            BaseUpdate::Merged
        );
        assert_eq!(mgr.behind_base(&wt.path, "main").await.unwrap(), 0);
        let diff = mgr.diff(&wt.path, "main").await.unwrap();
        assert!(
            diff.contains("card.txt") && !diff.contains("other.txt"),
            "{diff}"
        );
        // And it still lands.
        mgr.squash_merge(repo_dir.path(), &wt, "main", "card")
            .await
            .unwrap();
        assert!(repo_dir.path().join("card.txt").exists());
    }

    /// The whole path: conflict met on the card's branch, markers refused,
    /// resolution concluded, card lands.
    #[tokio::test]
    async fn a_conflict_waits_in_the_worktree_until_its_markers_are_gone() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        commit_on_main(repo_dir.path(), "shared.txt", "original\n").await;
        let mgr = WorktreeManager::new(root.path());
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("shared.txt"), "the card's version\n")
            .await
            .unwrap();
        commit_on_main(repo_dir.path(), "shared.txt", "main's version\n").await;

        // Landing directly is refused as a conflict, naming the file.
        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "card")
            .await
            .unwrap_err();
        let refusal = err.downcast_ref::<MergeRefusal>().expect("a typed refusal");
        assert_eq!(refusal.kind(), "conflict");
        assert_eq!(refusal.files(), ["shared.txt"]);

        // Brought into the card's branch, it waits there with markers.
        let update = mgr.update_from_base(&wt, "main", "card").await.unwrap();
        assert_eq!(
            update,
            BaseUpdate::Conflicted {
                files: vec!["shared.txt".into()]
            }
        );
        // Looking at the diff must not erase which files are in conflict.
        mgr.diff(&wt.path, "main").await.unwrap();
        assert_eq!(
            mgr.merge_in_progress(&wt.path).await,
            Some(vec!["shared.txt".to_string()])
        );

        // Neither landing nor concluding gets past the markers.
        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "card")
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<MergeRefusal>().map(|r| r.kind()),
            Some("markers")
        );
        assert!(mgr.conclude_merge(&wt, "card").await.is_err());

        // Resolved — here, to exactly what HEAD already had, the case where
        // `git status` reports nothing and the merge used to stay open.
        tokio::fs::write(wt.path.join("shared.txt"), "the card's version\n")
            .await
            .unwrap();
        assert!(mgr.conclude_merge(&wt, "card").await.unwrap());
        assert_eq!(mgr.merge_in_progress(&wt.path).await, None);

        mgr.squash_merge(repo_dir.path(), &wt, "main", "card")
            .await
            .unwrap();
        let landed = tokio::fs::read_to_string(repo_dir.path().join("shared.txt"))
            .await
            .unwrap();
        assert_eq!(landed, "the card's version\n");
    }

    /// Markers an agent committed itself are caught at landing too.
    #[tokio::test]
    async fn committed_conflict_markers_never_land() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(
            wt.path.join("oops.txt"),
            "<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> main\n",
        )
        .await
        .unwrap();
        git(&wt.path, &["add", "-A"]).await.unwrap();
        git(&wt.path, &["commit", "-m", "agent committed it"])
            .await
            .unwrap();

        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "card")
            .await
            .unwrap_err();
        let refusal = err.downcast_ref::<MergeRefusal>().unwrap();
        assert_eq!(refusal.kind(), "markers");
        assert_eq!(refusal.files(), ["oops.txt"]);
        assert!(!repo_dir.path().join("oops.txt").exists());
    }

    #[test]
    fn a_markdown_underline_is_not_a_conflict_marker() {
        assert!(!has_conflict_marker("Title\n=======\n\nText"));
        assert!(has_conflict_marker("a\n<<<<<<< HEAD\nb"));
        assert!(has_conflict_marker(">>>>>>>\n"));
        assert!(!has_conflict_marker("<<<<<<<<< not quite"));
        assert_eq!(
            conflict_markers("+++ b/x.rs\n+fine\n+++ b/y.md\n+<<<<<<< ours\n+>>>>>>> theirs\n"),
            vec!["y.md".to_string()]
        );
    }

    #[test]
    fn conflicted_files_are_read_out_of_gits_report() {
        let out = "git merge --squash eren/x failed: Auto-merging a.rs\n\
                   CONFLICT (content): Merge conflict in a.rs\n\
                   CONFLICT (content): Merge conflict in dir/b c.txt — Automatic merge failed";
        assert_eq!(
            conflicted_in(out),
            vec!["a.rs".to_string(), "dir/b c.txt".to_string()]
        );
    }

    #[tokio::test]
    async fn ensure_repo_makes_a_fresh_folder_immediately_worktree_able() {
        // The regression this guards: `git init` alone leaves an unborn HEAD,
        // so `git worktree add … main` fails and the project's first task run
        // dies before the engine ever starts.
        let dir = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);

        let mgr = WorktreeManager::new(wt_root.path());
        mgr.create(dir.path(), "main", Uuid::new_v4(), "first-task")
            .await
            .expect("a freshly initialized project must support a worktree");
    }

    /// A worktree with one edited file, one new file, and one binary.
    async fn worktree_with_changes() -> (tempfile::TempDir, tempfile::TempDir, Worktree) {
        let dir = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("kept.txt"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);
        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(dir.path(), "main", Uuid::new_v4(), "t")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("kept.txt"), "one\nCHANGED\nthree\nfour\n")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("added.txt"), "brand new\n")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("logo.png"), [0u8, 159, 146, 150, 0, 1, 2])
            .await
            .unwrap();
        (dir, wt_root, wt)
    }

    #[tokio::test]
    async fn diff_stat_counts_lines_per_file() {
        let (_dir, _root, wt) = worktree_with_changes().await;
        let mgr = WorktreeManager::new(std::path::Path::new("/unused"));
        let stats = mgr.diff_stat(&wt.path, "main").await.unwrap();

        let kept = stats.iter().find(|f| f.path == "kept.txt").unwrap();
        assert_eq!((kept.added, kept.removed), (2, 1));
        assert!(!kept.binary);
    }

    #[tokio::test]
    async fn an_untracked_file_is_counted() {
        // The reason `diff_stat` runs `git add -N .` — without it a brand new
        // file is invisible, and a card that only adds files reports nothing
        // changed.
        let (_dir, _root, wt) = worktree_with_changes().await;
        let mgr = WorktreeManager::new(std::path::Path::new("/unused"));
        let stats = mgr.diff_stat(&wt.path, "main").await.unwrap();
        let added = stats.iter().find(|f| f.path == "added.txt").unwrap();
        assert_eq!((added.added, added.removed), (1, 0));
    }

    #[tokio::test]
    async fn a_binary_file_is_reported_as_binary_not_as_zero() {
        // `--numstat` writes `-` for both counts. Reporting 0/0 would tell a
        // reader the image was untouched.
        let (_dir, _root, wt) = worktree_with_changes().await;
        let mgr = WorktreeManager::new(std::path::Path::new("/unused"));
        let stats = mgr.diff_stat(&wt.path, "main").await.unwrap();
        let png = stats.iter().find(|f| f.path == "logo.png").unwrap();
        assert!(png.binary, "a PNG must not read as an unchanged text file");
    }

    #[tokio::test]
    async fn diff_file_returns_only_that_file() {
        let (_dir, _root, wt) = worktree_with_changes().await;
        let mgr = WorktreeManager::new(std::path::Path::new("/unused"));
        let one = mgr.diff_file(&wt.path, "main", "kept.txt").await.unwrap();
        assert!(one.contains("CHANGED"));
        assert!(!one.contains("added.txt"), "it leaked another file: {one}");
    }

    #[tokio::test]
    async fn a_path_that_looks_like_a_flag_is_still_a_path() {
        // The `--` separator plus an argument vector, never a shell string.
        let (_dir, _root, wt) = worktree_with_changes().await;
        let mgr = WorktreeManager::new(std::path::Path::new("/unused"));
        tokio::fs::write(wt.path.join("--oops.txt"), "x\n")
            .await
            .unwrap();
        let out = mgr.diff_file(&wt.path, "main", "--oops.txt").await;
        assert!(out.is_ok(), "{out:?}");
    }

    #[test]
    fn numstat_parses_text_and_binary_lines() {
        let text = parse_numstat("12\t3\tsrc/main.rs").unwrap();
        assert_eq!((text.added, text.removed, text.binary), (12, 3, false));
        let bin = parse_numstat("-\t-\tassets/logo.png").unwrap();
        assert_eq!((bin.added, bin.removed, bin.binary), (0, 0, true));
        // A path with a space is one path, not two fields.
        assert_eq!(
            parse_numstat("1\t0\tmy notes.md").unwrap().path,
            "my notes.md"
        );
        assert!(parse_numstat("garbage").is_none());
    }

    #[tokio::test]
    async fn existing_files_survive_into_the_worktree() {
        // An *empty* initial commit would satisfy git but hand the agent a
        // worktree with none of the user's files in it.
        let dir = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("notes.md"), "existing work\n")
            .await
            .unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);

        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(dir.path(), "main", Uuid::new_v4(), "t")
            .await
            .unwrap();
        let carried = tokio::fs::read_to_string(wt.path.join("notes.md"))
            .await
            .unwrap();
        assert_eq!(carried, "existing work\n");
    }

    #[tokio::test]
    async fn a_file_written_after_the_repo_exists_still_reaches_a_worktree() {
        // The bug this pins, found by running it rather than by a test: an app
        // is `ensure_repo`'d and *then* has its manifest written, so the file
        // was never on `main` — and the agent asked to change that app opened
        // an empty folder and spent a paid run looking for it.
        let dir = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);
        tokio::fs::write(dir.path().join("eren.app.yaml"), "name: T\n")
            .await
            .unwrap();
        assert!(commit_all(dir.path(), "Add T").await.unwrap());

        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(dir.path(), "main", Uuid::new_v4(), "t")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(wt.path.join("eren.app.yaml"))
                .await
                .unwrap(),
            "name: T\n"
        );
    }

    #[tokio::test]
    async fn committing_an_unchanged_folder_writes_nothing() {
        // Saving the manifest is what commits it, and a person pressing Save on
        // text they did not change should not add a commit — nor should the
        // re-read that follows a landed merge, where git already has the file.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);
        tokio::fs::write(dir.path().join("a.txt"), "one")
            .await
            .unwrap();
        assert!(commit_all(dir.path(), "first").await.unwrap());
        assert!(!commit_all(dir.path(), "again").await.unwrap());

        let log = git(dir.path(), &["log", "--oneline"]).await.unwrap();
        assert_eq!(log.lines().count(), 2, "got: {log}");
    }

    #[tokio::test]
    async fn ensure_repo_leaves_an_existing_repo_alone() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        git(dir.path(), &["checkout", "-b", "trunk"]).await.unwrap();
        assert_eq!(ensure_repo(dir.path(), "main").await, Vcs::Git);
        // Still on the branch the user had; init would have reset this.
        let branch = git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(branch.trim(), "trunk");
    }

    #[tokio::test]
    async fn a_folder_inside_a_repo_is_not_nested_with_a_second_one() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let inner = repo.path().join("packages/web");
        tokio::fs::create_dir_all(&inner).await.unwrap();

        match ensure_repo(&inner, "main").await {
            Vcs::None(reason) => assert!(
                reason.contains("inside the repository"),
                "reason should point at the parent repo, got: {reason}"
            ),
            Vcs::Git => panic!("must not create a repo nested inside another"),
        }
        assert!(!inner.join(".git").exists());
    }

    /// The reported failure: `git worktree add … main` dies with
    /// "invalid reference: main" because the repo has no commits.
    #[tokio::test]
    async fn an_existing_repo_with_no_commits_is_made_usable() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]).await.unwrap();
        tokio::fs::write(repo.path().join("app.py"), "print('hi')\n")
            .await
            .unwrap();
        assert!(!has_commits(repo.path()).await, "precondition: unborn HEAD");

        let state = ensure_repo_state(repo.path(), "main").await;
        assert_eq!(state.vcs, Vcs::Git);
        assert_eq!(state.branch, "main");
        assert!(has_commits(repo.path()).await);

        // The user's existing file must be in that first commit, or the agent
        // gets a worktree with none of their work in it.
        let tracked = git(repo.path(), &["ls-files"]).await.unwrap();
        assert!(tracked.contains("app.py"));

        let wt_root = tempfile::tempdir().unwrap();
        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(repo.path(), "main", Uuid::new_v4(), "software-team")
            .await
            .expect("worktree creation should now succeed");
        assert!(wt.path.join("app.py").exists());
    }

    #[tokio::test]
    async fn reports_the_actual_branch_rather_than_the_assumed_one() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "master"]).await.unwrap();
        let state = ensure_repo_state(repo.path(), "main").await;
        assert_eq!(state.branch, "master");
    }

    #[tokio::test]
    async fn a_stale_base_branch_falls_back_instead_of_failing_the_run() {
        let repo = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "master"]).await.unwrap();
        super::init_repo(repo.path(), "master").await.unwrap();

        // The project row still says "main" — a rename, or a default guessed
        // before the repo existed.
        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(repo.path(), "main", Uuid::new_v4(), "task")
            .await
            .expect("should branch from the repo's actual branch");
        assert!(wt.path.exists());
    }

    /// Projects added before commits were checked for are still in the
    /// database, so starting a run has to repair them rather than fail.
    #[tokio::test]
    async fn a_run_on_a_commitless_repo_repairs_it_instead_of_failing() {
        let repo = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]).await.unwrap();
        tokio::fs::write(repo.path().join("main.py"), "x = 1\n")
            .await
            .unwrap();

        let mgr = WorktreeManager::new(wt_root.path());
        let wt = mgr
            .create(repo.path(), "main", Uuid::new_v4(), "software-team")
            .await
            .expect("a commitless repo should be repaired, not rejected");
        assert!(wt.path.join("main.py").exists());
    }

    #[tokio::test]
    async fn resolve_base_explains_a_repo_with_nothing_to_branch_from() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-b", "main"]).await.unwrap();
        let err = resolve_base(repo.path(), "main")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no commits"), "unhelpful message: {err}");
    }

    #[tokio::test]
    async fn create_diff_merge_roundtrip() {
        let repo_dir = tempfile::tempdir().unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;

        let mgr = WorktreeManager::new(wt_root.path());
        let task_id = Uuid::new_v4();
        let wt = mgr
            .create(repo_dir.path(), "main", task_id, "add-hello")
            .await
            .unwrap();
        assert!(mgr.manages(&wt.path));

        tokio::fs::write(wt.path.join("hello.txt"), "hi\n")
            .await
            .unwrap();
        let diff = mgr.diff(&wt.path, "main").await.unwrap();
        assert!(diff.contains("hello.txt"));

        mgr.squash_merge(repo_dir.path(), &wt, "main", "add hello")
            .await
            .unwrap();
        let log = git(repo_dir.path(), &["log", "--oneline"]).await.unwrap();
        assert!(log.contains("add hello"));
        assert!(repo_dir.path().join("hello.txt").exists());

        mgr.remove(repo_dir.path(), &wt).await.unwrap();
        assert!(!wt.path.exists());
    }

    #[test]
    fn a_worktree_list_yields_paths_and_branches() {
        let out = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree /wt/one\nHEAD def\nbranch refs/heads/eren/card-1234\n\n\
                   worktree /wt/detached\nHEAD 999\ndetached\n";
        let all = parse_worktree_list(out);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], (PathBuf::from("/repo"), Some("main".to_string())));
        assert_eq!(all[1].1.as_deref(), Some("eren/card-1234"));
        // A detached worktree has no branch line, and must not inherit the
        // previous record's — which is what a naive line-by-line parse does.
        assert_eq!(all[2].1, None);
    }

    #[tokio::test]
    async fn inventory_tells_apart_what_may_go_from_what_may_not() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        // Landed: merged, clean.
        let landed = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "landed")
            .await
            .unwrap();
        tokio::fs::write(landed.path.join("a.txt"), "a\n")
            .await
            .unwrap();
        mgr.squash_merge(repo_dir.path(), &landed, "main", "eren: landed")
            .await
            .unwrap();

        // Never landed: its work exists nowhere else.
        let open = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "open")
            .await
            .unwrap();
        tokio::fs::write(open.path.join("b.txt"), "b\n")
            .await
            .unwrap();
        git(&open.path, &["add", "-A"]).await.unwrap();
        git(&open.path, &["commit", "-m", "wip"]).await.unwrap();

        // Merged, but someone has been editing in it since.
        let touched = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "touched")
            .await
            .unwrap();
        tokio::fs::write(touched.path.join("c.txt"), "c\n")
            .await
            .unwrap();
        mgr.squash_merge(repo_dir.path(), &touched, "main", "eren: touched")
            .await
            .unwrap();
        tokio::fs::write(touched.path.join("c.txt"), "edited\n")
            .await
            .unwrap();

        let held = inventory(&mgr, repo_dir.path(), "main").await.unwrap();
        assert_eq!(
            held.len(),
            3,
            "the repo's own checkout is not a worktree: {held:?}"
        );

        let by = |name: &str| {
            held.iter()
                .find(|h| h.branch.contains(name))
                .unwrap()
                .clone()
        };
        // The one that matters: a *squash*-merged branch is never an ancestor
        // of the base, so `git branch --merged` calls it unlanded and this
        // whole sweep would decline to reclaim the directories it exists for.
        assert!(by("landed").landed, "a squash-merged branch has landed");
        assert!(by("landed").reclaimable());
        assert_eq!(by("landed").kept_because(), None);
        assert!(!by("open").reclaimable(), "unlanded work must be kept");
        assert_eq!(
            by("open").kept_because(),
            Some("its work is not in the base branch")
        );
        assert!(
            !by("touched").reclaimable(),
            "a dirty worktree must be kept"
        );
        assert_eq!(
            by("touched").kept_because(),
            Some("it has uncommitted changes in it")
        );
        assert!(by("landed").bytes > 0, "a size worth showing");
    }

    /// A person's own worktrees are never Eren's to reclaim, however clean
    /// and "landed" they look. Before ownership was checked, both of these
    /// were listed, and the boot sweep removed them with `branch -D`.
    #[tokio::test]
    async fn inventory_never_lists_a_worktree_eren_did_not_make() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        // `git worktree add ../hotfix -b hotfix`: no commits of its own, so
        // `git cherry` reports nothing unlanded.
        let hotfix = elsewhere.path().join("hotfix");
        git(
            repo_dir.path(),
            &["worktree", "add", hotfix.to_str().unwrap(), "-b", "hotfix"],
        )
        .await
        .unwrap();
        // Even on a branch that happens to look like ours, outside our root.
        let lookalike = elsewhere.path().join("lookalike");
        git(
            repo_dir.path(),
            &[
                "worktree",
                "add",
                lookalike.to_str().unwrap(),
                "-b",
                "eren/lookalike",
            ],
        )
        .await
        .unwrap();
        let ours = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "ours")
            .await
            .unwrap();

        let held = inventory(&mgr, repo_dir.path(), "main").await.unwrap();
        let branches: Vec<_> = held.iter().map(|h| h.branch.as_str()).collect();
        assert_eq!(branches, vec![ours.branch.as_str()], "{held:?}");
    }

    #[tokio::test]
    async fn a_card_branch_from_before_the_rename_is_still_ours() {
        // Its worktree is under our root and its branch has the old prefix:
        // still a card's, so still listed — and so still swept and landed.
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());
        let old_branch = format!("{}card-1a2b3c4d", eren_shared::brand::BRANCH_PREFIXES[1]);
        assert!(!old_branch.starts_with(BRANCH_PREFIX));
        let old = root.path().join("old-card");
        git(
            repo_dir.path(),
            &["worktree", "add", old.to_str().unwrap(), "-b", &old_branch],
        )
        .await
        .unwrap();

        let held = inventory(&mgr, repo_dir.path(), "main").await.unwrap();
        let branches: Vec<_> = held.iter().map(|h| h.branch.as_str()).collect();
        assert_eq!(branches, vec![old_branch.as_str()], "{held:?}");
    }

    #[test]
    fn porcelain_rows_carry_both_status_columns() {
        let rows = parse_porcelain(" M src/main.rs\nA  src/new.rs\nMM both.rs\n D gone.rs\n");
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            DirtyFile {
                index: ' ',
                worktree: 'M',
                path: "src/main.rs".into()
            }
        );
        assert_eq!(
            rows[1],
            DirtyFile {
                index: 'A',
                worktree: ' ',
                path: "src/new.rs".into()
            }
        );
        assert_eq!(
            rows[2],
            DirtyFile {
                index: 'M',
                worktree: 'M',
                path: "both.rs".into()
            }
        );
        assert_eq!(rows[3].path, "gone.rs");
    }

    #[test]
    fn a_rename_reports_where_the_file_is_now() {
        // Not where it was: the new name is the one you would go and look at.
        let rows = parse_porcelain("R  old/name.rs -> new/name.rs\n");
        assert_eq!(rows[0].path, "new/name.rs");
    }

    #[test]
    fn an_awkward_path_is_unquoted_rather_than_shown_as_escapes() {
        assert_eq!(
            parse_porcelain(" M \"with space.rs\"\n")[0].path,
            "with space.rs"
        );
        assert_eq!(
            parse_porcelain(" M \"say \\\"hi\\\".rs\"\n")[0].path,
            "say \"hi\".rs"
        );
        // Octal escapes are bytes, and a multi-byte character is several of
        // them — decoding one at a time would produce mojibake.
        assert_eq!(
            parse_porcelain(" M \"caf\\303\\251.rs\"\n")[0].path,
            "café.rs"
        );
    }

    #[test]
    fn nothing_dirty_is_no_rows_rather_than_one_empty_one() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n").is_empty());
    }

    #[tokio::test]
    async fn checkout_status_sees_exactly_what_the_merge_guard_sees() {
        // If these ever disagree, the dashboard offers to resolve a set of
        // files the merge does not care about — or misses one that it does.
        let repo_dir = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        tokio::fs::write(repo_dir.path().join("tracked.txt"), "one")
            .await
            .unwrap();
        git(repo_dir.path(), &["add", "-A"]).await.unwrap();
        git(repo_dir.path(), &["commit", "-m", "add"])
            .await
            .unwrap();

        tokio::fs::write(repo_dir.path().join("tracked.txt"), "two")
            .await
            .unwrap();
        tokio::fs::write(repo_dir.path().join("untracked.txt"), "new")
            .await
            .unwrap();

        let (branch, dirty) = checkout_status(repo_dir.path()).await.unwrap();
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(
            dirty.len(),
            1,
            "untracked files are not in the way: {dirty:?}"
        );
        assert_eq!(dirty[0].path, "tracked.txt");

        // And stashing it clears exactly that.
        stash(repo_dir.path(), "eren: test").await.unwrap();
        assert!(checkout_status(repo_dir.path()).await.unwrap().1.is_empty());
        assert!(
            repo_dir.path().join("untracked.txt").exists(),
            "stash took the wrong files"
        );
    }

    /// A merge that lands must not move the person somewhere else.
    ///
    /// `squash_merge` checks out the base branch to do its work. It never
    /// checked back, so anyone standing on a branch of their own was silently
    /// on `main` afterwards — noticed, if at all, by their next commit landing
    /// in the wrong place.
    #[tokio::test]
    async fn a_successful_merge_puts_the_checkout_back_where_it_was() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        git(repo_dir.path(), &["checkout", "-b", "wip"])
            .await
            .unwrap();
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("theirs.txt"), "agent\n")
            .await
            .unwrap();

        mgr.squash_merge(repo_dir.path(), &wt, "main", "eren: card")
            .await
            .unwrap();

        assert_eq!(current_branch(repo_dir.path()).await.unwrap(), "wip");
        let log = git(repo_dir.path(), &["log", "main", "--oneline"])
            .await
            .unwrap();
        assert!(
            log.contains("eren: card"),
            "the merge still has to land: {log}"
        );
    }

    /// A conflicting merge must not strand the checkout mid-merge.
    ///
    /// The nastiest shape this had: the `?` on `merge --squash` propagated with
    /// no recovery, leaving the person's own repository on the base branch,
    /// holding conflict markers, with a conflicted index. `git merge --abort`
    /// does not undo a `--squash` — there is no `MERGE_HEAD` — so they were one
    /// `reset --merge` away from working and nothing said so. Worse, the next
    /// Merge click hit the dirty guard and reported the conflict debris back to
    /// them as if it were their own uncommitted work.
    #[tokio::test]
    async fn a_conflicting_merge_leaves_the_checkout_clean_and_where_it_was() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        tokio::fs::write(repo_dir.path().join("shared.txt"), "original\n")
            .await
            .unwrap();
        git(repo_dir.path(), &["add", "-A"]).await.unwrap();
        git(repo_dir.path(), &["commit", "-m", "shared"])
            .await
            .unwrap();

        // The card edits the line…
        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("shared.txt"), "from the agent\n")
            .await
            .unwrap();

        // …and so does `main`, after the worktree branched. Committed, so the
        // dirty guard passes and the conflict is what fails.
        tokio::fs::write(repo_dir.path().join("shared.txt"), "from a person\n")
            .await
            .unwrap();
        git(repo_dir.path(), &["add", "-A"]).await.unwrap();
        git(repo_dir.path(), &["commit", "-m", "mine"])
            .await
            .unwrap();
        git(repo_dir.path(), &["checkout", "-b", "wip"])
            .await
            .unwrap();

        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "eren: card")
            .await
            .expect_err("a conflicting merge must fail");
        assert!(
            err.to_string().contains("put back on wip"),
            "the error has to say what was done for them: {err}"
        );

        assert_eq!(current_branch(repo_dir.path()).await.unwrap(), "wip");
        let dirty = git(repo_dir.path(), &["status", "--porcelain"])
            .await
            .unwrap();
        assert!(dirty.trim().is_empty(), "left behind: {dirty}");
        let content = tokio::fs::read_to_string(repo_dir.path().join("shared.txt"))
            .await
            .unwrap();
        assert!(
            !content.contains("<<<<"),
            "conflict markers survived: {content}"
        );
    }

    /// The merge must not swallow work the person had in progress.
    ///
    /// The bug: `squash_merge` checked out the base branch in the user's own
    /// repository with no idea what was in it. Git carried the uncommitted
    /// edits across and `merge --squash` staged into the same index, so
    /// unrelated in-progress work was committed under `eren: {title}` —
    /// silently, and attributed to a card that never touched it.
    #[tokio::test]
    async fn a_dirty_checkout_refuses_the_merge_instead_of_swallowing_it() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        // A tracked file the person is midway through editing, on a branch of
        // their own — the ordinary state of a working checkout.
        tokio::fs::write(repo_dir.path().join("mine.txt"), "draft\n")
            .await
            .unwrap();
        git(repo_dir.path(), &["add", "-A"]).await.unwrap();
        git(repo_dir.path(), &["commit", "-m", "mine"])
            .await
            .unwrap();
        git(repo_dir.path(), &["checkout", "-b", "wip"])
            .await
            .unwrap();
        tokio::fs::write(repo_dir.path().join("mine.txt"), "half a thought\n")
            .await
            .unwrap();

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("theirs.txt"), "agent\n")
            .await
            .unwrap();

        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "eren: card")
            .await
            .expect_err("a dirty checkout must refuse");
        let text = err.to_string();
        assert!(text.contains("uncommitted changes"), "{text}");
        assert!(
            text.contains("mine.txt"),
            "the error must name the files: {text}"
        );

        // …and nothing happened: their edit is still theirs, still uncommitted,
        // and the card's commit does not exist.
        let still = tokio::fs::read_to_string(repo_dir.path().join("mine.txt"))
            .await
            .unwrap();
        assert_eq!(still, "half a thought\n");
        assert_eq!(current_branch(repo_dir.path()).await.unwrap(), "wip");
        // `main`, not `--all`: the card's own branch is *supposed* to carry
        // that commit — committing the agent's work in its own worktree is
        // what the guard deliberately runs before refusing. What must not have
        // happened is that commit reaching the base branch.
        let log = git(repo_dir.path(), &["log", "main", "--oneline"])
            .await
            .unwrap();
        assert!(
            !log.contains("eren: card"),
            "the card landed on main anyway: {log}"
        );
    }

    /// The same bug wearing a different hat, and the nastier one.
    ///
    /// Already on the base branch with something staged: `checkout` is a no-op
    /// so nothing refuses, the squash stages on top, and the `diff --cached`
    /// check sees the *person's* staged work — so it commits it under the
    /// card's message.
    #[tokio::test]
    async fn staged_work_on_the_base_branch_is_not_committed_under_the_cards_message() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("theirs.txt"), "agent\n")
            .await
            .unwrap();

        // Staged, on main, which is where a person often is.
        tokio::fs::write(repo_dir.path().join("staged.txt"), "mine\n")
            .await
            .unwrap();
        git(repo_dir.path(), &["add", "staged.txt"]).await.unwrap();

        let err = mgr
            .squash_merge(repo_dir.path(), &wt, "main", "eren: card")
            .await
            .expect_err("staged work must refuse");
        assert!(err.to_string().contains("staged.txt"), "{err}");

        let log = git(repo_dir.path(), &["log", "--oneline"]).await.unwrap();
        assert!(
            !log.contains("eren: card"),
            "their staged work was committed: {log}"
        );
        // Still staged, still theirs to finish.
        let staged = git(repo_dir.path(), &["diff", "--cached", "--name-only"])
            .await
            .unwrap();
        assert!(staged.contains("staged.txt"));
    }

    /// Untracked files are not in the way, and counting them would be.
    ///
    /// `checkout` does not carry them and `merge --squash` does not stage
    /// them — while refusing on them would make any repository with untracked
    /// build output permanently unmergeable.
    #[tokio::test]
    async fn untracked_files_do_not_block_a_merge() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        tokio::fs::write(repo_dir.path().join("target.log"), "noise\n")
            .await
            .unwrap();

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("theirs.txt"), "agent\n")
            .await
            .unwrap();

        mgr.squash_merge(repo_dir.path(), &wt, "main", "eren: card")
            .await
            .expect("untracked files must not block a merge");
        let log = git(repo_dir.path(), &["log", "--oneline"]).await.unwrap();
        assert!(log.contains("eren: card"));
        // And the untracked file is untouched.
        assert!(repo_dir.path().join("target.log").exists());
    }

    /// A bare repository beside the real one, so push has somewhere to go
    /// without a network or a GitHub account.
    async fn with_bare_origin(repo: &Path) -> tempfile::TempDir {
        let bare = tempfile::tempdir().unwrap();
        git(repo, &["init", "--bare", bare.path().to_str().unwrap()])
            .await
            .unwrap();
        git(
            repo,
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        )
        .await
        .unwrap();
        bare
    }

    #[tokio::test]
    async fn push_sends_the_branch_and_the_uncommitted_work_with_it() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let bare = with_bare_origin(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        // Left uncommitted, exactly as a card in review can be.
        tokio::fs::write(wt.path.join("theirs.txt"), "agent\n")
            .await
            .unwrap();

        mgr.push(&wt, "eren: card", false).await.unwrap();

        // The ref is on the remote, and it carries the file.
        let refs = git(bare.path(), &["branch", "--list"]).await.unwrap();
        assert!(
            refs.contains(&wt.branch),
            "branch missing from origin: {refs}"
        );
        let files = git(bare.path(), &["ls-tree", "--name-only", &wt.branch])
            .await
            .unwrap();
        assert!(
            files.contains("theirs.txt"),
            "a pull request would have shown less than the diff did: {files}"
        );
    }

    #[tokio::test]
    async fn a_second_push_after_another_commit_fast_forwards() {
        // The ordinary case: a comment-driven fix run adds commits to a card
        // whose pull request is already open. GitHub updates it itself.
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let bare = with_bare_origin(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("a.txt"), "one\n")
            .await
            .unwrap();
        mgr.push(&wt, "eren: card", false).await.unwrap();

        tokio::fs::write(wt.path.join("b.txt"), "two\n")
            .await
            .unwrap();
        mgr.push(&wt, "eren: card", false).await.unwrap();

        let files = git(bare.path(), &["ls-tree", "--name-only", &wt.branch])
            .await
            .unwrap();
        assert!(
            files.contains("a.txt") && files.contains("b.txt"),
            "{files}"
        );
    }

    /// A retry from a clean checkout rewrites the branch under the same name,
    /// so the remote cannot fast-forward to it. That must be a question, not
    /// something that happens quietly — a force-push discards review comments
    /// on the commits it replaces.
    #[tokio::test]
    async fn a_rewritten_branch_is_refused_until_someone_asks_for_force() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let bare = with_bare_origin(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());
        let task_id = Uuid::new_v4();

        let wt = mgr
            .create(repo_dir.path(), "main", task_id, "card")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("first.txt"), "one\n")
            .await
            .unwrap();
        mgr.push(&wt, "eren: card", false).await.unwrap();

        // Retry-from-clean: same branch name, different history.
        mgr.remove(repo_dir.path(), &wt).await.unwrap();
        let again = mgr
            .create(repo_dir.path(), "main", task_id, "card")
            .await
            .unwrap();
        assert_eq!(
            again.branch, wt.branch,
            "the retry must reuse the name for this to bite"
        );
        tokio::fs::write(again.path.join("second.txt"), "two\n")
            .await
            .unwrap();

        let err = mgr
            .push(&again, "eren: card", false)
            .await
            .expect_err("a rewritten branch must not be force-pushed silently");
        let text = err.to_string();
        assert!(text.contains("overwrite"), "{text}");
        assert!(
            text.contains("review comments"),
            "the cost has to be stated: {text}"
        );

        // The remote still holds what a reviewer was looking at.
        let files = git(bare.path(), &["ls-tree", "--name-only", &again.branch])
            .await
            .unwrap();
        assert!(
            files.contains("first.txt") && !files.contains("second.txt"),
            "{files}"
        );

        // And force is the second, explicit answer.
        mgr.push(&again, "eren: card", true).await.unwrap();
        let files = git(bare.path(), &["ls-tree", "--name-only", &again.branch])
            .await
            .unwrap();
        assert!(files.contains("second.txt"), "{files}");
    }

    #[tokio::test]
    async fn pushing_without_a_remote_says_so_rather_than_failing_obscurely() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "card")
            .await
            .unwrap();
        let err = mgr.push(&wt, "eren: card", false).await.unwrap_err();
        assert!(err.to_string().contains("no `origin` remote"), "{err}");
    }

    #[tokio::test]
    async fn a_remote_is_reported_by_url_and_its_absence_is_not_an_error() {
        let repo_dir = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;

        // No remote is a condition to report, not a failure to log — which is
        // why this is an Option rather than a Result.
        assert_eq!(remote_url(repo_dir.path(), "origin").await, None);

        let bare = tempfile::tempdir().unwrap();
        git(
            repo_dir.path(),
            &["init", "--bare", bare.path().to_str().unwrap()],
        )
        .await
        .unwrap();
        git(
            repo_dir.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        )
        .await
        .unwrap();

        let url = remote_url(repo_dir.path(), "origin")
            .await
            .expect("origin is set");
        assert_eq!(
            url,
            bare.path().to_str().unwrap(),
            "trimmed, and no trailing newline"
        );
        // A remote that is not there is still None even when another one is.
        assert_eq!(remote_url(repo_dir.path(), "upstream").await, None);
    }

    #[test]
    fn only_a_history_disagreement_is_offered_force() {
        let rejected = "git push failed: ! [rejected]        eren/x -> eren/x (non-fast-forward)";
        assert!(push_rejection(rejected).is_some());
        assert!(push_rejection("! [rejected] main -> main (fetch first)").is_some());

        // Everything else is an ordinary failure, and offering force for it
        // would be advice that cannot help.
        assert_eq!(push_rejection("fatal: could not read Username"), None);
        assert_eq!(push_rejection("Permission denied (publickey)"), None);
        assert_eq!(push_rejection(""), None);
    }

    #[test]
    fn a_dirty_tree_is_described_without_becoming_the_whole_alert() {
        let short = " M src/main.rs\nA  src/new.rs\n";
        let described = describe_dirty(short);
        assert!(described.contains("M src/main.rs"));
        assert!(described.contains("A  src/new.rs"));
        assert!(
            !described.contains("and"),
            "nothing was elided: {described}"
        );

        let many: String = (0..40).map(|i| format!(" M file{i}.rs\n")).collect();
        let described = describe_dirty(&many);
        assert_eq!(described.lines().count(), 11, "ten files plus the tally");
        assert!(described.contains("… and 30 more"));

        assert_eq!(describe_dirty(""), "");
    }

    /// Clicking Merge twice must be quiet, not an error.
    ///
    /// The second squash stages nothing, and `git commit` exits non-zero for
    /// that — so committing unconditionally reported "Merge failed" for work
    /// that had merged perfectly well a moment earlier.
    #[tokio::test]
    async fn merging_an_already_merged_branch_is_a_no_op() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let mgr = WorktreeManager::new(root.path());

        let wt = mgr
            .create(repo_dir.path(), "main", Uuid::new_v4(), "twice")
            .await
            .unwrap();
        tokio::fs::write(wt.path.join("once.txt"), "one\n")
            .await
            .unwrap();

        mgr.squash_merge(repo_dir.path(), &wt, "main", "add once")
            .await
            .unwrap();
        let before = git(repo_dir.path(), &["rev-parse", "HEAD"]).await.unwrap();

        // Second time: succeeds, and creates no empty commit.
        mgr.squash_merge(repo_dir.path(), &wt, "main", "add once")
            .await
            .expect("merging twice should not fail");
        let after = git(repo_dir.path(), &["rev-parse", "HEAD"]).await.unwrap();
        assert_eq!(before, after, "a no-op merge must not add a commit");
    }

    /// git puts "nothing to commit" on stdout while exiting non-zero, so an
    /// error built from stderr alone read as "git commit … failed:" — a
    /// failure report containing no information at all.
    #[tokio::test]
    async fn git_errors_carry_whichever_stream_git_used() {
        let repo_dir = tempfile::tempdir().unwrap();
        init_repo(repo_dir.path()).await;
        let err = git(repo_dir.path(), &["commit", "-m", "empty"])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to commit"), "got: {err}");
    }
}
