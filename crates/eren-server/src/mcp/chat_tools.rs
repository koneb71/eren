//! Workspace tools MCP endpoint for chat runs: the project assistant calls
//! these to create/start/inspect tasks. Every tool resolves through the
//! chat's own project, so a chat can never touch another project's data.

use crate::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub async fn rpc(
    State(state): State<AppState>,
    Path((chat_id, run_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(id) = req.get("id").cloned() else {
        return (StatusCode::ACCEPTED, Json(Value::Null));
    };
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "eren", "version": env!("CARGO_PKG_VERSION") }
        }),
        "ping" => json!({}),
        // Which list depends on what the chat is scoped to: a space exposes
        // the document tools, everything else the board tools. A lookup
        // failure falls back to the board list — the tool call itself will
        // say what went wrong, with a better message than a silent empty
        // toolbox.
        // Plan mode narrows it further. Advertising a tool the run has denied
        // is the silent failure this endpoint exists to avoid: chat carries
        // `permission_prompt_tool: false`, so a refused call produces no
        // prompt to answer and the assistant reports the workspace as broken.
        "tools/list" => {
            let planning = planning(&state, chat_id).await;
            match chat_project(&state, chat_id).await {
                Ok((_, _, kind)) => tools_list(&kind, planning),
                Err(_) => tools_list("repo", planning),
            }
        }
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let outcome = match super::still_running(&state, run_id, "chat_id", chat_id).await {
                Ok(()) => call_tool(&state, chat_id, name, args).await,
                Err(e) => Err(e),
            };
            super::log_tool(&state, run_id, name, &outcome);
            match outcome {
                Ok(payload) => json!({
                    "content": [{ "type": "text", "text": payload.to_string() }]
                }),
                Err(msg) => json!({
                    "content": [{ "type": "text", "text": json!({"error": msg}).to_string() }],
                    "isError": true
                }),
            }
        }
        _ => {
            return (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                })),
            );
        }
    };
    (
        StatusCode::OK,
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
}

/// Is this chat planning rather than acting?
///
/// Read here rather than passed in, because the engine calls `tools/list`
/// itself and carries nothing but the chat id. Defaults to "no" on a lookup
/// failure — the run's own `denied_tools` is the real gate, so the worst a
/// wrong answer here does is advertise a tool that then refuses.
async fn planning(state: &AppState, chat_id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT plan_mode FROM chats WHERE id = $1")
        .bind(chat_id)
        .fetch_optional(&state.db.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

pub fn tools_list(kind: &str, planning: bool) -> Value {
    let obj = |props: Value, required: Vec<&str>| json!({ "type": "object", "properties": props, "required": required });
    // Offered everywhere, including plan mode and a space: a brief can be
    // ambiguous whatever the chat is scoped to, and asking is the *most*
    // plan-mode-appropriate thing an assistant can do.
    let ask = json!({
        "name": "ask_user",
        "description": "Ask the user a clarifying question with a small set of options, and stop. \
                        Reach for this when the request is ambiguous in a way that changes what \
                        you would actually do — two readings that lead to different cards, an \
                        unstated choice of approach, a scope you cannot infer. Do not use it for \
                        something you can settle by reading the code, for permission to proceed, \
                        or to confirm a decision the user already made. Your turn ends when you \
                        call this; their answer arrives as the next message and you keep the \
                        conversation.",
        "inputSchema": obj(json!({
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "description": "At most four. Ask the one that most changes what you would do.",
                "items": {
                    "type": "object",
                    "required": ["question", "options"],
                    "properties": {
                        "question": { "type": "string" },
                        "header": { "type": "string", "description": "two or three words, to tell several questions apart" },
                        "options": {
                            "type": "array", "minItems": 2, "maxItems": 4,
                            "items": {
                                "type": "object",
                                "required": ["label"],
                                "properties": {
                                    "label": { "type": "string" },
                                    "description": { "type": "string", "description": "what picking it means" }
                                }
                            }
                        },
                        "multiSelect": { "type": "boolean" }
                    }
                }
            }
        }), vec!["questions"])
    });
    // A space's toolbox is documents, not the board: relevant passages are
    // already injected per message, so the tools exist for digging — a
    // different phrasing, a full listing.
    if kind == "space" {
        // A space has no acting tools to take away, so plan mode is inert
        // here — and the orchestrator refuses to enter it for a space at all.
        return json!({
            "tools": [
                {
                    "name": "search_documents",
                    "description": "Search this space's documents semantically. Returns the best-matching passages with file names and scores. The user's message already carries the top matches — reach for this when you need a different phrasing or another angle, and open a file with Read for more than an excerpt.",
                    "inputSchema": obj(json!({
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 10 }
                    }), vec!["query"])
                },
                {
                    "name": "list_documents",
                    "description": "List the documents in this space with their index status.",
                    "inputSchema": obj(json!({}), vec![])
                },
                ask,
            ]
        });
    }
    let board = json!({
        "tools": [
            {
                "name": "create_task",
                "description": "Create a coding task on the project board. It runs in an isolated git worktree and its result appears for user review. Set start=true to launch immediately.",
                "inputSchema": obj(json!({
                    "title": { "type": "string" },
                    "prompt": { "type": "string", "description": "full instructions for the coding agent" },
                    "agent_name": { "type": "string", "description": "optional: bind a named agent from the library, spelled exactly as list_agents reports it. An unknown name is rejected. Omit it and a single agent the user @mentioned in their message is used instead." },
                    "goal": { "type": "string", "description": "optional: the goal this card serves, by its title exactly as the Goals section lists it — or by its path, \"Parent > Title\", when two goals share a title. Omit it and a card made during a manager pass serves that manager's goal." },
                    "skill_name": { "type": "string", "description": "optional: how this card's work should be done — a skill name exactly as list_skills reports it. A card takes one skill. Omit it and a single skill the user @mentioned is used; if they named several, say which one each card uses." },
                    "model_tier": { "type": "string", "enum": ["easy", "medium", "complex"] },
                    "start": { "type": "boolean" }
                }), vec!["title", "prompt"])
            },
            { "name": "start_task", "description": "Start a backlog task.",
              "inputSchema": obj(json!({ "task_id": { "type": "string" } }), vec!["task_id"]) },
            { "name": "list_tasks", "description": "List this project's tasks with status.",
              "inputSchema": obj(json!({}), vec![]) },
            { "name": "get_task_status", "description": "Status, cost, latest run, and whether the work passes the project's checks, for one task.",
              "inputSchema": obj(json!({ "task_id": { "type": "string" } }), vec!["task_id"]) },
            { "name": "list_agents", "description": "List available agents in this workspace.",
              "inputSchema": obj(json!({}), vec![]) },
            {
                "name": "cancel_task",
                "description": "Stop a task that is queued or running. Use it when the user says stop, or when you started something you should not have. It does not undo what the agent already wrote — the worktree and its diff survive for review. A task that already finished is reported as finished, which is not an error.",
                "inputSchema": obj(json!({ "task_id": { "type": "string" } }), vec!["task_id"])
            },
            {
                "name": "get_diff",
                "description": "What a task changed: one line per file, with lines added and removed. Use it to tell the user what a finished card did, or to judge whether it is worth their time to look at. Pass a path from that summary to read one file's actual diff, capped — it will be cut off, and asking for file after file to rebuild the whole change is not what this is for. A task that has not run has no diff.",
                "inputSchema": obj(json!({
                    "task_id": { "type": "string" },
                    "path": { "type": "string", "description": "optional: one file, spelled exactly as the summary reports it. Omit it for the summary." }
                }), vec!["task_id"])
            },
            {
                "name": "get_spend",
                "description": "What this project's runs have cost, and whether the queue will accept more work. Check it before starting anything large, and when the user asks what something cost. Runs still in flight report what they have spent so far, not what they will. Some engines never report a price; those runs are counted separately rather than folded in as zero. The budget and queue state are machine-wide, the totals are this project's.",
                "inputSchema": obj(json!({
                    "days": { "type": "integer", "description": "how far back to look, 1-365. Defaults to 7." }
                }), vec![])
            },
            {
                "name": "list_skills",
                "description": "The skills in this workspace: saved instructions for how the user wants a kind of job done. Use it to answer what skills they have, and to pass skill_name to create_task. You are given each skill's name and what it is for, never its text — the instructions are handed to whoever runs the task, so repeating them into a prompt would send them twice.",
                "inputSchema": obj(json!({}), vec![])
            },
            {
                "name": "move_task",
                "description": "File a card in a different column. Bookkeeping only: it changes nothing in git, and moving a card to done does not merge its branch. Use start_task to start a card. It refuses while the card is being worked on.",
                "inputSchema": obj(json!({
                    "task_id": { "type": "string" },
                    "column": { "type": "string", "enum": ["backlog", "review", "done"] }
                }), vec!["task_id", "column"])
            },
            {
                "name": "search_code",
                "description": "Search this project's code by meaning rather than by exact text. Use it when you do not know what a thing is called: \"where do we decide whether a card can start\" finds the function even though the question never says \"vet\". Returns paths with line numbers and excerpts; open a file with Read for more than an excerpt. Grep is still the better tool when you already know the exact string to look for.",
                "inputSchema": obj(json!({
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                }), vec!["query"])
            },
            // Deliberately absent: merge_task. `squash_merge` runs four git
            // commands in the user's real checkout, which is the same reason
            // Edit/Write/Bash are denied by name for chat runs — a front door
            // locked and a back door left open. There is also nowhere to ask:
            // chat runs carry `permission_prompt_tool: false`, so a
            // confirmation step has no channel to confirm on. Say the card is
            // ready and let them press Merge, where the diff is.
            //
            // And retry_task, whose only difference from start_task is
            // `fresh: true` — which deletes the worktree *and* the branch,
            // destroying the only copy of an unmerged diff.
            ask,
        ]
    });

    if !planning {
        return board;
    }
    // Plan mode: everything that reads stays, everything that acts goes. The
    // filter is over the same list rather than a second hand-written one, so a
    // tool added above cannot be quietly missing from the planning toolbox.
    let kept: Vec<Value> = board["tools"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter(|t| {
                    !eren_core::runs::chat_plan::ACTING_TOOL_NAMES
                        .contains(&t["name"].as_str().unwrap_or(""))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    json!({ "tools": kept })
}

/// Record a clarifying question and tell the assistant to stop.
///
/// The tool returns immediately rather than blocking on an answer, and that is
/// the design rather than a shortcut. A chat turn holds a concurrency permit
/// from the same queue as every other run, so a turn parked on a question
/// would sit on one of a small number of slots for as long as somebody takes
/// to look — and `routes::chat::active_run` counts it as active, so the very
/// conversation needed to answer would refuse the next message. The question
/// is stored, the turn ends, and answering starts the next one; the session
/// resumes, so the assistant carries on where it left off.
///
/// The returned text is written *at the model*: it has just called a tool and
/// needs to know that the right move now is to stop, not to guess and carry
/// on with the answer it hoped for.
async fn ask_user(state: &AppState, chat_id: Uuid, args: Value) -> Result<Value, String> {
    let questions: Vec<eren_core::runs::questions::Question> =
        serde_json::from_value(args.get("questions").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("questions must be a list of {{question, options}}: {e}"))?;
    let questions = eren_core::runs::questions::validate(questions)?;

    // One open question at a time. Two would give the person two cards to
    // answer for one turn, and only the last answer could be sent — the other
    // would sit there looking live forever.
    sqlx::query(
        "UPDATE chat_questions SET answered_at = now(),
                answer = '\"superseded\"'::jsonb
          WHERE chat_id = $1 AND answered_at IS NULL",
    )
    .bind(chat_id)
    .execute(&state.db.pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT INTO chat_questions (chat_id, run_id, questions)
         SELECT $1, r.id, $2
           FROM runs r
          WHERE r.chat_id = $1 AND r.status NOT IN ('completed','failed','canceled')
          ORDER BY r.created_at DESC LIMIT 1",
    )
    .bind(chat_id)
    .bind(serde_json::to_value(&questions).map_err(|e| e.to_string())?)
    .execute(&state.db.pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "asked": questions.len(),
        "next": "The question is now in front of the user. End your turn here — \
                 do not answer it yourself and do not carry on as though it had \
                 been answered. Their reply arrives as the next message and you \
                 will still have this conversation.",
    }))
}

async fn chat_project(state: &AppState, chat_id: Uuid) -> Result<(Uuid, Uuid, String), String> {
    let row = sqlx::query(
        "SELECT p.id AS project_id, p.workspace_id, p.kind FROM chats c
         JOIN projects p ON p.id = c.project_id WHERE c.id = $1",
    )
    .bind(chat_id)
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok((
        row.get("project_id"),
        row.get("workspace_id"),
        row.get("kind"),
    ))
}

async fn call_tool(
    state: &AppState,
    chat_id: Uuid,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let (project_id, workspace_id, kind) = chat_project(state, chat_id).await?;
    // The two toolboxes are disjoint on purpose, and the guard runs both
    // ways: a space chat calling create_task would run a coding agent
    // in-place inside a documents folder, and a repo chat calling
    // search_documents would search an index that does not exist.
    let is_space = kind == "space";
    // Asking a clarifying question belongs to neither toolbox and to both: a
    // space chat can be as ambiguously briefed as a repo one.
    if name == "ask_user" {
        return ask_user(state, chat_id, args).await;
    }
    let doc_tool = matches!(name, "search_documents" | "list_documents");
    if is_space && !doc_tool {
        return Err(
            "this chat is scoped to a document space — the board tools work in project chats"
                .into(),
        );
    }
    if !is_space && doc_tool {
        return Err(
            "this chat's project is a repository — the document tools work in spaces".into(),
        );
    }
    match name {
        "create_task" => {
            let title = unescape_html(
                args.get("title")
                    .and_then(Value::as_str)
                    .ok_or("title is required")?,
            );
            let title = title.as_str();
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or("prompt is required")?;
            let tier = match args.get("model_tier").and_then(Value::as_str) {
                Some(t @ ("easy" | "medium" | "complex")) => t,
                _ => "medium",
            };
            let (agent_id, agent_name) =
                resolve_agent(state, chat_id, workspace_id, args.get("agent_name")).await?;
            // A skill the user named with `@`, on the same reasoning as the
            // agent: they asked for the work to be done a particular way, and
            // whether that survives should not depend on the assistant
            // remembering to say so.
            let mentioned = eren_core::runs::mentions::latest_skills_for_chat(&state.db, chat_id)
                .await
                .map_err(|e| e.to_string())?;
            let skill =
                resolve_skill(state, workspace_id, args.get("skill_name"), &mentioned).await?;
            let skill_id = skill.as_ref().map(|(id, _)| *id);
            let start = args.get("start").and_then(Value::as_bool).unwrap_or(false);
            let pass = eren_core::manager::pass_for_chat(&state.db, chat_id).await;
            // A manager with reports delegates down its own tree, never
            // across it — refused here, with the names it may use.
            if let (Some(pass), Some(agent)) = (pass.as_ref(), agent_id) {
                eren_core::org_chart::may_delegate(&state.db, pass.id, agent)
                    .await
                    .map_err(|e| e.to_string())??;
            }
            // The goal it serves: named, or the manager's own.
            let goal_id = match args
                .get("goal")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|g| !g.is_empty())
            {
                Some(title) => Some(
                    eren_core::goals::by_title(&state.db, workspace_id, title)
                        .await
                        .map_err(|e| e.to_string())??,
                ),
                None => match pass.as_ref() {
                    Some(p) => eren_core::goals::of_pass(&state.db, p.id)
                        .await
                        .map_err(|e| e.to_string())?,
                    None => None,
                },
            };
            // Checked before the card exists, so a refusal does not leave a
            // half-made card behind. `None` for the task: this one is about to
            // be created here, so it cannot have come from outside — only the
            // cap applies.
            if start {
                vet_manager_start(state, pass.as_ref(), None).await?;
            }

            // Always born in the backlog, and promoted below once it has
            // actually started. It used to be inserted straight into 'running'
            // when `start=true`, which was fine only because nothing between
            // the insert and the enqueue could fail — and vetting, added
            // below, can. A card sitting in 'running' with no run behind it is
            // the one state the board cannot explain.
            let row = sqlx::query(
                "INSERT INTO tasks (project_id, title, prompt, model_tier, agent_id, skill_id, chat_id, board_column, engine, goal_id)
                 VALUES ($1,$2,$3,$4,$5,$7,$6,'backlog',$8,$9)
                 RETURNING id",
            )
            .bind(project_id)
            .bind(title)
            .bind(prompt)
            .bind(tier)
            .bind(agent_id)
            .bind(chat_id)
            .bind(skill_id)
            // Named rather than left to the column default, which is the
            // literal string 'claude-code' from migration 0001. On a machine
            // where the installed engine is something else, every card made
            // this way was queued onto an engine that is not there and failed
            // at dispatch. The HTTP create path has always resolved it; this
            // one inherited a default from before there was more than one
            // engine.
            .bind(state.orchestrator.default_engine())
            .bind(goal_id)
            .fetch_one(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            let task_id: Uuid = row.get("id");

            let run_id = if start {
                // The same vetting `start_task` and the Start button do. This
                // path went straight to `enqueue_task`, so `start=true` was a
                // way round the dependency check *and* the engine/permission
                // capability gate — a card blocked by two others, or a
                // Reviewed card on an engine that cannot ask, started anyway.
                // It mattered least when a person was watching the reply; it
                // matters most now that a timer can be the caller.
                crate::routes::tasks::vet_task(state, task_id)
                    .await
                    .map_err(|(_, message)| message)?;
                vet_spend(state, task_id).await?;
                // Recorded only once the card is definitely going to start —
                // after the vet, before the enqueue. Recording it earlier
                // burned a unit of the cap on a card the vet then refused,
                // and told the morning log it had started. `start_task` has
                // always had this order; this arm did not.
                if let Some(pass) = &pass {
                    eren_core::manager::record_start(&state.db, pass, task_id, title).await?;
                }
                let run_id = state
                    .orchestrator
                    .enqueue_task(task_id)
                    .await
                    .map_err(|e| e.to_string())?;
                sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
                    .bind(task_id)
                    .execute(&state.db.pool)
                    .await
                    .map_err(|e| e.to_string())?;
                Some(run_id)
            } else {
                // A card filed in the backlog is the manager doing its job
                // too, and a summary that showed only what it started would
                // read as though it had done nothing.
                if let Some(pass) = &pass {
                    eren_core::manager::record_action(
                        &state.db,
                        pass,
                        "create",
                        Some(task_id),
                        title,
                    )
                    .await;
                }
                None
            };
            // The bound agent is echoed back so the assistant reports what
            // actually happened rather than what it asked for — those differ
            // exactly when it left `agent_name` off and the user's `@mention`
            // supplied it.
            Ok(json!({
                "task_id": task_id,
                "run_id": run_id,
                "started": start,
                "agent": agent_name,
                "skill": skill.map(|(_, name)| name),
            }))
        }
        "start_task" => {
            let task_id = parse_task_id(&args)?;
            ensure_task_in_project(state, task_id, project_id).await?;
            // The same vetting the Start button does. Without it the assistant
            // can queue a Reviewed card onto an engine that cannot ask for
            // permission, and get a run that refuses every tool call — the
            // capability gate is meant to refuse at the click, and this was a
            // way round it.
            crate::routes::tasks::vet_task(state, task_id)
                .await
                .map_err(|(_, message)| message)?;
            vet_spend(state, task_id).await?;
            let pass = eren_core::manager::pass_for_chat(&state.db, chat_id).await;
            vet_manager_start(state, pass.as_ref(), Some(task_id)).await?;
            if let Some(pass) = &pass {
                let title: String = sqlx::query_scalar("SELECT title FROM tasks WHERE id=$1")
                    .bind(task_id)
                    .fetch_one(&state.db.pool)
                    .await
                    .map_err(|e| e.to_string())?;
                eren_core::manager::record_start(&state.db, pass, task_id, &title).await?;
            }
            let run_id = state
                .orchestrator
                .enqueue_task(task_id)
                .await
                .map_err(|e| e.to_string())?;
            sqlx::query("UPDATE tasks SET board_column='running' WHERE id=$1")
                .bind(task_id)
                .execute(&state.db.pool)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "run_id": run_id, "started": true }))
        }
        "list_tasks" => {
            let rows = sqlx::query(
                "SELECT t.id, t.title, t.board_column, r.status AS run_status
                 FROM tasks t
                 LEFT JOIN LATERAL (SELECT status FROM runs WHERE task_id=t.id
                                    ORDER BY created_at DESC LIMIT 1) r ON TRUE
                 WHERE t.project_id=$1 ORDER BY t.created_at DESC",
            )
            .bind(project_id)
            .fetch_all(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "tasks": rows.iter().map(|r| json!({
                    "task_id": r.get::<Uuid, _>("id"),
                    "title": r.get::<String, _>("title"),
                    "column": r.get::<String, _>("board_column"),
                    "run_status": r.get::<Option<String>, _>("run_status"),
                })).collect::<Vec<_>>()
            }))
        }
        "get_task_status" => {
            let task_id = parse_task_id(&args)?;
            ensure_task_in_project(state, task_id, project_id).await?;
            let row = sqlx::query(
                "SELECT t.title, t.board_column, r.status, r.cost_usd, r.error_reason,
                        ck.status AS checks_status, ck.results AS checks_results
                 FROM tasks t
                 LEFT JOIN LATERAL (SELECT * FROM runs WHERE task_id=t.id
                                    ORDER BY created_at DESC LIMIT 1) r ON TRUE
                 LEFT JOIN LATERAL (SELECT status, results FROM check_runs WHERE task_id=t.id
                                    ORDER BY created_at DESC LIMIT 1) ck ON TRUE
                 WHERE t.id=$1",
            )
            .bind(task_id)
            .fetch_one(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "title": row.get::<String, _>("title"),
                "column": row.get::<String, _>("board_column"),
                "run_status": row.get::<Option<String>, _>("status"),
                "cost_usd": row.get::<Option<f64>, _>("cost_usd"),
                "error": row.get::<Option<String>, _>("error_reason"),
                // Whether the work passes the project's own checks — what a
                // manager deciding "land it or send it back" needs most. Names
                // and verdicts only; the logs are for the person's drawer.
                "checks": row.get::<Option<String>, _>("checks_status").map(|status| {
                    let results: Vec<eren_core::checks::CheckResult> = row
                        .get::<Option<serde_json::Value>, _>("checks_results")
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();
                    json!({
                        "status": status,
                        "results": results.iter().map(|r| json!({
                            "name": r.name,
                            "passed": r.passed(),
                        })).collect::<Vec<_>>(),
                    })
                }),
            }))
        }
        "list_agents" => {
            let rows = sqlx::query(
                "SELECT a.name, a.description, a.model_tier, a.status, a.title, m.name AS manager
                   FROM agents a LEFT JOIN agents m ON m.id = a.reports_to AND m.status <> 'retired'
                  WHERE a.workspace_id=$1 AND a.status <> 'retired'
                 ORDER BY a.name ASC",
            )
            .bind(workspace_id)
            .fetch_all(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "agents": rows.iter().map(|r| json!({
                    "name": r.get::<String, _>("name"),
                    "description": r.get::<String, _>("description"),
                    "model_tier": r.get::<String, _>("model_tier"),
                    // A paused agent can be given a card, but it will not
                    // start until a person resumes it.
                    "paused": r.get::<String, _>("status") != "active",
                    "title": r.get::<Option<String>, _>("title"),
                    "reports_to": r.get::<Option<String>, _>("manager"),
                })).collect::<Vec<_>>()
            }))
        }
        "cancel_task" => {
            let task_id = parse_task_id(&args)?;
            ensure_task_in_project(state, task_id, project_id).await?;
            // The task's run, never a run id from the model: `task_id` is the
            // only handle `ensure_task_in_project` can check, so a run id
            // would be an unguardable back door into another project. It also
            // means the assistant cannot reach its own chat run, which carries
            // a chat_id and a null task_id.
            let run_id: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM runs WHERE task_id=$1 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(task_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            let Some(run_id) = run_id else {
                // A different fact from "already finished", and the assistant
                // should not report one as the other.
                return Err(
                    "this task has never been started, so there is nothing to cancel".into(),
                );
            };
            // The route handler itself, not a copy of it: its guards, its
            // wording, and anything added to it later apply here without a
            // second implementation drifting out of agreement.
            let axum::Json(v) = crate::routes::tasks::cancel_run(
                axum::extract::State(state.clone()),
                axum::extract::Path(run_id),
            )
            .await
            .map_err(|(_, message)| message)?;
            record_if_pass(state, chat_id, "cancel", task_id, "").await;
            Ok(v)
        }
        "get_diff" => {
            let task_id = parse_task_id(&args)?;
            ensure_task_in_project(state, task_id, project_id).await?;
            let row = sqlx::query(
                "SELECT p.vcs, COALESCE(t.worktree_path, (
                            SELECT r.worktree_path FROM runs r
                             WHERE r.task_id = t.id AND r.worktree_path IS NOT NULL
                             ORDER BY r.created_at DESC LIMIT 1)) AS worktree,
                        COALESCE(p.default_branch, 'main') AS base
                   FROM tasks t JOIN projects p ON p.id = t.project_id
                  WHERE t.id = $1",
            )
            .bind(task_id)
            .fetch_one(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;

            // Refused with a reason rather than answered with an empty diff,
            // which to a model reads as "nothing changed".
            if row.get::<String, _>("vcs") != "git" {
                return Err("this project has no version control, so its tasks edit the folder directly — there is no diff".into());
            }
            let Some(worktree) = row.get::<Option<String>, _>("worktree") else {
                return Err("this task has not run yet, so there is nothing to diff".into());
            };
            let worktree = std::path::PathBuf::from(worktree);
            let base: String = row.get("base");

            let stats = state
                .orchestrator
                .worktrees
                .diff_stat(&worktree, &base)
                .await
                .map_err(|e| e.to_string())?;

            match args.get("path").and_then(Value::as_str) {
                Some(path) => {
                    if !stats.iter().any(|f| f.path == path) {
                        let names: Vec<_> = stats.iter().take(20).map(|f| f.path.clone()).collect();
                        return Err(format!(
                            "no file called {path} changed in this task. These did: {}",
                            names.join(", ")
                        ));
                    }
                    let full = state
                        .orchestrator
                        .worktrees
                        .diff_file(&worktree, &base, path)
                        .await
                        .map_err(|e| e.to_string())?;
                    let (text, dropped) = clip(&full, MAX_DIFF_CHARS);
                    Ok(json!({
                        "path": path,
                        "diff": text,
                        "truncated": dropped > 0,
                    }))
                }
                None => Ok(summarize(stats)),
            }
        }
        "get_spend" => {
            let days = args
                .get("days")
                .and_then(Value::as_i64)
                .unwrap_or(7)
                .clamp(1, 365) as i32;
            let totals = eren_core::spend::for_project(&state.db, project_id, days)
                .await
                .map_err(|e| e.to_string())?;
            let gate = state.orchestrator.queue_gate().await;
            Ok(json!({
                "days": days,
                "project": totals,
                // Named apart from the project totals on purpose: the cap and
                // the queue are the whole machine's, not this project's.
                "queue": match &gate {
                    eren_core::runs::orchestrator::QueueGate::Open => json!({ "state": "open" }),
                    eren_core::runs::orchestrator::QueueGate::Paused => json!({ "state": "paused" }),
                    eren_core::runs::orchestrator::QueueGate::OverBudget(over) =>
                        json!({ "state": "over_budget", "budget": over.policy, "why": over.to_string() }),
                },
            }))
        }
        "list_skills" => {
            let skills = eren_core::skills::list(&state.db, workspace_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({
                // Names and descriptions only. The bodies are capped at 4000
                // characters each and are fenced and neutralised at run time
                // precisely because they are untrusted text — handing them to
                // the assistant outside that fence would undo the work, and it
                // has no use for them: `create_task` binds by id and the text
                // is folded in when the run starts.
                "skills": skills.iter().filter(|s| s.enabled).map(|s| json!({
                    "name": s.name,
                    "description": s.description,
                })).collect::<Vec<_>>()
            }))
        }
        "move_task" => {
            let task_id = parse_task_id(&args)?;
            ensure_task_in_project(state, task_id, project_id).await?;
            let column = args
                .get("column")
                .and_then(Value::as_str)
                .ok_or("column must be backlog, review or done")?;
            // "running" is deliberately not offered: it would relabel a card
            // that has no run, which is worse than the stranded state it looks
            // like it fixes. Starting work is `start_task`.
            if !["backlog", "review", "done"].contains(&column) {
                return Err(format!(
                    "{column} is not a column you can file a card in — use backlog, review or done, and start_task to start one"
                ));
            }
            let axum::Json(_) = crate::routes::tasks::move_task(
                axum::extract::State(state.clone()),
                axum::extract::Path(task_id),
                axum::Json(crate::routes::tasks::MoveTask::to_column(column)),
            )
            .await
            .map_err(|(_, message)| message)?;
            record_if_pass(state, chat_id, "move", task_id, column).await;
            // Says "filed", never "landed": a user reading "done" in chat must
            // not come away thinking their code merged.
            Ok(json!({ "filed": true, "column": column }))
        }
        "search_code" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if query.is_empty() {
                return Err("say what to look for".into());
            }
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(8)
                .clamp(1, 20) as usize;
            // Deliberately not gated on `embed::status()`. That status reports
            // what this process has *asked* the embedder for, and it only turns
            // Ready once something does — so a pre-check would refuse every
            // search after a restart, on an index that is complete. Asking is
            // what makes it ready.
            match eren_core::rag::retrieve::top_k(&state.db, project_id, query, limit).await {
                Ok(hits) => Ok(json!({
                    "hits": hits.iter().map(|h| json!({
                        "path": h.rel_path,
                        "line": h.start_line,
                        "symbol": h.symbol,
                        "score": h.score,
                        "excerpt": h.content.chars().take(600).collect::<String>(),
                    })).collect::<Vec<_>>()
                })),
                // Not an error: the assistant has Grep and Glob, and a tool
                // failure it cannot act on is worse than being told to use them.
                Err(e) => Ok(json!({
                    "hits": [],
                    "note": format!("meaning search is unavailable ({e}) — use Grep and Glob"),
                })),
            }
        }
        "search_documents" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or("query is required")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n.clamp(1, 10) as usize)
                .unwrap_or(eren_core::rag::retrieve::DEFAULT_K);
            let passages = eren_core::rag::retrieve::top_k(&state.db, project_id, query, limit)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "passages": passages.iter().map(|p| json!({
                    "path": p.rel_path,
                    "part": p.chunk_index + 1,
                    "score": p.score,
                    "text": p.content,
                })).collect::<Vec<_>>(),
                "note": if passages.is_empty() {
                    "nothing matched — the index may be empty, or try other words"
                } else { "" },
            }))
        }
        "list_documents" => {
            let rows = sqlx::query(
                "SELECT rel_path, status, bytes FROM project_documents
                 WHERE project_id=$1 ORDER BY rel_path",
            )
            .bind(project_id)
            .fetch_all(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "documents": rows.iter().map(|r| json!({
                    "path": r.get::<String, _>("rel_path"),
                    "status": r.get::<String, _>("status"),
                    "bytes": r.get::<i64, _>("bytes"),
                })).collect::<Vec<_>>(),
            }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Which skill a new card uses: the one named, else the one the user
/// @mentioned. Several mentioned and none named is a question for the
/// assistant, not a guess — a card takes one skill, and picking the first
/// would quietly drop the others the person asked for.
async fn resolve_skill(
    state: &AppState,
    workspace_id: Uuid,
    named: Option<&Value>,
    mentioned: &[(Uuid, String)],
) -> Result<Option<(Uuid, String)>, String> {
    if let Some(name) = named
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        // Matched without regard to case, like agents; a disabled skill is
        // as good as absent.
        let found: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT id, name FROM skills
              WHERE skill_in_workspace(workspace_id, owner_id, $1)
                AND lower(name)=lower($2) AND enabled",
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|e| e.to_string())?;
        return found.map(Some).ok_or_else(|| {
            format!(
                "there is no skill called \"{name}\" here — list_skills shows the ones there are"
            )
        });
    }
    match mentioned {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        several => Err(format!(
            "the user named several skills ({}) and a card takes one — pass skill_name to say which this card uses",
            several.iter().map(|(_, n)| format!("\"{n}\"")).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Files listed in one summary, and characters of one file's diff.
///
/// A five-thousand-line change must not be able to reach the model in one
/// call. The failure to avoid is not a truncated answer — it is an assistant
/// that calls `get_diff` twelve times to rebuild what it was capped out of,
/// which is why the tool description says not to.
const MAX_DIFF_FILES: usize = 200;
const MAX_DIFF_CHARS: usize = 12_000;

/// One line per file, biggest first, saying plainly what did not fit.
fn summarize(mut stats: Vec<eren_core::worktrees::manager::FileStat>) -> Value {
    let files_changed = stats.len();
    let added: i64 = stats.iter().map(|f| f.added).sum();
    let removed: i64 = stats.iter().map(|f| f.removed).sum();
    stats.sort_by_key(|f| -(f.added + f.removed));
    let more = files_changed.saturating_sub(MAX_DIFF_FILES);
    stats.truncate(MAX_DIFF_FILES);
    json!({
        "files_changed": files_changed,
        "added": added,
        "removed": removed,
        "files": stats,
        // Named rather than silently dropped, on the same reasoning the
        // knowledge base uses: a list that stops without saying so reads as a
        // complete list.
        "more_files": more,
    })
}

/// Cut to a budget on a line boundary, saying how much was left out.
///
/// The same shape and marker as `brain`, `skills` and the knowledge base use.
fn clip(text: &str, budget: usize) -> (String, usize) {
    if text.chars().count() <= budget {
        return (text.to_string(), 0);
    }
    let kept: String = text.chars().take(budget).collect();
    let cut = kept
        .rfind('\n')
        .map(|i| &kept[..i])
        .unwrap_or(&kept)
        .to_string();
    let dropped = text.chars().count() - cut.chars().count();
    (
        format!("{cut}\n[truncated — {dropped} more characters]"),
        dropped,
    )
}

/// Which agent a new task binds to: what the assistant asked for, or failing
/// that, who the user named with `@` in the message being answered.
///
/// Two behaviours worth stating, because both used to be absent:
///
/// * **An unknown `agent_name` is an error**, not a shrug. It used to resolve
///   to `NULL`, so a single typo produced an unassigned task while the
///   assistant cheerfully reported it had assigned one. The error lists the
///   real names, which is something the model can act on.
/// * **A single `@mention` binds even when the model forgets to pass it
///   through.** That is the whole point of resolving the mention at send time:
///   the user's instruction does not depend on the model relaying it. Two or
///   more mentions are left to the model, because "which of these two tasks is
///   whose" is a question only the request can answer — and the prompt block
///   `mentions::augment_prompt` adds tells it to answer it.
async fn resolve_agent(
    state: &AppState,
    chat_id: Uuid,
    workspace_id: Uuid,
    asked: Option<&Value>,
) -> Result<(Option<Uuid>, Option<String>), String> {
    if let Some(name) = asked
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        // Matched without regard to case, because everything upstream is:
        // `@frontend` finds the agent called Frontend, and a model echoing the
        // user's own typing back must not turn a mention that already resolved
        // into a hard error. `agents_ws_name` is unique per workspace, and two
        // names differing only in case would be a library nobody could use.
        let row = sqlx::query(
            "SELECT id, name FROM agents
              WHERE workspace_id=$1 AND lower(name)=lower($2) AND status <> 'retired'",
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|e| e.to_string())?;
        return match row {
            Some(r) => Ok((Some(r.get("id")), Some(r.get("name")))),
            None => {
                let known = sqlx::query(
                    "SELECT name FROM agents WHERE workspace_id=$1 AND status <> 'retired'
                          ORDER BY name",
                )
                .bind(workspace_id)
                .fetch_all(&state.db.pool)
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .map(|r| format!("\"{}\"", r.get::<String, _>("name")))
                .collect::<Vec<_>>()
                .join(", ");
                Err(if known.is_empty() {
                    format!("no agent named \"{name}\" — this workspace has no agents yet")
                } else {
                    format!("no agent named \"{name}\". The agents here are: {known}")
                })
            }
        };
    }

    let mentioned = eren_core::runs::mentions::latest_for_chat(&state.db, chat_id)
        .await
        .map_err(|e| e.to_string())?;
    match mentioned.len() {
        1 => Ok((Some(mentioned[0].0), Some(mentioned[0].1.clone()))),
        _ => Ok((None, None)),
    }
}

/// Undo HTML escaping a model applied to a plain-text field.
///
/// Not hypothetical: `Snakes &amp; Ladders board game app` is in this
/// database, from an assistant that escaped its own title on the way into
/// `create_task`. Nothing downstream renders a card title as HTML — the board,
/// the drawer and the commit message all treat it as text — so the escape has
/// no reader and shows up verbatim in every one of them, including the git
/// history a merge writes.
///
/// The five XML entities only, and applied once. A title that genuinely
/// contains `&amp;` is a title about HTML, which is rarer than a title about
/// something and someone else.
fn unescape_html(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // Last, or `&amp;lt;` would decode two steps into `<`.
        .replace("&amp;", "&")
}

fn parse_task_id(args: &Value) -> Result<Uuid, String> {
    args.get("task_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| "task_id must be a UUID".to_string())
}

/// Note down an action if this turn is a management pass, and do nothing at
/// all if it is a person typing.
///
/// The uncapped half of the log. Only `start` spends the budget, but a pass
/// that filed three finished cards and cancelled one that had been stuck for a
/// day did real work, and a morning summary that showed only what it started
/// would read as though it had done nothing.
async fn record_if_pass(state: &AppState, chat_id: Uuid, kind: &str, task_id: Uuid, detail: &str) {
    if let Some(pass) = eren_core::manager::pass_for_chat(&state.db, chat_id).await {
        eren_core::manager::record_action(&state.db, &pass, kind, Some(task_id), detail).await;
    }
}

/// The rails an unattended management pass runs inside.
///
/// Returns the pass when this turn is one, having already refused the call if
/// it may not go ahead. `None` means an ordinary chat — a person is typing,
/// and none of this applies to them.
///
/// The cap is counted from `manager_actions`, never from anything the model
/// says about its own history, and the caller records the start *before* it
/// enqueues. A start that the recorder missed would be a start the cap never
/// saw, and the next pass would spend the same budget again.
///
/// The refusals are written at the model: it is mid-tool-call and needs to
/// know what to do instead, which in both cases is "leave it in the backlog
/// and say so", not "try a different tool".
/// The budget questions, asked before a start is recorded so a refusal
/// neither burns a unit of a manager pass's cap nor tells its log the card
/// started. A spent budget refuses; a start that could overrun one is a
/// person's call, never the assistant's — it stays in the backlog.
async fn vet_spend(state: &AppState, task_id: Uuid) -> Result<(), String> {
    let scope = eren_core::budgets::scope_of_task(&state.db, task_id)
        .await
        .map_err(|e| e.to_string())?;
    eren_core::budgets::check(&state.db, &scope, true)
        .await
        .map_err(|e| e.to_string())?;
    match eren_core::budgets::forecast_check(&state.db, task_id, false)
        .await
        .map_err(|e| e.to_string())?
    {
        Ok(()) => Ok(()),
        Err(ask) => Err(format!(
            "{ask} — the card is in the backlog; a person has to choose to start it"
        )),
    }
}

async fn vet_manager_start(
    state: &AppState,
    pass: Option<&eren_core::manager::Pass>,
    task_id: Option<Uuid>,
) -> Result<(), String> {
    let Some(pass) = pass else {
        return Ok(());
    };
    let used = eren_core::manager::starts_used(&state.db, pass).await;
    if used >= pass.max_starts {
        return Err(if pass.max_starts == 0 {
            "This management pass may not start cards — it is configured to review and \
             report only. Create the card in the backlog instead and say in your summary \
             that it is waiting for someone to start it."
                .to_string()
        } else {
            format!(
                "This management pass has already started its {} allowed card{}. Leave this \
                 one in the backlog and name it in your summary as the thing you would do \
                 next — do not try again with another tool.",
                pass.max_starts,
                if pass.max_starts == 1 { "" } else { "s" },
            )
        });
    }
    // A card that came from outside Eren was written by somebody who is not
    // the owner of this machine. `tasks::create_imported` refuses to start one
    // for exactly this reason — "the one place a human has to stand is between
    // them and an agent holding Write and Bash" — and an agent running on a
    // timer is precisely what would remove that human.
    if let Some(task_id) = task_id {
        let source: Option<String> = sqlx::query_scalar("SELECT source FROM tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|e| e.to_string())?
            .flatten();
        if let Some(source) = source {
            return Err(format!(
                "This card was imported from outside Eren ({source}), and a scheduled pass \
                 cannot start one — a person has to read it first. Say in your summary that \
                 it looks ready to start, and leave it to them."
            ));
        }
    }
    Ok(())
}

async fn ensure_task_in_project(
    state: &AppState,
    task_id: Uuid,
    project_id: Uuid,
) -> Result<(), String> {
    let row = sqlx::query("SELECT project_id FROM tasks WHERE id=$1")
        .bind(task_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|e| e.to_string())?;
    match row {
        Some(r) if r.get::<Uuid, _>("project_id") == project_id => Ok(()),
        Some(_) => Err("task belongs to a different project".into()),
        None => Err("no such task".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name `tools_list` advertises, in order.
    fn advertised(kind: &str, planning: bool) -> Vec<String> {
        tools_list(kind, planning)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Plan mode advertises exactly the board list minus the four that change
    /// something — and, crucially, the *same* four the run denies.
    ///
    /// Advertising one the run has denied is the silent failure: chat carries
    /// `permission_prompt_tool: false`, so the refused call produces no prompt
    /// to answer and the assistant reports the workspace as broken.
    #[test]
    fn planning_advertises_exactly_what_planning_allows() {
        let acting = eren_core::runs::chat_plan::ACTING_TOOL_NAMES;
        let full = advertised("repo", false);
        let planning = advertised("repo", true);

        assert_eq!(
            planning,
            full.iter()
                .filter(|n| !acting.contains(&n.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        );
        for name in acting {
            assert!(
                !planning.contains(&name.to_string()),
                "{name} survived plan mode"
            );
            assert!(
                full.contains(&name.to_string()),
                "{name} is not in the board list at all"
            );
        }
        // Reading the board is what makes a plan worth anything.
        for name in [
            "list_tasks",
            "get_task_status",
            "list_agents",
            "get_diff",
            "search_code",
        ] {
            assert!(
                planning.contains(&name.to_string()),
                "{name} should survive plan mode"
            );
        }

        // The invariant that actually matters, and it spans two crates: what
        // this endpoint advertises in plan mode has to be exactly what the
        // orchestrator's plan-mode RunSpec allows. Equality, not subset — a
        // tool allowed but never offered is a grant nothing can use, and a
        // tool offered but not allowed is the silent refusal above.
        let allowed: std::collections::HashSet<String> =
            eren_core::runs::chat_plan::without_acting(
                &eren_core::runs::orchestrator::CHAT_ALLOWED_TOOLS
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .filter(|t| t.starts_with("mcp__eren__"))
            .collect();
        let offered: std::collections::HashSet<String> =
            planning.iter().map(|n| format!("mcp__eren__{n}")).collect();
        assert_eq!(offered, allowed);
    }

    /// A space has nothing to take away, so plan mode must not change it.
    #[test]
    fn planning_does_not_touch_a_space() {
        assert_eq!(advertised("space", true), advertised("space", false));
    }

    /// The same equality the board arm gets, for the space arm.
    ///
    /// It was missing, and the gap is not theoretical: a space chat that
    /// advertises a tool absent from `SPACE_CHAT_ALLOWED_TOOLS` is refused at
    /// runtime with no prompt to answer, because chat carries
    /// `permission_prompt_tool: false`. The board side has been pinned since
    /// the tools existed; the space side had nothing.
    #[test]
    fn a_space_is_offered_exactly_what_a_space_is_allowed() {
        let offered: std::collections::HashSet<String> = advertised("space", false)
            .iter()
            .map(|n| format!("mcp__eren__{n}"))
            .collect();
        let allowed: std::collections::HashSet<String> =
            eren_core::runs::orchestrator::SPACE_CHAT_ALLOWED_TOOLS
                .iter()
                .filter(|t| t.starts_with("mcp__eren__"))
                .map(|t| t.to_string())
                .collect();
        assert_eq!(offered, allowed);
    }

    #[test]
    fn tools_list_exposes_every_tool_the_assistant_has() {
        let v = tools_list("repo", false);
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                // What it can start.
                "create_task",
                "start_task",
                // What it can see.
                "list_tasks",
                "get_task_status",
                "list_agents",
                // And what it can do about the result, which is the half that
                // was missing: it could spend $143 and then had no tool to
                // stop it, read it, or say what it cost.
                "cancel_task",
                "get_diff",
                "get_spend",
                "list_skills",
                "move_task",
                // Finding code by meaning, for the question Grep cannot answer
                // because the asker does not know the word yet.
                "search_code",
                // And the one that answers nothing: asking the person, when
                // the request is ambiguous in a way that changes the work.
                "ask_user",
            ]
        );
    }

    /// A space's toolbox is disjoint from the board's, both ways — the
    /// call_tool guard enforces it at runtime, and this pins what each list
    /// advertises so the two cannot silently bleed together.
    #[test]
    fn a_space_advertises_document_tools_and_nothing_else() {
        let v = tools_list("space", false);
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // `ask_user` is the one thing both toolboxes share, and deliberately:
        // a space's brief can be as ambiguous as a repository's.
        assert_eq!(names, ["search_documents", "list_documents", "ask_user"]);
    }

    /// The schema is the only place the model learns these two rules, and both
    /// are behaviour `resolve_agent` will actually enforce — a description that
    /// drifts from it is how a model ends up retrying a call that can never
    /// succeed, or omitting an argument thinking something else will fill it in.
    #[test]
    fn a_title_the_model_escaped_is_stored_as_the_words_it_meant() {
        // `Snakes &amp; Ladders board game app` is really in the database.
        assert_eq!(
            super::unescape_html("Snakes &amp; Ladders board game app"),
            "Snakes & Ladders board game app"
        );
        assert_eq!(
            super::unescape_html("Fix &lt;head&gt; ordering"),
            "Fix <head> ordering"
        );
        // Applied once: `&amp;` is decoded last, so this stays as the text it
        // was rather than decoding twice into `<`.
        assert_eq!(super::unescape_html("&amp;lt;"), "&lt;");
        // Untouched when there is nothing to undo.
        assert_eq!(super::unescape_html("Add a README"), "Add a README");
    }

    #[test]
    fn create_task_tells_the_model_what_agent_name_does() {
        let v = tools_list("repo", false);
        let described = v["tools"][0]["inputSchema"]["properties"]["agent_name"]["description"]
            .as_str()
            .unwrap();
        assert!(described.contains("unknown name is rejected"));
        assert!(described.contains("@mention"));
    }

    /// The failure this change can introduce, and it is silent.
    ///
    /// A tool advertised by `tools/list` but missing from `CHAT_ALLOWED_TOOLS`
    /// is one the model reaches for and is refused — and chat runs carry
    /// `permission_prompt_tool: false`, so there is no prompt to answer. The
    /// call simply fails and the assistant reports that the workspace is
    /// broken. The two lists live in different crates, which is exactly why
    /// nothing else would catch it.
    #[test]
    fn every_tool_the_assistant_is_offered_is_one_it_is_pre_approved_to_call() {
        let offered: std::collections::HashSet<String> = tools_list("repo", false)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| format!("mcp__eren__{}", t["name"].as_str().unwrap()))
            .collect();
        let allowed: std::collections::HashSet<String> =
            eren_core::runs::orchestrator::CHAT_ALLOWED_TOOLS
                .iter()
                .filter(|t| t.starts_with("mcp__eren__"))
                .map(|t| t.to_string())
                .collect();
        // Both directions: a grant for a tool that does not exist is usually a
        // rename done on one side only.
        assert_eq!(offered, allowed);
    }

    /// Keeps the omission a decision the codebase holds, rather than a gap the
    /// next contributor fills in.
    #[test]
    fn the_chat_assistant_is_offered_no_way_to_merge() {
        for t in tools_list("repo", false)["tools"].as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            assert!(!name.contains("merge"), "{name} lands code from chat");
            assert!(
                !name.contains("retry"),
                "{name} can discard an unmerged diff"
            );
        }
    }

    #[test]
    fn the_descriptions_say_the_parts_that_are_easy_to_get_wrong() {
        let tools = tools_list("repo", false);
        let by_name = |n: &str| -> String {
            tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == n)
                .unwrap()["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // A model that does not know the diff is capped will try to rebuild it.
        assert!(by_name("get_diff").contains("capped"));
        // And one that thinks "done" merges will tell the user their code landed.
        assert!(by_name("move_task").contains("does not merge"));
        // Cancelling must not read as undoing.
        assert!(by_name("cancel_task").contains("does not undo"));
    }

    #[test]
    fn a_summary_names_the_files_it_left_out() {
        let stats: Vec<_> = (0..MAX_DIFF_FILES + 5)
            .map(|i| eren_core::worktrees::manager::FileStat {
                path: format!("f{i}.rs"),
                added: i as i64,
                removed: 0,
                binary: false,
            })
            .collect();
        let v = summarize(stats);
        assert_eq!(v["files_changed"], MAX_DIFF_FILES + 5);
        assert_eq!(v["files"].as_array().unwrap().len(), MAX_DIFF_FILES);
        assert_eq!(v["more_files"], 5);
        // Biggest first, so what is dropped is the least interesting.
        assert_eq!(v["files"][0]["path"], format!("f{}.rs", MAX_DIFF_FILES + 4));
    }

    #[test]
    fn a_short_diff_is_not_marked_truncated() {
        let (text, dropped) = clip("one\ntwo\n", 100);
        assert_eq!(text, "one\ntwo\n");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn a_long_diff_says_how_much_it_left_out() {
        let long = "a line of diff\n".repeat(2000);
        let (text, dropped) = clip(&long, 200);
        assert!(dropped > 0);
        assert!(text.contains("[truncated —"), "{text}");
        // Cut on a line boundary, so the last line shown is a whole one.
        let body = text.lines().next().unwrap();
        assert_eq!(body, "a line of diff");
    }
}
