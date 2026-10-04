//! Upload, serve, and claim prompt attachments.
//!
//! Uploads are two-phase: the browser POSTs files here and gets ids back, then
//! passes those ids to create-task or send-message, which *claims* them. That
//! keeps the create endpoints on small JSON bodies and means a failed create
//! leaks at most an unclaimed row, which the sweeper reaps.
//!
//! Bytes land in `~/.eren/attachments/<id>/<filename>` — outside every git
//! tree. See the 0009 migration for why that matters.

use super::{internal, ApiError};
use crate::auth::Caller;
use crate::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use eren_core::runs::attachments as core;
use eren_core::scope::Owned;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Whole-request ceiling: enough for MAX_ATTACHMENTS × MAX_ATTACHMENT_BYTES
/// plus multipart framing. Layered on the upload route alone — putting it on
/// the router would hand every other endpoint the same limit.
const MAX_UPLOAD_BYTES: usize = 26 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 10;

/// `(extension, mime, kind)`. The extension is authoritative: a multipart
/// part's Content-Type is chosen by the client and is not evidence.
///
/// `svg` is deliberately absent — it executes script when served back, and a
/// model gains nothing from it over a png.
const ALLOWED: &[(&str, &str, &str)] = &[
    ("png", "image/png", "image"),
    ("jpg", "image/jpeg", "image"),
    ("jpeg", "image/jpeg", "image"),
    ("gif", "image/gif", "image"),
    ("webp", "image/webp", "image"),
    ("pdf", "application/pdf", "pdf"),
    ("txt", "text/plain", "text"),
    ("md", "text/markdown", "text"),
    ("csv", "text/csv", "text"),
    ("json", "application/json", "text"),
    ("log", "text/plain", "text"),
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/{id}/attachments",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        // A general chat has no project; its uploads belong to the workspace.
        .route(
            "/workspaces/{id}/attachments",
            post(upload_general).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/attachments/{id}", get(serve).delete(remove))
        .route("/tasks/{id}/attachments", get(list_for_task))
}

/// Reduce a client-supplied filename to a safe basename, or `None` if nothing
/// safe survives.
pub(crate) fn sanitize_filename(raw: &str) -> Option<String> {
    if raw.contains('\0') {
        return None;
    }
    // Strip any directory component. Both separators, because a Windows client
    // sends backslashes and `Path` would not treat them as separators here.
    let base = raw.rsplit(['/', '\\']).next()?.trim();
    if base.is_empty() || base == "." || base == ".." || base.starts_with('.') {
        return None;
    }

    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Keep the extension when truncating — it is what `classify` reads.
    const MAX: usize = 128;
    if cleaned.len() <= MAX {
        return Some(cleaned);
    }
    match cleaned.rsplit_once('.') {
        Some((stem, ext)) if ext.len() < 16 => {
            let keep = MAX.saturating_sub(ext.len() + 1);
            Some(format!("{}.{}", &stem[..keep.min(stem.len())], ext))
        }
        _ => Some(cleaned[..MAX].to_string()),
    }
}

/// Extension-driven `(mime, kind)`, or `None` when the type is not allowed.
fn classify(filename: &str) -> Option<(&'static str, &'static str)> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    ALLOWED
        .iter()
        .find(|(e, _, _)| *e == ext)
        .map(|(_, mime, kind)| (*mime, *kind))
}

/// Does the payload look like what its extension claims? Cheap magic-byte
/// check so a renamed binary cannot masquerade as an image.
fn content_matches(mime: &str, kind: &str, bytes: &[u8]) -> bool {
    match kind {
        // Same heuristic `files.rs` uses to call a blob binary.
        "text" => !bytes.contains(&0),
        "pdf" => bytes.starts_with(b"%PDF-"),
        "image" => match mime {
            "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G']),
            "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            "image/gif" => bytes.starts_with(b"GIF8"),
            "image/webp" => {
                bytes.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP"
            }
            _ => false,
        },
        _ => false,
    }
}

/// Where an upload belongs, and so where it may be claimed: a project (a
/// card, a project chat) or a workspace (a general chat).
#[derive(Clone, Copy)]
pub(crate) enum Home {
    Project(Uuid),
    Workspace(Uuid),
}

impl Home {
    fn project(self) -> Option<Uuid> {
        match self {
            Home::Project(id) => Some(id),
            Home::Workspace(_) => None,
        }
    }
    fn workspace(self) -> Option<Uuid> {
        match self {
            Home::Workspace(id) => Some(id),
            Home::Project(_) => None,
        }
    }
}

async fn upload(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(project_id): UrlPath<Uuid>,
    multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Project(project_id)).await?;
    // Check the project up front so a bogus id is a 404 rather than an FK 500.
    let exists = sqlx::query("SELECT 1 AS ok FROM projects WHERE id=$1")
        .bind(project_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?;
    if exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such project".into()));
    }
    store(&state, Home::Project(project_id), multipart).await
}

async fn upload_general(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(workspace_id): UrlPath<Uuid>,
    multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    caller
        .require(&state, Owned::Workspace(workspace_id))
        .await?;
    let exists = sqlx::query("SELECT 1 AS ok FROM workspaces WHERE id=$1")
        .bind(workspace_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?;
    if exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such workspace".into()));
    }
    store(&state, Home::Workspace(workspace_id), multipart).await
}

/// Validate and write every file in the upload, each to its own directory.
async fn store(
    state: &AppState,
    home: Home,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let root = core::default_root();
    let mut saved: Vec<Value> = vec![];

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("malformed upload: {e}")))?
    {
        if saved.len() >= MAX_ATTACHMENTS {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("at most {MAX_ATTACHMENTS} files per upload"),
            ));
        }
        let Some(raw_name) = field.file_name().map(|s| s.to_string()) else {
            continue; // not a file part
        };
        let Some(filename) = sanitize_filename(&raw_name) else {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unusable filename: {raw_name}"),
            ));
        };
        let Some((mime, kind)) = classify(&filename) else {
            let exts: Vec<&str> = ALLOWED.iter().map(|(e, _, _)| *e).collect();
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!(
                    "{filename}: only these types are accepted: {}",
                    exts.join(", ")
                ),
            ));
        };

        let bytes = field.bytes().await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("{filename}: upload failed: {e}"),
            )
        })?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "{filename} is larger than {} MB",
                    MAX_ATTACHMENT_BYTES / 1024 / 1024
                ),
            ));
        }
        if !content_matches(mime, kind, &bytes) {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("{filename}: contents do not match its extension"),
            ));
        }

        // Insert first so the row's id names the directory.
        let id: Uuid = sqlx::query(
            "INSERT INTO attachments (project_id, workspace_id, filename, mime, kind, size_bytes, disk_path)
             VALUES ($1,$2,$3,$4,$5,$6,'') RETURNING id",
        )
        .bind(home.project())
        .bind(home.workspace())
        .bind(&filename)
        .bind(mime)
        .bind(kind)
        .bind(bytes.len() as i64)
        .fetch_one(&state.db.pool)
        .await
        .map_err(internal)?
        .get("id");

        let dir = root.join(id.to_string());
        let disk_path = dir.join(&filename);
        let write = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(&disk_path, &bytes).await
        }
        .await;
        if let Err(e) = write {
            // Don't leave a row pointing at bytes that were never written.
            let _ = sqlx::query("DELETE FROM attachments WHERE id=$1")
                .bind(id)
                .execute(&state.db.pool)
                .await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(internal(e));
        }
        sqlx::query("UPDATE attachments SET disk_path=$2 WHERE id=$1")
            .bind(id)
            .bind(disk_path.to_string_lossy().as_ref())
            .execute(&state.db.pool)
            .await
            .map_err(internal)?;

        saved.push(json!({
            "id": id, "filename": filename, "mime": mime,
            "kind": kind, "size": bytes.len(),
        }));
    }

    if saved.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no files in the upload".into()));
    }
    Ok(Json(json!({ "attachments": saved })))
}

async fn serve(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
) -> Result<Response, ApiError> {
    caller.require(&state, Owned::Attachment(id)).await?;
    let row = sqlx::query("SELECT filename, mime, kind, disk_path FROM attachments WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such attachment".to_string()))?;

    let filename: String = row.get("filename");
    let mime: String = row.get("mime");
    let kind: String = row.get("kind");
    let path: String = row.get("disk_path");

    let Ok(bytes) = tokio::fs::read(&path).await else {
        // Row outlived its bytes; the sweeper will reap it.
        return Err((StatusCode::GONE, "attachment file is missing".into()));
    };

    // Text kinds are served as plain text whatever their nominal type: an
    // uploaded .json or .csv must never be interpreted as a document.
    let content_type = if kind == "text" {
        "text/plain; charset=utf-8"
    } else {
        &mime
    };
    // Only render inline what the browser previews safely.
    let disposition = if kind == "image" || kind == "pdf" {
        "inline".to_string()
    } else {
        // filename is already [A-Za-z0-9._-], so no header injection and no
        // RFC 5987 encoding needed.
        format!("attachment; filename=\"{filename}\"")
    };

    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        // Bytes never change for a given id.
        .header(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        )
        .body(Body::from(bytes))
        .map_err(internal)
}

async fn remove(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(id): UrlPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Attachment(id)).await?;
    // Only unclaimed attachments can be removed — this is the ✕ on a chip
    // before submitting. Once claimed, the owning task/message controls it.
    let row = sqlx::query(
        "DELETE FROM attachments
         WHERE id=$1 AND task_id IS NULL AND message_id IS NULL
         RETURNING disk_path",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(internal)?
    .ok_or((
        StatusCode::CONFLICT,
        "attachment is already in use or does not exist".to_string(),
    ))?;

    if let Some(dir) = std::path::PathBuf::from(row.get::<String, _>("disk_path")).parent() {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    Ok(Json(json!({ "deleted": true })))
}

async fn list_for_task(
    State(state): State<AppState>,
    caller: Caller,
    UrlPath(task_id): UrlPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    caller.require(&state, Owned::Task(task_id)).await?;
    let rows = sqlx::query(
        "SELECT id, filename, mime, kind, size_bytes FROM attachments
         WHERE task_id=$1 ORDER BY created_at",
    )
    .bind(task_id)
    .fetch_all(&state.db.pool)
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "attachments": rows.iter().map(|r| json!({
            "id": r.get::<Uuid, _>("id"),
            "filename": r.get::<String, _>("filename"),
            "mime": r.get::<String, _>("mime"),
            "kind": r.get::<String, _>("kind"),
            "size": r.get::<i64, _>("size_bytes"),
        })).collect::<Vec<_>>()
    })))
}

/// Which row an upload is being bound to.
pub(crate) enum Owner {
    Task(Uuid),
    Message(Uuid),
}

/// Bind previously-uploaded attachments to their owner.
///
/// The `WHERE` clause is simultaneously the ownership check, the
/// cross-project check, and the double-claim check: an id that belongs to
/// another project or workspace, or has already been used, simply doesn't
/// match, and the affected-row count catches it.
///
/// All or nothing: a refusal claims none of them, where it used to leave the
/// ids that did match bound to an owner the caller was told had failed.
pub(crate) async fn claim(
    db: &eren_core::db::Db,
    ids: &[Uuid],
    home: Home,
    owner: Owner,
) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool.begin().await.map_err(internal)?;
    claim_in(&mut tx, ids, home, owner).await?;
    tx.commit().await.map_err(internal)
}

/// [`claim`] inside the caller's transaction, so the owner's own row and its
/// attachments are written together or not at all. On an `Err` the caller
/// must not commit.
pub(crate) async fn claim_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ids: &[Uuid],
    home: Home,
    owner: Owner,
) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Ok(());
    }
    // Two literal statements picked by variant — no interpolation into SQL.
    let (sql, owner_id) = match owner {
        Owner::Task(id) => (
            "UPDATE attachments SET task_id = $1
             WHERE id = ANY($2) AND project_id IS NOT DISTINCT FROM $3
               AND workspace_id IS NOT DISTINCT FROM $4
               AND task_id IS NULL AND message_id IS NULL",
            id,
        ),
        Owner::Message(id) => (
            "UPDATE attachments SET message_id = $1
             WHERE id = ANY($2) AND project_id IS NOT DISTINCT FROM $3
               AND workspace_id IS NOT DISTINCT FROM $4
               AND task_id IS NULL AND message_id IS NULL",
            id,
        ),
    };
    let affected = sqlx::query(sql)
        .bind(owner_id)
        .bind(ids)
        .bind(home.project())
        .bind(home.workspace())
        .execute(&mut **tx)
        .await
        .map_err(internal)?
        .rows_affected();

    if affected as usize != ids.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            "unknown, already-used, or cross-project attachment".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{classify, content_matches, sanitize_filename};

    #[test]
    fn sanitize_strips_directories_and_rejects_the_dangerous() {
        assert_eq!(
            sanitize_filename("diagram.png").as_deref(),
            Some("diagram.png")
        );
        // Traversal is reduced to a basename, never allowed through.
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        // A Windows client sends backslashes.
        assert_eq!(
            sanitize_filename("C:\\users\\me\\y.png").as_deref(),
            Some("y.png")
        );
        // Dotfiles, bare dots and NULs are refused outright.
        assert_eq!(sanitize_filename(".bashrc"), None);
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename("a\0b.txt"), None);
        assert_eq!(sanitize_filename("   "), None);
        // Everything outside [A-Za-z0-9._-] becomes '_'.
        assert_eq!(
            sanitize_filename("my report (1).pdf").as_deref(),
            Some("my_report__1_.pdf")
        );
    }

    #[test]
    fn long_names_are_truncated_but_keep_their_extension() {
        let name = format!("{}.png", "a".repeat(300));
        let out = sanitize_filename(&name).unwrap();
        assert!(out.len() <= 128);
        assert!(
            out.ends_with(".png"),
            "extension drives classify(), so it must survive"
        );
    }

    #[test]
    fn classify_is_extension_driven_and_case_insensitive() {
        assert_eq!(classify("a.PNG"), Some(("image/png", "image")));
        assert_eq!(classify("a.pdf"), Some(("application/pdf", "pdf")));
        assert_eq!(classify("a.md"), Some(("text/markdown", "text")));
        assert_eq!(classify("a.exe"), None);
        assert_eq!(classify("noextension"), None);
        // Excluded on purpose: it executes script when served back.
        assert_eq!(classify("a.svg"), None);
    }

    #[test]
    fn magic_bytes_catch_a_renamed_file() {
        assert!(content_matches(
            "image/png",
            "image",
            &[0x89, b'P', b'N', b'G', 0x0d]
        ));
        // An ELF binary renamed to .png must not pass.
        assert!(!content_matches("image/png", "image", b"\x7fELF\x02\x01"));
        assert!(content_matches("application/pdf", "pdf", b"%PDF-1.7 ..."));
        assert!(!content_matches("application/pdf", "pdf", b"not a pdf"));
        assert!(content_matches(
            "image/jpeg",
            "image",
            &[0xFF, 0xD8, 0xFF, 0xE0]
        ));
        assert!(content_matches(
            "image/webp",
            "image",
            b"RIFF\0\0\0\0WEBPVP8 "
        ));
        assert!(!content_matches(
            "image/webp",
            "image",
            b"RIFF\0\0\0\0AVI LIST"
        ));
        // Text is judged by the absence of NULs, same as files.rs.
        assert!(content_matches("text/plain", "text", b"hello\nworld"));
        assert!(!content_matches("text/plain", "text", b"hel\0lo"));
    }
}
