//! Skills that came from somewhere else.
//!
//! `npx skills add owner/repo` installs an Agent Skill into the project: a
//! directory under `.agents/skills/<name>/` holding a `SKILL.md`, symlinked
//! into `.claude/skills/` for Claude Code and copied to `agent/skills/` for
//! the agents that read there. Alongside it the tool writes `skills-lock.json`
//! at the repository root, which is the manifest of what is installed and
//! where each skill came from.
//!
//! This module is the reading half, and it is pure: given the text of a
//! `SKILL.md` and of a lockfile, produce what Eren needs to mirror them into
//! its own library. Nothing here spawns anything or touches a database, so the
//! awkward parts — a description written as a folded block, a lockfile from a
//! newer version of the tool — are testable against real files.
//!
//! ## Somebody else's format
//!
//! The frontmatter is parsed with `serde_yaml` rather than by hand, and that
//! is not fussiness. Real skills in the wild write `description:` three
//! different ways in the same repository — a plain scalar, a double-quoted
//! string, and a folded block spanning five lines — and carry keys Eren has
//! never heard of (`license`, `metadata.author`, `metadata.version`).
//!
//! Unknown keys are **ignored, not refused**. That is the opposite of the rule
//! for an app manifest, where an unknown key is a typo in something the user
//! wrote and refusing it is a kindness. This file was written by a stranger
//! against a spec that is still moving, and refusing to install a skill
//! because it declared a field we have not heard of would make Eren the
//! thing that is broken.

use serde::Deserialize;

/// What a `SKILL.md` says, reduced to the three things Eren stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDoc {
    /// The frontmatter's `name`. Lowercase with hyphens, by the spec — but
    /// taken as written rather than validated, because it is the key the
    /// installer already used for the directory on disk.
    pub name: String,
    /// One line on what the skill is for. The spec calls it required; a skill
    /// missing it still installs, so an empty string here rather than an
    /// error — the person can see the body and write their own.
    pub description: String,
    /// Everything after the frontmatter: the instructions themselves.
    pub body: String,
}

/// Only the two fields Eren reads. `serde` ignores the rest by default,
/// which is the behaviour this format needs.
#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: String,
    #[serde(default)]
    description: String,
}

/// Split `---\n…\n---\n` off the front. Returns (frontmatter, body).
///
/// Returns `None` when there is no frontmatter at all, which is a skill this
/// cannot mirror — the name lives in there.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    // Normalising CRLF by allocating would cost a copy of every skill; the
    // markers are matched with the carriage return optional instead.
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    for marker in ["\n---\n", "\n---\r\n", "\r\n---\r\n", "\r\n---\n"] {
        if let Some(end) = rest.find(marker) {
            return Some((&rest[..end], &rest[end + marker.len()..]));
        }
    }
    // A file that opens a fence and never closes it: the whole thing is
    // frontmatter and there is no body, which is not a skill.
    None
}

/// Read a `SKILL.md`.
pub fn parse_skill_md(text: &str) -> Result<SkillDoc, String> {
    let (front, body) = split_frontmatter(text)
        .ok_or("this file has no SKILL.md frontmatter — the --- block naming the skill")?;
    let fm: FrontMatter = serde_yaml::from_str(front)
        .map_err(|e| format!("could not read the skill's frontmatter: {e}"))?;
    let name = fm.name.trim().to_string();
    if name.is_empty() {
        return Err("the skill's frontmatter has no name".into());
    }
    Ok(SkillDoc {
        name,
        // A folded block arrives with its newlines already turned into
        // spaces by YAML, but it can still carry a trailing one.
        description: fm
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        body: body.trim().to_string(),
    })
}

/// One row of `skills-lock.json`: what is installed, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockEntry {
    /// The installed directory name, and the key Eren mirrors under.
    pub name: String,
    /// `vercel-labs/agent-skills` — where it came from.
    pub source: String,
    /// The path inside that repository, so a repo holding nine skills can say
    /// which one this is.
    pub skill_path: String,
    /// The installer's own content hash. Stored on the mirror row so a later
    /// sync can tell "the file changed" from "nothing has happened".
    pub hash: String,
}

#[derive(Debug, Deserialize)]
struct LockFile {
    #[serde(default)]
    skills: std::collections::BTreeMap<String, LockSkill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockSkill {
    #[serde(default)]
    source: String,
    #[serde(default)]
    skill_path: String,
    #[serde(default)]
    computed_hash: String,
}

/// Read `skills-lock.json`.
///
/// Sorted by name, because a `BTreeMap` is what makes two installs of the same
/// repository produce the same list in the same order — the UI shows it and a
/// list that reshuffles reads as though something changed.
///
/// The `version` field is deliberately not checked. A lockfile from a newer
/// installer will still have the parts read here, and refusing to mirror
/// anything because a number went up would break Eren on somebody else's
/// release schedule.
pub fn parse_lock(json: &str) -> Result<Vec<LockEntry>, String> {
    let lock: LockFile =
        serde_json::from_str(json).map_err(|e| format!("could not read skills-lock.json: {e}"))?;
    // A name becomes a path segment under `.agents/skills/`, and the lockfile
    // was written from what a registry said. One segment, or the file is not
    // one Eren reads.
    if let Some(bad) = lock.skills.keys().find(|n| !is_single_segment(n)) {
        return Err(format!(
            "skills-lock.json names a skill {bad:?}, which is not a folder name"
        ));
    }
    Ok(lock
        .skills
        .into_iter()
        .map(|(name, s)| LockEntry {
            name,
            source: s.source,
            skill_path: s.skill_path,
            hash: s.computed_hash,
        })
        .collect())
}

/// One folder name: not empty, no separators, not `.` or `..`, no NUL.
fn is_single_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// What `npx skills add` will be handed, from what a person typed.
///
/// Strict, and the strictness is the point: this becomes an argument to a
/// spawned process, so the defence is the charset rather than the quoting —
/// the same reasoning the app manifest's identifiers are a closed set. It is
/// passed as one argv element and never through a shell, but a value that is
/// not a repository reference has no business being tried at all.
///
/// Accepts what a person actually has to hand: `owner/repo`, a GitHub URL, or
/// a skills.sh page. Anything else is refused by name rather than passed on
/// for the installer to fail at.
pub fn normalise_ref(input: &str) -> Result<String, String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("say which skill repository to install, as owner/repo".into());
    }
    // A URL from either site: take the first two path segments. Both
    // github.com/owner/repo and skills.sh/owner/repo are shaped that way, and
    // a deeper path (a tree link, a skill page) still names the repo first.
    let path = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .map(|rest| match rest.split_once('/') {
            Some((host, p))
                if host.eq_ignore_ascii_case("github.com")
                    || host.eq_ignore_ascii_case("www.github.com")
                    || host.eq_ignore_ascii_case("skills.sh")
                    || host.eq_ignore_ascii_case("www.skills.sh") =>
            {
                Ok(p)
            }
            _ => Err(format!("{raw} is not a github.com or skills.sh address")),
        })
        .transpose()?
        .unwrap_or(raw);
    // Drop a query or fragment before counting segments.
    let path = path.split(['?', '#']).next().unwrap_or(path);

    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
        return Err(format!("{raw} does not name a repository — use owner/repo"));
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            // Never a leading dash: the reference is passed as an argv
            // element, and a value starting with one is read as a flag rather
            // than as a repository. Never a leading dot either, which is how
            // `..` walks out of the path the installer is given.
            && !s.starts_with(['-', '.'])
    };
    if !ok(owner) || !ok(repo) {
        return Err(format!(
            "{raw} does not look like a repository — owner and name may use letters, \
             digits, dot, dash and underscore"
        ));
    }
    Ok(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frontmatter copied from vercel-labs/agent-skills, which is what the
    /// installer actually put on disk — the three shapes below all appear in
    /// that one repository.
    #[test]
    fn reads_a_plain_description() {
        let doc = parse_skill_md(
            "---\nname: deploy-to-vercel\ndescription: Deploy applications and websites to Vercel.\nmetadata:\n  author: vercel\n  version: \"3.0.0\"\n---\n\n# Deploy to Vercel\n\nDeploy any project.\n",
        )
        .unwrap();
        assert_eq!(doc.name, "deploy-to-vercel");
        assert_eq!(
            doc.description,
            "Deploy applications and websites to Vercel."
        );
        assert!(doc.body.starts_with("# Deploy to Vercel"));
        assert!(doc.body.ends_with("Deploy any project."), "body is trimmed");
    }

    #[test]
    fn reads_a_folded_description_as_one_line() {
        // The real vercel-composition-patterns skill writes it this way.
        let doc = parse_skill_md(
            "---\nname: vercel-composition-patterns\ndescription:\n  React composition patterns that scale. Use when refactoring\n  components with boolean prop proliferation.\n---\n\nbody\n",
        )
        .unwrap();
        assert_eq!(
            doc.description,
            "React composition patterns that scale. Use when refactoring components with boolean prop proliferation."
        );
    }

    #[test]
    fn reads_a_quoted_description_and_ignores_keys_we_do_not_know() {
        // `license` is a real top-level key on vercel-react-best-practices.
        // Refusing it would make Eren the broken one.
        let doc = parse_skill_md(
            "---\nname: vercel-optimize\nlicense: MIT\ndescription: \"Use for cost and performance work: collect metrics first.\"\nmetadata:\n  version: \"1.2.0\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(doc.name, "vercel-optimize");
        assert!(doc.description.starts_with("Use for cost"));
    }

    #[test]
    fn a_missing_description_is_not_a_failure() {
        // The spec calls it required; a skill without one still installs, and
        // refusing to mirror it would leave the library disagreeing with disk.
        let doc = parse_skill_md("---\nname: bare\n---\nbody\n").unwrap();
        assert_eq!(doc.description, "");
        assert_eq!(doc.body, "body");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_skill() {
        assert!(parse_skill_md("# just a readme\n").is_err());
        // Opened and never closed: everything would be frontmatter and there
        // would be no instructions at all.
        assert!(parse_skill_md("---\nname: x\nstill going\n").is_err());
        // Present but empty.
        assert!(parse_skill_md("---\nname: \"  \"\n---\nbody").is_err());
    }

    #[test]
    fn survives_windows_line_endings() {
        let doc = parse_skill_md("---\r\nname: x\r\ndescription: y\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(doc.name, "x");
        assert_eq!(doc.description, "y");
    }

    #[test]
    fn reads_the_lockfile_the_installer_writes() {
        // Trimmed from the real skills-lock.json.
        let entries = parse_lock(
            r#"{"version":1,"skills":{
                 "deploy-to-vercel":{"source":"vercel-labs/agent-skills","sourceType":"github",
                   "skillPath":"skills/deploy-to-vercel/SKILL.md","computedHash":"03e0eaaa"},
                 "vercel-optimize":{"source":"vercel-labs/agent-skills","sourceType":"github",
                   "skillPath":"skills/vercel-optimize/SKILL.md","computedHash":"ad0ef9c5"}}}"#,
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        // Sorted, so the list does not reshuffle between installs.
        assert_eq!(entries[0].name, "deploy-to-vercel");
        assert_eq!(entries[0].source, "vercel-labs/agent-skills");
        assert_eq!(entries[0].skill_path, "skills/deploy-to-vercel/SKILL.md");
        assert_eq!(entries[0].hash, "03e0eaaa");
        assert_eq!(entries[1].name, "vercel-optimize");
    }

    #[test]
    fn a_newer_lockfile_version_still_reads() {
        // Eren must not break because somebody else shipped a release.
        let entries = parse_lock(
            r#"{"version":9,"generatedBy":"skills@2","skills":{"a":{"source":"o/r","skillPath":"p","computedHash":"h","newField":true}}}"#,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "o/r");
    }

    #[test]
    fn an_empty_lockfile_is_not_an_error() {
        assert_eq!(parse_lock(r#"{"version":1,"skills":{}}"#).unwrap(), vec![]);
        assert!(parse_lock("not json").is_err());
    }

    #[test]
    fn normalises_every_way_a_person_has_the_reference() {
        for input in [
            "vercel-labs/agent-skills",
            "  vercel-labs/agent-skills/  ",
            "https://github.com/vercel-labs/agent-skills",
            "https://github.com/vercel-labs/agent-skills.git",
            "https://www.skills.sh/vercel-labs/agent-skills",
            "https://skills.sh/vercel-labs/agent-skills?tab=readme",
            // A deeper link still names the repository first.
            "https://github.com/vercel-labs/agent-skills/tree/main/skills",
        ] {
            assert_eq!(
                normalise_ref(input).unwrap(),
                "vercel-labs/agent-skills",
                "failed on {input}"
            );
        }
    }

    #[test]
    fn refuses_anything_that_is_not_a_repository_reference() {
        for bad in [
            "",
            "   ",
            "just-a-name",
            "https://example.com/owner/repo",
            // The charset is the defence, not the quoting: this becomes an
            // argument to a spawned process.
            "owner/repo; rm -rf /",
            "owner/../../etc/passwd",
            "-a/--flag",
            "own er/repo",
        ] {
            assert!(normalise_ref(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_reference_never_starts_with_a_dash() {
        // Otherwise the installer would read it as a flag rather than a
        // repository, which is how an argv becomes an option injection.
        assert!(normalise_ref("-rf/x").is_err());
        assert!(normalise_ref("x/-rf").is_err());
    }
}
