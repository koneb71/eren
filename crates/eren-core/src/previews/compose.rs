//! Preview a stack, not just a container.
//!
//! Most real projects are more than one process — a frontend, an API, a
//! database — and a preview that builds only the Dockerfile at the root shows
//! you a front end talking to nothing.
//!
//! ## The two things that make this safe
//!
//! **Ports are stripped and republished.** A compose file declares host ports:
//! `"${FRONTEND_PORT:-9000}:80"`, `"5173:5173"`. Bringing one up as written
//! seizes exactly the ports the user's own standing stacks run on — which is
//! the collision this whole feature exists to avoid. So every `ports:` entry is
//! removed and one service is republished on a free loopback port. Compose
//! *merges* port lists when you layer an override file, so removal has to
//! happen by rewriting the file; an override cannot take a binding away.
//!
//! **Everything is namespaced.** Compose prefixes networks, volumes and
//! container names with its project name, so running under
//! `eren-preview-<id>` means a preview's `app` network and `backend-data`
//! volume are its own — it cannot join, reuse, or later delete the ones the
//! user's real stack is using.
//!
//! Pure and tested: the rewriting is where a mistake would be expensive, and it
//! is decided entirely from the file's text.

use serde_yaml::Value;

/// Compose file names, in the order Compose itself looks for them.
pub const COMPOSE_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// Service names that usually mean "the thing a person opens".
const WEBBISH: &[&str] = &["web", "frontend", "www", "ui", "client", "app", "nginx"];

/// What we decided to do with a stack.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The service whose port gets published.
    pub service: String,
    /// The port *inside* that service's container.
    pub container_port: u16,
    /// Nothing said which port; we guessed. Travels so the UI can admit it.
    pub port_assumed: bool,
    /// Every service the stack will start, for showing what is about to run.
    pub services: Vec<String>,
    /// The parsed file with every host binding removed. Rendered by
    /// [`render`], which puts exactly one binding back.
    doc: Value,
}

impl Plan {
    /// The final compose file: no declared bindings, one loopback publication
    /// of the service we chose, and every image this stack *builds* under a
    /// name of Eren's own.
    ///
    /// The binding is written into the file rather than passed as a flag
    /// because `docker compose up` has no `--publish`. That is also why the
    /// port has to be chosen before the file is written — and it is why the
    /// image names go here too: `docker compose up --build` has no `--label`
    /// either, so the file is the only place to say either thing.
    pub fn render(&self, preview_id: &uuid::Uuid, publish: std::net::SocketAddr) -> String {
        let mut doc = self.doc.clone();
        namespace_built_images(&mut doc, preview_id);
        if let Some(spec) = doc
            .get_mut("services")
            .and_then(Value::as_mapping_mut)
            .and_then(|m| m.get_mut(Value::String(self.service.clone())))
            .and_then(Value::as_mapping_mut)
        {
            spec.insert(
                Value::String("ports".into()),
                Value::Sequence(vec![Value::String(format!(
                    "{publish}:{}",
                    self.container_port
                ))]),
            );
        }
        serde_yaml::to_string(&doc).unwrap_or_default()
    }

    /// The stripped file, for tests and for showing what will run.
    pub fn stripped(&self) -> String {
        serde_yaml::to_string(&self.doc).unwrap_or_default()
    }
}

/// Give every image this stack builds a name that is unmistakably Eren's.
///
/// The third thing that makes a stack preview safe, and the one that was
/// missing. Networks, volumes and container names are all namespaced by
/// compose's own project prefix; images are not, because their names come from
/// the file. So a preview of a project whose compose file says
/// `image: win11-frontend:latest` *built over* the image the user's own
/// `docker compose up` uses — and, worse, was invisible afterwards:
/// `image_disk_bytes` filters on Eren's label, compose applies no label, and
/// the disk figure read `0 B` while gigabytes sat there. Reclaiming by that tag
/// would have taken the user's image with it.
///
/// Under its own name neither can happen. The label goes on as well, so the
/// existing accounting works unchanged.
///
/// **Only services that build.** A service with `image:` and no `build:` is a
/// pulled base image — postgres, redis, nginx — shared with everything else on
/// the machine that uses it. Renaming would force a redundant pull; removing it
/// later would be spending somebody else's disk.
///
/// The cost is a full first build per preview instead of reusing the user's
/// image by tag. Layer cache still applies, and it is the same trade already
/// made for volumes.
fn namespace_built_images(doc: &mut Value, preview_id: &uuid::Uuid) {
    let prefix = super::recipe::container_name(preview_id);
    let Some(services) = doc.get_mut("services").and_then(Value::as_mapping_mut) else {
        return;
    };
    for (name, spec) in services.iter_mut() {
        let Some(name) = name.as_str().map(str::to_string) else {
            continue;
        };
        let Some(map) = spec.as_mapping_mut() else {
            continue;
        };
        if !map.contains_key(Value::String("build".into())) {
            continue;
        }
        map.insert(
            Value::String("image".into()),
            Value::String(format!("{prefix}-{}", slug(&name))),
        );
        label_build(map);
    }
}

/// Add Eren's label to a `build:` section, whichever of its two shapes it is.
///
/// `build: .` is shorthand for `build: { context: . }`, and a label cannot be
/// attached to the short form — so it is expanded first.
fn label_build(service: &mut serde_yaml::Mapping) {
    let key = Value::String("build".into());
    let build = service.get_mut(&key);
    let expanded = match build {
        Some(Value::String(context)) => {
            let mut m = serde_yaml::Mapping::new();
            m.insert(
                Value::String("context".into()),
                Value::String(context.clone()),
            );
            Some(m)
        }
        _ => None,
    };
    if let Some(m) = expanded {
        service.insert(key.clone(), Value::Mapping(m));
    }
    let Some(map) = service.get_mut(&key).and_then(Value::as_mapping_mut) else {
        return;
    };
    let mut labels = serde_yaml::Mapping::new();
    labels.insert(
        Value::String(super::docker::OWNER_LABEL.into()),
        Value::String("1".into()),
    );
    // Replaced rather than merged: the only label Eren cares about is its
    // own, and a file that already set it was set by a previous render.
    map.insert(Value::String("labels".into()), Value::Mapping(labels));
}

/// A service name that is safe in an image tag: lower-case, and nothing outside
/// what Docker accepts in a repository name.
fn slug(name: &str) -> String {
    let cleaned: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "service".into()
    } else {
        trimmed
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    Unparseable(String),
    NoServices,
}

impl ComposeError {
    pub fn message(&self) -> String {
        match self {
            Self::Unparseable(why) => {
                format!("This project's compose file could not be read: {why}")
            }
            Self::NoServices => "This project's compose file defines no services.".to_string(),
        }
    }
}

/// Read a stack and decide what to publish, stripping every host binding.
pub fn plan(text: &str) -> Result<Plan, ComposeError> {
    let mut doc: Value =
        serde_yaml::from_str(text).map_err(|e| ComposeError::Unparseable(e.to_string()))?;

    let services = doc
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or(ComposeError::NoServices)?;
    if services.is_empty() {
        return Err(ComposeError::NoServices);
    }

    let names: Vec<String> = services
        .keys()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();

    // Which service is the one you open, and on which port. Preference order:
    // a web-sounding name that publishes a port, then any service that
    // publishes one, then a web-sounding name at all, then the first service.
    let published: Vec<(String, u16)> = names
        .iter()
        .filter_map(|n| {
            container_port(services.get(Value::String(n.clone()))?).map(|p| (n.clone(), p))
        })
        .collect();

    let (service, container_port, port_assumed) = published
        .iter()
        .find(|(n, _)| is_webbish(n))
        .or_else(|| published.first())
        .map(|(n, p)| (n.clone(), *p, false))
        .unwrap_or_else(|| {
            let n = names
                .iter()
                .find(|n| is_webbish(n))
                .unwrap_or(&names[0])
                .clone();
            (n, super::recipe::ASSUMED_PORT, true)
        });

    // Strip host bindings everywhere. Not just from the published service:
    // a database that declares "5432:5432" would collide with the user's own
    // Postgres just as surely as a frontend would.
    for (_, spec) in services.iter_mut() {
        if let Some(map) = spec.as_mapping_mut() {
            map.remove(Value::String("ports".into()));
        }
    }

    Ok(Plan {
        service,
        container_port,
        port_assumed,
        services: names,
        doc,
    })
}

fn is_webbish(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    WEBBISH.iter().any(|w| lower == *w || lower.contains(w))
}

/// The container-side port a service publishes or exposes.
///
/// `"9000:80"` is host 9000 to container 80, so 80 is what we republish. The
/// host side is discarded on purpose — it is the number that would collide.
fn container_port(spec: &Value) -> Option<u16> {
    let map = spec.as_mapping()?;
    if let Some(ports) = map
        .get(Value::String("ports".into()))
        .and_then(Value::as_sequence)
    {
        for entry in ports {
            if let Some(p) = port_of_entry(entry) {
                return Some(p);
            }
        }
    }
    // `expose:` publishes nothing but does say which port the service serves.
    map.get(Value::String("expose".into()))
        .and_then(Value::as_sequence)?
        .iter()
        .find_map(|e| parse_port(&scalar(e)?))
}

fn port_of_entry(entry: &Value) -> Option<u16> {
    // Long form: `{ target: 80, published: 9000 }`.
    if let Some(map) = entry.as_mapping() {
        return map
            .get(Value::String("target".into()))
            .and_then(|t| parse_port(&scalar(t)?));
    }
    // Short form: "9000:80", "127.0.0.1:9000:80", "80", "9000:80/tcp".
    let text = scalar(entry)?;
    let text = text.split('/').next().unwrap_or(&text).to_string();
    let parts: Vec<&str> = text.split(':').collect();
    // The container port is always last.
    parse_port(parts.last()?)
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A literal port. `${WEB_PORT:-8080}` is not one — it resolves at compose
/// time, and guessing what it will become is how you publish the wrong thing.
fn parse_port(text: &str) -> Option<u16> {
    let t = text.trim();
    if t.contains('$') {
        return None;
    }
    t.parse::<u16>().ok().filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    /// A fixed id, so the expected image names in these tests are readable.
    const ID: uuid::Uuid = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);

    use super::*;

    fn loopback(port: u16) -> std::net::SocketAddr {
        (std::net::Ipv4Addr::LOCALHOST, port).into()
    }

    /// The user's own `windows11`, trimmed — the file this has to get right.
    const REAL: &str = r#"
services:
  backend:
    build: { context: ./backend }
    volumes: [backend-data:/data]
    networks: [app]
  frontend:
    build: { context: ., dockerfile: Dockerfile }
    ports:
      - "${FRONTEND_PORT:-9000}:80"
    networks: [app]
networks:
  app: { driver: bridge }
volumes:
  backend-data:
"#;

    #[test]
    fn publishes_the_web_service_on_its_container_port() {
        let p = plan(REAL).unwrap();
        assert_eq!(p.service, "frontend");
        // 80, not 9000. The host number is the one that would collide.
        assert_eq!(p.container_port, 80);
        assert!(!p.port_assumed);
        assert_eq!(p.services, vec!["backend", "frontend"]);
    }

    #[test]
    fn strips_every_host_binding_not_just_the_published_one() {
        // The regression that matters: bringing this up as written seizes 9000
        // and 5432, which are the ports the user's real stacks are on.
        let p = plan(
            r#"
services:
  web:
    ports: ["9000:80"]
  db:
    ports: ["5432:5432"]
"#,
        )
        .unwrap();
        let out = p.stripped();
        assert!(!out.contains("9000"), "{out}");
        assert!(!out.contains("5432"), "{out}");
        assert!(!out.contains("ports"), "{out}");
        // ...and the services themselves survive the surgery.
        assert!(out.contains("web"));
        assert!(out.contains("db"));
    }

    #[test]
    fn every_image_the_stack_builds_gets_a_name_of_our_own() {
        // The bug this pins, measured rather than imagined: the user's own
        // compose file says `image: win11-frontend:latest`, Eren built over
        // that tag, and the preview panel then reported `0 B of images` while
        // gigabytes sat there — `image_disk_bytes` filters on Eren's label
        // and compose applies none. Reclaiming by that tag would have taken
        // the image their own `docker compose up` uses.
        let out = plan(REAL).unwrap().render(&ID, loopback(54321));
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let services = doc.get("services").unwrap().as_mapping().unwrap();

        for name in ["backend", "frontend"] {
            let spec = services.get(Value::String(name.into())).unwrap();
            let image = spec
                .get(Value::String("image".into()))
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(image, format!("eren-preview-123456789abc-{name}"), "{out}");
            // And the label, so the existing accounting works unchanged.
            let labels = spec
                .get(Value::String("build".into()))
                .and_then(|b| b.get(Value::String("labels".into())))
                .unwrap_or_else(|| panic!("no build labels on {name}: {out}"));
            assert_eq!(
                labels.get(Value::String(super::super::docker::OWNER_LABEL.into())),
                Some(&Value::String("1".into())),
                "{out}"
            );
        }
    }

    #[test]
    fn a_pulled_image_is_left_exactly_as_it_was() {
        // postgres is shared with everything else on the machine that uses it.
        // Renaming would force a redundant pull; removing it later would be
        // spending somebody else's disk.
        let text = r#"
services:
  db:
    image: postgres:16
    ports: ["5432:5432"]
  web:
    build: .
    ports: ["3000:3000"]
"#;
        let out = plan(text).unwrap().render(&ID, loopback(5000));
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let services = doc.get("services").unwrap().as_mapping().unwrap();

        let db = services.get(Value::String("db".into())).unwrap();
        assert_eq!(
            db.get(Value::String("image".into())).unwrap().as_str(),
            Some("postgres:16"),
        );
        assert!(db.get(Value::String("build".into())).is_none(), "{out}");

        // …while the one that builds is renamed, and its shorthand `build: .`
        // is expanded so a label can be attached to it at all.
        let web = services.get(Value::String("web".into())).unwrap();
        assert_eq!(
            web.get(Value::String("image".into())).unwrap().as_str(),
            Some("eren-preview-123456789abc-web"),
        );
        assert_eq!(
            web.get(Value::String("build".into()))
                .and_then(|b| b.get(Value::String("context".into())))
                .and_then(Value::as_str),
            Some("."),
            "the shorthand has to survive expansion: {out}"
        );
    }

    #[test]
    fn a_service_that_both_builds_and_names_an_image_takes_our_name() {
        // The shape the user's own file has, and the one that used to collide.
        let text = "services:\n  api:\n    build: ./api\n    image: my-api:local\n";
        let out = plan(text).unwrap().render(&ID, loopback(5000));
        assert!(out.contains("eren-preview-123456789abc-api"), "{out}");
        assert!(
            !out.contains("my-api:local"),
            "the colliding tag must be gone: {out}"
        );
    }

    #[test]
    fn a_service_name_that_is_not_a_legal_tag_is_made_into_one() {
        let text = "services:\n  \"Web UI\":\n    build: .\n";
        let out = plan(text).unwrap().render(&ID, loopback(5000));
        assert!(out.contains("eren-preview-123456789abc-web-ui"), "{out}");
    }

    #[test]
    fn renders_exactly_one_loopback_binding_back() {
        let p = plan(REAL).unwrap();
        let out = p.render(&ID, loopback(54321));
        // One binding, on loopback, for the service we chose.
        assert!(out.contains("127.0.0.1:54321:80"), "{out}");
        assert_eq!(out.matches("127.0.0.1:").count(), 1, "{out}");
        // And the one it replaced is gone.
        assert!(!out.contains("9000"), "{out}");
        assert!(!out.contains("FRONTEND_PORT"), "{out}");
    }

    #[test]
    fn reads_every_shape_compose_allows_for_a_port() {
        let cases = [
            (r#"services: {web: {ports: ["80"]}}"#, 80),
            (r#"services: {web: {ports: ["9000:80"]}}"#, 80),
            (r#"services: {web: {ports: ["127.0.0.1:9000:80"]}}"#, 80),
            (r#"services: {web: {ports: ["9000:80/tcp"]}}"#, 80),
            (
                r#"services: {web: {ports: [{target: 8080, published: 9000}]}}"#,
                8080,
            ),
            (r#"services: {web: {expose: [3000]}}"#, 3000),
        ];
        for (yaml, want) in cases {
            let p = plan(yaml).unwrap_or_else(|e| panic!("{yaml}: {e:?}"));
            assert_eq!(p.container_port, want, "{yaml}");
            assert!(!p.port_assumed, "{yaml}");
        }
    }

    #[test]
    fn an_interpolated_port_is_not_a_port() {
        // `${WEB_PORT:-8080}:80` still yields 80 — the container side is
        // literal. But when the *container* side is a variable there is nothing
        // honest to read, so it falls through to the guess.
        let p = plan(r#"services: {web: {ports: ["${WEB_PORT:-8080}:80"]}}"#).unwrap();
        assert_eq!(p.container_port, 80);

        let p = plan(r#"services: {web: {ports: ["8080:${PORT}"]}}"#).unwrap();
        assert!(p.port_assumed);
        assert_eq!(p.container_port, super::super::recipe::ASSUMED_PORT);
    }

    #[test]
    fn prefers_a_web_sounding_service_over_whichever_came_first() {
        let p = plan(
            r#"
services:
  api: {ports: ["8000:8000"]}
  frontend: {ports: ["9000:80"]}
"#,
        )
        .unwrap();
        assert_eq!(p.service, "frontend");
        // With no web-sounding name, the first that publishes anything wins.
        let p = plan(r#"services: {api: {ports: ["8000:8000"]}, worker: {}}"#).unwrap();
        assert_eq!(p.service, "api");
    }

    #[test]
    fn a_stack_that_publishes_nothing_still_gets_a_guess() {
        let p = plan(r#"services: {web: {image: nginx}, db: {image: postgres}}"#).unwrap();
        assert_eq!(p.service, "web");
        assert!(p.port_assumed);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert!(matches!(plan("services:"), Err(ComposeError::NoServices)));
        assert!(matches!(
            plan("services: {}"),
            Err(ComposeError::NoServices)
        ));
        assert!(matches!(plan("name: x"), Err(ComposeError::NoServices)));
        assert!(matches!(
            plan("services: {web: {ports: [\"80\"]}"),
            Err(ComposeError::Unparseable(_))
        ));
    }
}
