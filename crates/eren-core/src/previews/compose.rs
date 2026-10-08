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
//! user's real stack is using. (`container_name` would defeat that, so it is
//! stripped like a port.)
//!
//! **The file is vetted against a closed allow-list before it is written.** A
//! compose file is agent-written code — a card's branch, or a recipe an agent
//! proposed — and compose is a very expressive way to ask for the host:
//! `privileged: true`, a bind mount of `/` or of the Docker socket,
//! `pid: host`, `network_mode: host`, added capabilities, devices, an
//! `extends` of a file outside the branch, a named volume whose
//! `driver_opts` bind a host path. Stripping `ports` alone left every one of
//! those in. So [`vet`] walks the document with a closed list of the keys a
//! preview may use, refuses — naming the service and the key — anything that
//! reaches outside the preview, and refuses anything it does not recognise
//! rather than letting it through: a new compose feature is a decision here,
//! not a surprise. Refused, not stripped, because the person about to click
//! Preview should know their branch asked for it. Paths (build contexts,
//! `env_file`, bind-mount sources) must stay inside the stack's own folder.
//! Then [`Plan::render`] gives every service the same memory, CPU, pid and
//! no-new-privileges limits a single-container preview gets.
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
        harden_every_service(&mut doc);
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

/// The limits a single-container preview gets (`docker::run`), on every
/// service of a stack. `vet` has already refused a file's own `security_opt`,
/// so the one written here is the only one.
fn harden_every_service(doc: &mut Value) {
    let Some(services) = doc.get_mut("services").and_then(Value::as_mapping_mut) else {
        return;
    };
    for (_, spec) in services.iter_mut() {
        let Some(map) = spec.as_mapping_mut() else {
            continue;
        };
        map.insert(
            Value::String("security_opt".into()),
            Value::Sequence(vec![Value::String("no-new-privileges:true".into())]),
        );
        map.insert(
            Value::String("mem_limit".into()),
            Value::String(super::docker::PREVIEW_MEMORY.into()),
        );
        map.insert(
            Value::String("cpus".into()),
            Value::Number(super::docker::PREVIEW_CPUS.into()),
        );
        map.insert(
            Value::String("pids_limit".into()),
            Value::Number(super::docker::PREVIEW_PIDS.into()),
        );
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
    /// A key that reaches outside the preview — the host's filesystem,
    /// namespaces, privileges — on a service (or, with `service: None`, at
    /// the top level). `why` says what it would reach.
    Refused {
        service: Option<String>,
        key: String,
        why: &'static str,
    },
    /// A key the allow-list does not know. Refused rather than passed
    /// through, because compose keeps growing ways to ask for the host.
    Unsupported {
        service: Option<String>,
        key: String,
    },
}

impl ComposeError {
    pub fn message(&self) -> String {
        let place = |service: &Option<String>| match service {
            Some(s) => format!("on service `{s}`"),
            None => "at the top level".to_string(),
        };
        match self {
            Self::Unparseable(why) => {
                format!("This project's compose file could not be read: {why}")
            }
            Self::NoServices => "This project's compose file defines no services.".to_string(),
            Self::Refused { service, key, why } => format!(
                "This project's compose file asks for `{key}` {}, which {why}. Eren will not \
                 run it: remove it from the branch, or preview a Dockerfile instead.",
                place(service)
            ),
            Self::Unsupported { service, key } => format!(
                "This project's compose file uses `{key}` {}, which Eren's previews do not \
                 support. Remove it from the branch, or preview a Dockerfile instead.",
                place(service)
            ),
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

    // Before anything is rewritten: a file that asks for the host is refused
    // whole, with the service and key named, never partly run.
    vet(&doc)?;
    let services = doc
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or(ComposeError::NoServices)?;

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
    // Postgres just as surely as a frontend would. `container_name` goes the
    // same way: it is used verbatim, outside the project namespace, so it
    // would collide with — or be — the user's real container.
    for (_, spec) in services.iter_mut() {
        if let Some(map) = spec.as_mapping_mut() {
            map.remove(Value::String("ports".into()));
            map.remove(Value::String("container_name".into()));
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

/// Whether `text` is a compose file at all — parses, and declares at least
/// one service — whatever [`vet`] will say about it. For telling a stack from
/// a Dockerfile; a refused stack is still a stack, and is refused by name
/// when it is built rather than mistaken for prose.
pub fn looks_like_stack(text: &str) -> bool {
    serde_yaml::from_str::<Value>(text)
        .ok()
        .and_then(|doc| doc.get("services")?.as_mapping().map(|m| !m.is_empty()))
        .unwrap_or(false)
}

/// Top-level keys a stack may use. `x-*` extension fields are allowed too.
const TOP_LEVEL_KEYS: &[&str] = &["version", "name", "services", "volumes", "networks"];

/// Top-level keys that read files outside the stack.
const TOP_LEVEL_REFUSED: &[(&str, &str)] = &[
    ("include", "pulls in compose files from outside this one"),
    ("secrets", "reads files on the host"),
    ("configs", "reads files on the host"),
];

/// Service keys a preview may use. Closed: a key not here is refused by name.
const SERVICE_KEYS: &[&str] = &[
    "image",
    "build",
    "command",
    "entrypoint",
    "environment",
    "env_file",
    "expose",
    "ports",
    "container_name",
    "depends_on",
    "healthcheck",
    "working_dir",
    "user",
    "restart",
    "volumes",
    "networks",
    "labels",
    "stop_grace_period",
    "stop_signal",
    "init",
    "read_only",
    "tmpfs",
    "shm_size",
    "platform",
    "profiles",
    "deploy",
    "logging",
    "extra_hosts",
    "dns",
    "dns_search",
    "hostname",
    "domainname",
    "stdin_open",
    "tty",
    "cap_drop",
    "pull_policy",
    "links",
    "scale",
];

/// Service keys that reach outside the preview, each with what it reaches.
const SERVICE_REFUSED: &[(&str, &str)] = &[
    ("privileged", "runs the container with the host's own privileges"),
    ("cap_add", "adds kernel capabilities"),
    ("devices", "hands it the host's devices"),
    ("device_cgroup_rules", "hands it the host's devices"),
    ("pid", "shares a process namespace, the host's included"),
    ("ipc", "shares an IPC namespace, the host's included"),
    ("uts", "shares the host's UTS namespace"),
    ("cgroup", "shares the host's cgroup namespace"),
    ("cgroup_parent", "places it in a cgroup of its choosing"),
    ("network_mode", "joins another network namespace, the host's included"),
    ("userns_mode", "changes the user namespace"),
    ("security_opt", "changes the security profile Eren sets"),
    ("sysctls", "changes kernel parameters"),
    ("volumes_from", "mounts another container's volumes"),
    ("extends", "pulls in a service from a file outside this one"),
    ("secrets", "reads files on the host"),
    ("configs", "reads files on the host"),
    ("external_links", "links to containers outside this preview"),
    ("group_add", "adds host groups to the container's user"),
    ("runtime", "chooses the container runtime"),
    ("isolation", "chooses the isolation technology"),
    ("storage_opt", "sets storage driver options"),
    ("ulimits", "raises resource limits"),
    ("develop", "watches and syncs host paths"),
    ("credential_spec", "reads a credential file on the host"),
];

/// `build:` keys a preview may use.
const BUILD_KEYS: &[&str] = &[
    "context",
    "dockerfile",
    "dockerfile_inline",
    "args",
    "target",
    "labels",
    "cache_from",
    "cache_to",
    "no_cache",
    "pull",
    "tags",
    "platforms",
    "shm_size",
];

/// `build:` keys that reach outside the stack's own folder or the build sandbox.
const BUILD_REFUSED: &[(&str, &str)] = &[
    ("additional_contexts", "reads paths outside the stack's folder"),
    ("ssh", "hands the build an SSH agent or key"),
    ("secrets", "reads files on the host"),
    ("privileged", "runs build steps with the host's own privileges"),
    ("network", "joins another network namespace at build time"),
    ("extra_hosts", "changes the build's host resolution"),
    ("isolation", "chooses the isolation technology"),
    ("ulimits", "raises resource limits"),
    ("entitlements", "grants build entitlements"),
];

/// Whether a path a compose file names stays inside the stack's own folder:
/// relative, never climbing out, nothing the shell or compose would expand,
/// nothing fetched from elsewhere. Used for build contexts, Dockerfiles,
/// `env_file`s and bind-mount sources alike — every path is resolved against
/// the folder the compose file is in, which is the branch under review.
fn stays_inside(path: &str) -> bool {
    let p = path.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.starts_with('~')
        && !p.contains('$')
        && !p.contains("://")
        && !p.starts_with("git@")
        // `C:\…` and `C:/…`.
        && !(p.len() > 1 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic())
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// A short-form volume source that names a volume rather than a path.
fn is_named_volume(source: &str) -> bool {
    !source.is_empty()
        && !source.contains(['/', '\\'])
        && !source.starts_with(['.', '~', '$'])
}

/// The names of a mapping, for walking its keys.
fn keys(map: &serde_yaml::Mapping) -> impl Iterator<Item = &str> {
    map.keys().filter_map(Value::as_str)
}

/// Refuse a document that asks for anything outside the preview. See the
/// module comment for why this is an allow-list and why it refuses rather
/// than strips.
fn vet(doc: &Value) -> Result<(), ComposeError> {
    let Some(top) = doc.as_mapping() else {
        return Err(ComposeError::NoServices);
    };
    for key in keys(top) {
        if let Some((_, why)) = TOP_LEVEL_REFUSED.iter().find(|(k, _)| *k == key) {
            return Err(ComposeError::Refused {
                service: None,
                key: key.into(),
                why,
            });
        }
        if !TOP_LEVEL_KEYS.contains(&key) && !key.starts_with("x-") {
            return Err(ComposeError::Unsupported {
                service: None,
                key: key.into(),
            });
        }
    }
    vet_top_level_volumes(top)?;
    vet_top_level_networks(top)?;

    let Some(services) = top.get("services").and_then(Value::as_mapping) else {
        return Err(ComposeError::NoServices);
    };
    for (name, spec) in services {
        let name = name.as_str().unwrap_or("?").to_string();
        let refused = |key: &str, why: &'static str| ComposeError::Refused {
            service: Some(name.clone()),
            key: key.into(),
            why,
        };
        let Some(map) = spec.as_mapping() else {
            continue;
        };
        for key in keys(map) {
            if let Some((_, why)) = SERVICE_REFUSED.iter().find(|(k, _)| *k == key) {
                return Err(refused(key, why));
            }
            if !SERVICE_KEYS.contains(&key) && !key.starts_with("x-") {
                return Err(ComposeError::Unsupported {
                    service: Some(name.clone()),
                    key: key.into(),
                });
            }
        }
        if let Some(build) = map.get("build") {
            vet_build(&name, build)?;
        }
        if let Some(files) = map.get("env_file") {
            let paths: Vec<&Value> = match files {
                Value::Sequence(list) => list.iter().collect(),
                one => vec![one],
            };
            for entry in paths {
                let path = match entry {
                    Value::String(s) => Some(s.as_str()),
                    Value::Mapping(m) => m.get("path").and_then(Value::as_str),
                    _ => None,
                };
                if !path.is_some_and(stays_inside) {
                    return Err(refused("env_file", "reads a file outside the stack's folder"));
                }
            }
        }
        if let Some(Value::Sequence(volumes)) = map.get("volumes") {
            for entry in volumes {
                vet_volume(&name, entry)?;
            }
        }
    }
    Ok(())
}

fn vet_build(service: &str, build: &Value) -> Result<(), ComposeError> {
    let refused = |key: &str, why: &'static str| ComposeError::Refused {
        service: Some(service.to_string()),
        key: key.into(),
        why,
    };
    match build {
        Value::String(context) if stays_inside(context) => Ok(()),
        Value::String(_) => Err(refused("build", "builds from outside the stack's folder")),
        Value::Mapping(map) => {
            for key in keys(map) {
                if let Some((_, why)) = BUILD_REFUSED.iter().find(|(k, _)| *k == key) {
                    return Err(refused(&format!("build.{key}"), why));
                }
                if !BUILD_KEYS.contains(&key) && !key.starts_with("x-") {
                    return Err(ComposeError::Unsupported {
                        service: Some(service.to_string()),
                        key: format!("build.{key}"),
                    });
                }
            }
            for key in ["context", "dockerfile"] {
                if let Some(path) = map.get(key) {
                    if !path.as_str().is_some_and(stays_inside) {
                        return Err(refused(
                            &format!("build.{key}"),
                            "builds from outside the stack's folder",
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Err(ComposeError::Unsupported {
            service: Some(service.to_string()),
            key: "build".into(),
        }),
    }
}

fn vet_volume(service: &str, entry: &Value) -> Result<(), ComposeError> {
    let refused = |why: &'static str| ComposeError::Refused {
        service: Some(service.to_string()),
        key: "volumes".into(),
        why,
    };
    const HOST_PATH: &str = "mounts a path from the host";
    match entry {
        // `src:dst[:mode]`, or a bare container path (an anonymous volume).
        Value::String(short) => {
            // `C:\\code:/app` — a Windows drive, which the colon split below
            // would otherwise read as a volume called `C`.
            let b = short.as_bytes();
            if b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
                return Err(refused(HOST_PATH));
            }
            let mut parts = short.splitn(3, ':');
            let first = parts.next().unwrap_or("");
            let Some(_destination) = parts.next() else {
                return Ok(());
            };
            if is_named_volume(first) || stays_inside(first) {
                Ok(())
            } else {
                Err(refused(HOST_PATH))
            }
        }
        Value::Mapping(long) => {
            let kind = long.get("type").and_then(Value::as_str);
            let source = long.get("source").and_then(Value::as_str);
            match kind {
                Some("tmpfs") => Ok(()),
                Some("volume") => match source {
                    None => Ok(()),
                    Some(s) if is_named_volume(s) => Ok(()),
                    Some(_) => Err(refused(HOST_PATH)),
                },
                Some("bind") | None => match source {
                    None => Ok(()),
                    Some(s) if is_named_volume(s) || stays_inside(s) => Ok(()),
                    Some(_) => Err(refused(HOST_PATH)),
                },
                Some(_) => Err(refused("mounts something that is not a volume or a path")),
            }
        }
        _ => Ok(()),
    }
}

fn vet_top_level_volumes(top: &serde_yaml::Mapping) -> Result<(), ComposeError> {
    let Some(Value::Mapping(volumes)) = top.get("volumes") else {
        return Ok(());
    };
    for (name, spec) in volumes {
        let Some(map) = spec.as_mapping() else {
            continue;
        };
        for key in keys(map) {
            let why = match key {
                "driver" | "driver_opts" => "binds the volume to a host path or driver",
                "external" | "name" => "reuses a volume outside this preview's namespace",
                "labels" => continue,
                _ => {
                    return Err(ComposeError::Unsupported {
                        service: None,
                        key: format!("volumes.{}.{key}", name.as_str().unwrap_or("?")),
                    })
                }
            };
            return Err(ComposeError::Refused {
                service: None,
                key: format!("volumes.{}.{key}", name.as_str().unwrap_or("?")),
                why,
            });
        }
    }
    Ok(())
}

fn vet_top_level_networks(top: &serde_yaml::Mapping) -> Result<(), ComposeError> {
    let Some(Value::Mapping(networks)) = top.get("networks") else {
        return Ok(());
    };
    for (name, spec) in networks {
        let Some(map) = spec.as_mapping() else {
            continue;
        };
        let label = |key: &str| format!("networks.{}.{key}", name.as_str().unwrap_or("?"));
        for key in keys(map) {
            let why = match key {
                "external" | "name" => "joins a network outside this preview's namespace",
                "driver_opts" => "sets network driver options",
                "driver" if map.get("driver").and_then(Value::as_str) != Some("bridge") => {
                    "uses a network driver other than bridge, the host's included"
                }
                "driver" | "internal" | "labels" | "ipam" | "attachable" | "enable_ipv6" => {
                    continue
                }
                _ => {
                    return Err(ComposeError::Unsupported {
                        service: None,
                        key: label(key),
                    })
                }
            };
            return Err(ComposeError::Refused {
                service: None,
                key: label(key),
                why,
            });
        }
    }
    Ok(())
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

    /// A stack is agent-written code, and compose is a fluent way to ask for
    /// the host. Every key that does is refused with its name, whole.
    #[test]
    fn refuses_every_key_that_reaches_outside_the_preview() {
        for (key, _) in SERVICE_REFUSED {
            let text = format!("services:\n  api:\n    image: x\n    {key}: true\n");
            match plan(&text) {
                Err(ComposeError::Refused { service, key: k, .. }) => {
                    assert_eq!(service.as_deref(), Some("api"), "{key}");
                    assert_eq!(k, *key);
                }
                other => panic!("{key} was not refused: {other:?}"),
            }
        }
        for (key, _) in TOP_LEVEL_REFUSED {
            let text = format!("services: {{web: {{image: x}}}}\n{key}: {{}}\n");
            assert!(
                matches!(plan(&text), Err(ComposeError::Refused { service: None, .. })),
                "{key}"
            );
        }
        // The message names the place and the key, which is what the person
        // about to click Preview needs.
        let m = plan("services: {api: {privileged: true}}").unwrap_err().message();
        assert!(m.contains("`privileged`") && m.contains("`api`"), "{m}");
    }

    #[test]
    fn an_unknown_service_key_is_refused_not_ignored() {
        for text in [
            "services: {web: {image: x, made_up: 1}}",
            "services: {web: {image: x, build: {context: ., ssh: default}}}",
            "services: {web: {image: x}}\nmade_up: 1",
        ] {
            let e = plan(text).unwrap_err();
            assert!(
                matches!(
                    e,
                    ComposeError::Unsupported { .. } | ComposeError::Refused { .. }
                ),
                "{text}: {e:?}"
            );
        }
        // Extension fields are the one open door, by compose's own rule.
        assert!(plan("x-shared: {a: 1}\nservices: {web: {image: x, x-note: hi}}").is_ok());
    }

    #[test]
    fn a_host_path_bind_is_refused_a_relative_one_is_not() {
        for src in [
            "/var/run/docker.sock",
            "/",
            "~/.ssh",
            "../../etc",
            "${HOME}",
            "$PWD/x",
            "C:\\Users",
        ] {
            let text = format!("services:\n  web:\n    image: x\n    volumes: ['{src}:/x']\n");
            assert!(
                matches!(plan(&text), Err(ComposeError::Refused { key, .. }) if key == "volumes"),
                "{src}"
            );
        }
        let long = "services:\n  web:\n    image: x\n    volumes:\n      - type: bind\n        source: /etc\n        target: /x\n";
        assert!(matches!(plan(long), Err(ComposeError::Refused { .. })));

        for ok in ["./src:/app", "src:/app:ro", "data:/data", "/just/a/container/path"] {
            let text = format!("services:\n  web:\n    image: x\n    volumes: ['{ok}']\n");
            assert!(plan(&text).is_ok(), "{ok}");
        }
        let long_ok = "services:\n  web:\n    image: x\n    volumes:\n      - type: bind\n        source: ./src\n        target: /app\n      - type: volume\n        source: data\n        target: /data\n      - type: tmpfs\n        target: /tmp\n";
        assert!(plan(long_ok).is_ok());
    }

    #[test]
    fn a_remote_or_climbing_build_context_is_refused() {
        for ctx in ["../other", "/srv/code", "https://github.com/x/y.git", "git@github.com:x/y.git", "~/code"] {
            let short = format!("services: {{web: {{build: '{ctx}'}}}}");
            assert!(matches!(plan(&short), Err(ComposeError::Refused { .. })), "{ctx}");
            let long = format!("services: {{web: {{build: {{context: '{ctx}'}}}}}}");
            assert!(matches!(plan(&long), Err(ComposeError::Refused { .. })), "{ctx}");
        }
        assert!(matches!(
            plan("services: {web: {build: {context: ., dockerfile: ../Dockerfile}}}"),
            Err(ComposeError::Refused { .. })
        ));
        assert!(matches!(
            plan("services: {web: {image: x, env_file: ../.env}}"),
            Err(ComposeError::Refused { key, .. }) if key == "env_file"
        ));
        assert!(plan("services: {web: {build: {context: ./api, dockerfile: Dockerfile.dev, args: {A: 1}}, env_file: [.env, {path: .env.local, required: false}]}}").is_ok());
    }

    #[test]
    fn top_level_volumes_and_networks_stay_namespaced() {
        for text in [
            "services: {web: {image: x}}\nvolumes: {data: {external: true}}",
            "services: {web: {image: x}}\nvolumes: {data: {name: theirs}}",
            "services: {web: {image: x}}\nvolumes: {data: {driver: local, driver_opts: {type: none, o: bind, device: /}}}",
            "services: {web: {image: x}}\nnetworks: {app: {external: true}}",
            "services: {web: {image: x}}\nnetworks: {app: {driver: host}}",
            "services: {web: {image: x}}\nnetworks: {app: {driver_opts: {parent: eth0}}}",
        ] {
            assert!(
                matches!(plan(text), Err(ComposeError::Refused { service: None, .. })),
                "{text}"
            );
        }
        // What a real stack declares.
        assert!(plan(REAL).is_ok());
        assert!(plan("services: {web: {image: x}}\nvolumes: {data: null, logs: {labels: {a: b}}}\nnetworks: {app: {driver: bridge, internal: true}}").is_ok());
    }

    #[test]
    fn every_service_is_hardened_on_render() {
        let out = plan(REAL).unwrap().render(&ID, loopback(54321));
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let services = doc.get("services").unwrap().as_mapping().unwrap();
        assert_eq!(services.len(), 2);
        for (_, spec) in services {
            let opts = spec.get("security_opt").unwrap().as_sequence().unwrap();
            assert_eq!(opts, &[Value::String("no-new-privileges:true".into())], "{out}");
            assert_eq!(
                spec.get("mem_limit").and_then(Value::as_str),
                Some(super::super::docker::PREVIEW_MEMORY)
            );
            assert_eq!(
                spec.get("cpus").and_then(Value::as_u64),
                Some(super::super::docker::PREVIEW_CPUS)
            );
            assert_eq!(
                spec.get("pids_limit").and_then(Value::as_u64),
                Some(super::super::docker::PREVIEW_PIDS)
            );
        }
    }

    #[test]
    fn a_container_name_is_stripped_like_a_port() {
        let p = plan("services: {web: {image: x, container_name: my-real-app}}").unwrap();
        assert!(!p.stripped().contains("my-real-app"));
    }

    #[test]
    fn a_stack_is_recognised_even_when_it_will_be_refused() {
        let privileged = "services: {web: {image: x, privileged: true}}";
        assert!(looks_like_stack(privileged));
        assert!(plan(privileged).is_err());
        assert!(!looks_like_stack("FROM nginx"));
        assert!(!looks_like_stack("services: {}"));
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
