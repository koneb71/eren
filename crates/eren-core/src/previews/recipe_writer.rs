//! Ask an agent what this project needs, then make a person read it.
//!
//! Three of four projects on a typical machine have neither a Dockerfile nor a
//! compose file, so previews do not work for them at all. An agent can write
//! what is missing — but *which* file is missing is itself a judgement, and one
//! the evidence in the repo supports: a static site needs one container, and
//! something with a database and a separate API does not.
//!
//! So the agent decides and says which it chose. Getting that wrong in the
//! cheap direction — a Dockerfile for something that needs a stack — produces
//! a front end talking to nothing, which looks like a broken branch rather than
//! a wrong recipe.
//!
//! ## Why this is gated
//!
//! A Dockerfile is not configuration. `RUN` executes arbitrary commands at
//! build time, on this machine, with the network. Building an agent-written one
//! without a person reading it is executing agent-authored code on the host —
//! which is precisely the thing every other agent action in Eren stops to ask
//! about. So a written recipe is a *proposal*: stored, shown in full, and never
//! built until someone approves it. Same shape as an agent's wiki edit, and for
//! a much sharper reason.
//!
//! The agent is given no tools and never runs in the project directory. The
//! evidence is gathered here, by code, and pasted into the prompt — so what it
//! saw is exactly what the person reviewing can see, and a repository cannot
//! talk the agent into doing something on its way past.

use std::path::Path;

/// Which shape the agent decided this project needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One container is enough.
    Dockerfile,
    /// Several services that have to talk to each other.
    Compose,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dockerfile => "dockerfile",
            Self::Compose => "compose",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dockerfile" => Some(Self::Dockerfile),
            "compose" => Some(Self::Compose),
            _ => None,
        }
    }
}

/// What a project looks like, as far as building it is concerned.
#[derive(Debug, Clone, PartialEq)]
pub struct Survey {
    /// Paths at the root, so the agent can tell a monorepo from a single app.
    pub entries: Vec<String>,
    /// The files that actually decide how a thing is built, with their text.
    pub key_files: Vec<(String, String)>,
}

/// Files worth reading in full. Ordered: the first few matter most, and the
/// budget runs out from the end.
const KEY_FILES: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "Cargo.toml",
    "go.mod",
    "Gemfile",
    "composer.json",
    "pom.xml",
    "build.gradle",
    "Makefile",
    ".nvmrc",
    ".python-version",
    "compose.yaml",
    "docker-compose.yml",
];

/// How much of any one file to include. A lockfile-sized `package.json` is
/// still mostly noise after this, and the parts that decide a build — scripts,
/// engines, dependencies — are at the top.
const PER_FILE_BYTES: usize = 4_000;

pub async fn survey(dir: &Path) -> anyhow::Result<Survey> {
    let mut entries = Vec::new();
    let mut listing = tokio::fs::read_dir(dir).await?;
    while let Some(e) = listing.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        // Noise that tells you nothing and can be enormous.
        if matches!(
            name.as_str(),
            ".git" | "node_modules" | "target" | "dist" | ".venv"
        ) {
            continue;
        }
        let suffix = if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            "/"
        } else {
            ""
        };
        entries.push(format!("{name}{suffix}"));
    }
    entries.sort();

    let mut key_files = Vec::new();
    for name in KEY_FILES {
        let path = dir.join(name);
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            let clipped: String = text.chars().take(PER_FILE_BYTES).collect();
            key_files.push((name.to_string(), clipped));
        }
    }
    Ok(Survey { entries, key_files })
}

/// The prompt. Pure, so what the agent is asked is a thing you can read here
/// rather than reconstruct from logs.
pub fn prompt(survey: &Survey) -> String {
    let mut out = String::from(
        "Decide what this project needs in order to run, and write it, so a \
         reviewer can open it in a browser and look at it.\n\n\
         First choose ONE:\n\
         - A Dockerfile, if one container can serve the whole thing. Prefer this \
         when you can: it builds faster and there is less to go wrong.\n\
         - A docker compose file, if the project genuinely cannot start without \
         more than one service — a database it connects to on boot, a separate API \
         the frontend calls, a queue it reads. Do not reach for this merely \
         because the repo mentions one.\n\n\
         Then output ONLY that file, in a single fenced block, and tag the fence \
         with the choice: ```dockerfile or ```compose. No prose either side.\n\n\
         Either way:\n\
         - It must end with long-running server processes, not a build that exits.\n\
         - The service a person opens must declare the port it listens on — EXPOSE \
         in a Dockerfile, `ports:` in compose. That is what gets published, so it \
         must be right. (In compose, the host number you write is ignored and \
         replaced; only the container port is read.)\n\
         - Build everything from this repository. There are no mounted host paths, \
         no .env file, and no secrets at run time — anything needed must have a \
         working default.\n\
         - Do not fetch from a private registry.\n\
         - In compose, give any database a throwaway password and no host port.\n\
         - In compose, never ask for the host: no `privileged`, `cap_add`, `devices`, \
         `pid`, `network_mode`, `security_opt`, `extends`, `container_name`, no bind \
         mount of an absolute or `..` path, no external volume or network. Eren \
         refuses a stack that asks for any of them.\n\n",
    );
    out.push_str("Files at the project root:\n");
    for e in &survey.entries {
        out.push_str(&format!("  {e}\n"));
    }
    for (name, text) in &survey.key_files {
        out.push_str(&format!("\n--- {name} ---\n{text}\n"));
    }
    out
}

/// Pull the Dockerfile out of the reply.
///
/// Models put it in a fenced block even when told not to add prose, so the
/// fence is what we look for, with the whole reply as a fallback. Returns
/// `None` when there is nothing that looks like a Dockerfile at all — better a
/// clear failure than storing an apology as a build recipe.
pub fn extract(reply: &str) -> Option<(Kind, String)> {
    let (tag, body) = match fenced(reply) {
        Some(pair) => pair,
        None => (String::new(), reply.trim().to_string()),
    };

    // The fence tag is the agent's stated choice, but it is not trusted over
    // the content: a file that parses as compose is compose whatever the tag
    // says, and a tag is the easiest thing in a reply to get wrong.
    let looks_dockerfile = body
        .lines()
        .any(|l| l.trim_start().to_ascii_uppercase().starts_with("FROM "));
    // Recognised as a stack whether or not it will be allowed to run: a stack
    // that asks for the host is refused by name when it is built, which is a
    // better answer than "this is neither".
    let looks_compose = super::compose::looks_like_stack(&body);

    match (looks_compose, looks_dockerfile) {
        // A Dockerfile is not valid YAML with a `services:` key, so these are
        // not usually both true; when they are, the tag breaks the tie.
        (true, true) if tag.contains("docker") && !tag.contains("compose") => {
            Some((Kind::Dockerfile, body))
        }
        (true, _) => Some((Kind::Compose, body)),
        (false, true) => Some((Kind::Dockerfile, body)),
        // Neither: an apology, a question, or prose about Dockerfiles.
        (false, false) => None,
    }
}

/// The first fenced block, with its language tag.
fn fenced(reply: &str) -> Option<(String, String)> {
    let start = reply.find("```")?;
    let after = &reply[start + 3..];
    let tag_end = after.find('\n')?;
    let tag = after[..tag_end].trim().to_ascii_lowercase();
    let body = &after[tag_end + 1..];
    let end = body.find("```")?;
    Some((tag, body[..end].trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_dockerfile_out_of_a_fenced_reply() {
        let reply =
            "Here you go:\n\n```dockerfile\nFROM node:20\nEXPOSE 3000\n```\n\nHope that helps!";
        assert_eq!(
            extract(reply).unwrap(),
            (Kind::Dockerfile, "FROM node:20\nEXPOSE 3000".to_string())
        );
    }

    #[test]
    fn recognises_a_stack_when_the_agent_chooses_one() {
        let reply = "```compose\nservices:\n  web:\n    build: .\n    ports: [\"8080:80\"]\n  db:\n    image: postgres\n```";
        let (kind, body) = extract(reply).unwrap();
        assert_eq!(kind, Kind::Compose);
        assert!(body.contains("services:"));
        // The tag is not what decides it — content that parses as compose is
        // compose even when the agent tags the fence `yaml`.
        let (kind, _) = extract("```yaml\nservices:\n  web:\n    image: nginx\n```").unwrap();
        assert_eq!(kind, Kind::Compose);
    }

    #[test]
    fn a_stack_that_will_be_refused_is_still_a_stack() {
        // Classified as compose, so the refusal names the key at build time
        // instead of this reading as "neither a Dockerfile nor a stack".
        let (kind, body) =
            extract("```compose\nservices:\n  web:\n    image: nginx\n    privileged: true\n```")
                .unwrap();
        assert_eq!(kind, Kind::Compose);
        assert!(super::super::compose::plan(&body).is_err());
    }

    #[test]
    fn accepts_a_bare_dockerfile_with_no_fence() {
        assert_eq!(
            extract("FROM nginx\nEXPOSE 80").unwrap(),
            (Kind::Dockerfile, "FROM nginx\nEXPOSE 80".to_string())
        );
        // An unlabelled fence is still a fence.
        assert_eq!(
            extract("```\nFROM nginx\n```").unwrap(),
            (Kind::Dockerfile, "FROM nginx".to_string())
        );
    }

    #[test]
    fn refuses_a_reply_that_is_neither() {
        // The realistic failure: the model asks a question instead. Storing
        // that as a recipe would fail at build time with a baffling error.
        assert_eq!(extract("I need to know which port you want."), None);
        assert_eq!(extract(""), None);
        assert_eq!(extract("```\njust some text\n```"), None);
        assert_eq!(
            extract("```\n# This project has no FROM instruction\n```"),
            None
        );
        // YAML that is not a compose file is not a stack.
        assert_eq!(extract("```yaml\nname: hello\n```"), None);
    }

    #[test]
    fn the_prompt_carries_the_evidence_and_the_constraints() {
        let s = Survey {
            entries: vec!["package.json".into(), "src/".into()],
            key_files: vec![("package.json".into(), "{\"name\":\"app\"}".into())],
        };
        let p = prompt(&s);
        // The listing and the file contents both have to be in there, or the
        // agent is guessing.
        assert!(p.contains("package.json"));
        assert!(p.contains("{\"name\":\"app\"}"));
        assert!(p.contains("src/"));
        // The choice itself has to be asked for, or the agent always writes
        // the same thing.
        assert!(p.contains("```dockerfile"));
        assert!(p.contains("```compose"));
        // The constraints that make the result usable as a preview.
        assert!(p.contains("EXPOSE"));
        assert!(p.contains("long-running"));
        // No volumes and no secrets are the two that produce a container which
        // starts and then immediately dies if they are forgotten.
        assert!(p.contains("no mounted host paths"));
        assert!(p.contains("secrets"));
    }
}
