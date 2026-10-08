//! Accounts: who may sign in, and which workspaces are theirs.
//!
//! Off until an admin exists. With no admin, Eren is the single-person,
//! loopback-first server it always was — the access token and the loopback
//! rule decide who gets in — so a desktop install never meets a login it did
//! not ask for. `eren admin create` makes the one admin (the database holds
//! "one": a partial unique index), adopts every workspace that has no owner,
//! and from then on every request needs a session ([`crate::sessions`]).
//!
//! Sign-up is closed until the admin opens it on the Users page; while it is
//! open, anyone who can reach the server may make an account, and each new
//! account starts with a workspace of its own. Closed by default because a
//! wide bind is exactly where accounts get turned on, and an open door there
//! is an account for whoever finds the port. The admin can
//! reset a password (a temporary one, which works only to choose a new one)
//! and disable an account. Neither ever shows the admin a password but the
//! temporary one they set.
//!
//! What an account does *not* buy is said in SECURITY.md: every account's
//! agents run as the same OS user against the same disk, so a person who can
//! run an agent with a shell can read what other accounts' agents wrote. The
//! boundary is the dashboard, not the machine.

use crate::db::Db;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

pub const MIN_PASSWORD: usize = 10;
/// Long enough for any passphrase; short enough that hashing one is not a
/// way to make the server work.
pub const MAX_PASSWORD: usize = 256;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    pub must_change_password: bool,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

/// Why an account operation was refused. Every variant but `Internal` is the
/// caller's to fix, and its message says how.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    Invalid(String),
    #[error("that username is taken")]
    Taken,
    #[error("an admin already exists; there is only ever one")]
    AdminExists,
    #[error("no such account")]
    NotFound,
    #[error("sign-up is closed; ask the admin for an account")]
    SignupClosed,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        Refusal::Internal(e.into())
    }
}

/// A username as stored: trimmed and lowercased, then held to the same
/// charset the table's CHECK holds it to, so the refusal names the rule
/// instead of a constraint.
pub fn normalize_username(raw: &str) -> Result<String, Refusal> {
    let name = raw.trim().to_ascii_lowercase();
    let len = name.chars().count();
    if !(3..=32).contains(&len) {
        return Err(Refusal::Invalid("a username is 3 to 32 characters".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(Refusal::Invalid(
            "a username uses letters, digits and . _ - only".into(),
        ));
    }
    Ok(name)
}

pub fn check_password(password: &str) -> Result<(), Refusal> {
    let len = password.chars().count();
    if len < MIN_PASSWORD {
        return Err(Refusal::Invalid(format!(
            "a password is at least {MIN_PASSWORD} characters"
        )));
    }
    if len > MAX_PASSWORD {
        return Err(Refusal::Invalid(format!(
            "a password is at most {MAX_PASSWORD} characters"
        )));
    }
    Ok(())
}

/// Hash off the async threads: argon2 is deliberately slow, and a sign-in
/// should not stall every socket on the same worker.
async fn hash(password: &str) -> anyhow::Result<String> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("could not hash the password: {e}"))
    })
    .await?
}

async fn matches(password: &str, stored: &str) -> bool {
    let (password, stored) = (password.to_string(), stored.to_string());
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&stored)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// A hash of nothing anyone knows, checked against when the username does
/// not exist — so an unknown name costs the same time as a wrong password,
/// and timing says nothing about which names are taken.
static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn dummy_hash() -> &'static str {
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(Uuid::new_v4().as_bytes(), &salt)
            .map(|h| h.to_string())
            .unwrap_or_default()
    })
}

const COLUMNS: &str =
    "id, username, is_admin, must_change_password, disabled_at IS NOT NULL AS disabled, created_at";

fn from_row(r: &sqlx::postgres::PgRow) -> User {
    User {
        id: r.get("id"),
        username: r.get("username"),
        is_admin: r.get("is_admin"),
        must_change_password: r.get("must_change_password"),
        disabled: r.get("disabled"),
        created_at: r.get("created_at"),
    }
}

/// Whether accounts are on: an admin exists.
pub async fn accounts_on(db: &Db) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE is_admin)")
            .fetch_one(&db.pool)
            .await?,
    )
}

pub async fn get(db: &Db, id: Uuid) -> anyhow::Result<Option<User>> {
    Ok(
        sqlx::query(&format!("SELECT {COLUMNS} FROM users WHERE id = $1"))
            .bind(id)
            .fetch_optional(&db.pool)
            .await?
            .as_ref()
            .map(from_row),
    )
}

pub async fn by_username(db: &Db, username: &str) -> anyhow::Result<Option<User>> {
    let Ok(name) = normalize_username(username) else {
        return Ok(None);
    };
    Ok(
        sqlx::query(&format!("SELECT {COLUMNS} FROM users WHERE username = $1"))
            .bind(name)
            .fetch_optional(&db.pool)
            .await?
            .as_ref()
            .map(from_row),
    )
}

pub async fn list(db: &Db) -> anyhow::Result<Vec<User>> {
    Ok(sqlx::query(&format!(
        "SELECT {COLUMNS} FROM users ORDER BY is_admin DESC, username"
    ))
    .fetch_all(&db.pool)
    .await?
    .iter()
    .map(from_row)
    .collect())
}

/// The admin, made from the CLI. Adopts every workspace nobody owns — the
/// work done before accounts existed is the admin's — in the same
/// transaction, so there is no moment with accounts on and orphaned data.
pub async fn create_admin(db: &Db, username: &str, password: &str) -> Result<User, Refusal> {
    let name = normalize_username(username)?;
    check_password(password)?;
    let hashed = hash(password).await?;
    let mut tx = db.pool.begin().await?;
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE is_admin)")
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(Refusal::AdminExists);
    }
    let user = insert(&mut tx, &name, &hashed, true).await?;
    sqlx::query("UPDATE workspaces SET owner_id = $1 WHERE owner_id IS NULL")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    // And what that person carried between projects: their rules and their
    // personal skills.
    crate::rules::adopt_local(&mut tx, user.id).await?;
    // A database with no workspace at all (one somebody emptied) still
    // leaves the admin somewhere to land.
    ensure_workspace(&mut tx, user.id).await?;
    tx.commit().await?;
    Ok(user)
}

/// Somebody signing themselves up. Refused while sign-up is closed (which it
/// is until the admin opens it), and while accounts are off — there is no
/// admin yet to have opened it.
pub async fn sign_up(db: &Db, username: &str, password: &str) -> Result<User, Refusal> {
    let name = normalize_username(username)?;
    check_password(password)?;
    if !accounts_on(db).await? || !signup_open(db).await? {
        return Err(Refusal::SignupClosed);
    }
    let hashed = hash(password).await?;
    let mut tx = db.pool.begin().await?;
    let user = insert(&mut tx, &name, &hashed, false).await?;
    ensure_workspace(&mut tx, user.id).await?;
    tx.commit().await?;
    Ok(user)
}

async fn insert(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    name: &str,
    hashed: &str,
    admin: bool,
) -> Result<User, Refusal> {
    let row = sqlx::query(&format!(
        "INSERT INTO users (username, password_hash, is_admin) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING RETURNING {COLUMNS}"
    ))
    .bind(name)
    .bind(hashed)
    .bind(admin)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(r) => Ok(from_row(&r)),
        // Either index: the name, or a second admin racing the first.
        None if admin => Err(Refusal::AdminExists),
        None => Err(Refusal::Taken),
    }
}

async fn ensure_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO workspaces (name, owner_id)
         SELECT 'My Workspace', $1
          WHERE NOT EXISTS (SELECT 1 FROM workspaces WHERE owner_id = $1)",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// What a sign-in attempt came to.
#[derive(Debug, PartialEq, Eq)]
pub enum SignIn {
    Ok(User),
    /// Wrong name or wrong password — deliberately one answer.
    Wrong,
    Disabled,
}

pub async fn verify(db: &Db, username: &str, password: &str) -> anyhow::Result<SignIn> {
    let row = match normalize_username(username) {
        Ok(name) => {
            sqlx::query(&format!(
                "SELECT {COLUMNS}, password_hash FROM users WHERE username = $1"
            ))
            .bind(name)
            .fetch_optional(&db.pool)
            .await?
        }
        Err(_) => None,
    };
    let Some(row) = row else {
        let _ = matches(password, dummy_hash()).await;
        return Ok(SignIn::Wrong);
    };
    if !matches(password, row.get::<String, _>("password_hash").as_str()).await {
        return Ok(SignIn::Wrong);
    }
    let user = from_row(&row);
    Ok(if user.disabled {
        SignIn::Disabled
    } else {
        SignIn::Ok(user)
    })
}

/// Set a password and sign the account out everywhere. `temporary` is the
/// admin's reset: the account must choose its own at the next sign-in.
pub async fn set_password(
    db: &Db,
    id: Uuid,
    password: &str,
    temporary: bool,
) -> Result<(), Refusal> {
    check_password(password)?;
    let hashed = hash(password).await?;
    let mut tx = db.pool.begin().await?;
    let n =
        sqlx::query("UPDATE users SET password_hash = $2, must_change_password = $3 WHERE id = $1")
            .bind(id)
            .bind(hashed)
            .bind(temporary)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if n == 0 {
        return Err(Refusal::NotFound);
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// A person changing their own password: the current one first, so a
/// browser left signed in is not enough to lock its owner out.
pub async fn change_password(db: &Db, id: Uuid, current: &str, new: &str) -> Result<(), Refusal> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&db.pool)
            .await?;
    let Some(stored) = stored else {
        return Err(Refusal::NotFound);
    };
    if !matches(current, &stored).await {
        return Err(Refusal::Invalid("the current password is wrong".into()));
    }
    if current == new {
        return Err(Refusal::Invalid(
            "the new password is the same as the current one".into(),
        ));
    }
    set_password(db, id, new, false).await
}

/// Disable or re-enable an account. Disabling signs it out everywhere. The
/// admin cannot be disabled: there would be nobody left to undo it.
pub async fn set_disabled(db: &Db, id: Uuid, disabled: bool) -> Result<(), Refusal> {
    let mut tx = db.pool.begin().await?;
    let admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    match admin {
        None => return Err(Refusal::NotFound),
        Some(true) if disabled => {
            return Err(Refusal::Invalid("the admin cannot be disabled".into()))
        }
        _ => {}
    }
    sqlx::query(
        "UPDATE users SET disabled_at = CASE WHEN $2 THEN COALESCE(disabled_at, now()) END
          WHERE id = $1",
    )
    .bind(id)
    .bind(disabled)
    .execute(&mut *tx)
    .await?;
    if disabled {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

const SIGNUP_KEY: &str = "signup";

/// Whether anyone who can reach the server may make an account. Closed unless
/// the admin opened it (`PUT /api/auth/signup-open`, the switch under Users).
pub async fn signup_open(db: &Db) -> anyhow::Result<bool> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(SIGNUP_KEY)
            .fetch_optional(&db.pool)
            .await?;
    Ok(v.and_then(|v| v.get("open").and_then(|o| o.as_bool()))
        .unwrap_or(false))
}

pub async fn set_signup_open(db: &Db, open: bool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(SIGNUP_KEY)
    .bind(serde_json::json!({ "open": open }))
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// The workspaces an account owns, oldest first.
pub async fn workspaces(db: &Db, user: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_id = $1 ORDER BY created_at")
            .bind(user)
            .fetch_all(&db.pool)
            .await?,
    )
}

/// A temporary password for an admin's reset: long, and typeable.
pub fn temporary_password() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    Alphanumeric.sample_string(&mut rand::rng(), 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_folded_and_held_to_one_charset() {
        assert_eq!(normalize_username("  Neiell ").unwrap(), "neiell");
        assert_eq!(normalize_username("a.b_c-1").unwrap(), "a.b_c-1");
        for bad in ["ab", "has space", "émile", "x".repeat(33).as_str(), "a/b"] {
            assert!(normalize_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords_have_a_floor_and_a_ceiling() {
        assert!(check_password("short").is_err());
        assert!(check_password("long enough!").is_ok());
        assert!(check_password(&"x".repeat(MAX_PASSWORD + 1)).is_err());
    }

    #[test]
    fn a_temporary_password_clears_the_floor() {
        assert!(check_password(&temporary_password()).is_ok());
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::testdb;

    #[tokio::test]
    async fn the_first_admin_adopts_what_nobody_owned_and_there_is_only_one() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        assert!(!accounts_on(db).await.unwrap());
        // Accounts off: nobody can sign up, there is no admin to have opened it.
        assert!(matches!(
            sign_up(db, "early", "a long password").await,
            Err(Refusal::SignupClosed)
        ));

        let seeded: Uuid = sqlx::query_scalar("SELECT id FROM workspaces LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let admin = create_admin(db, "Admin", "a long password").await.unwrap();
        assert!(admin.is_admin);
        assert_eq!(admin.username, "admin");
        assert!(accounts_on(db).await.unwrap());
        assert_eq!(workspaces(db, admin.id).await.unwrap(), vec![seeded]);

        assert!(matches!(
            create_admin(db, "second", "a long password").await,
            Err(Refusal::AdminExists)
        ));
        t.finish().await;
    }

    /// Sign-up is closed until the admin opens it: turning accounts on must
    /// never, by itself, let whoever can reach the port make an account.
    #[tokio::test]
    async fn sign_up_is_closed_until_the_admin_opens_it() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let admin = create_admin(db, "admin", "a long password").await.unwrap();
        assert!(!signup_open(db).await.unwrap());
        assert!(matches!(
            sign_up(db, "bea", "another password").await,
            Err(Refusal::SignupClosed)
        ));

        set_signup_open(db, true).await.unwrap();
        let user = sign_up(db, "Bea", "another password").await.unwrap();
        let mine = workspaces(db, user.id).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert!(!workspaces(db, admin.id).await.unwrap().contains(&mine[0]));
        assert!(matches!(
            sign_up(db, "bea", "another password").await,
            Err(Refusal::Taken)
        ));

        set_signup_open(db, false).await.unwrap();
        assert!(matches!(
            sign_up(db, "cal", "another password").await,
            Err(Refusal::SignupClosed)
        ));
        t.finish().await;
    }

    #[tokio::test]
    async fn sign_in_says_one_thing_for_a_wrong_name_or_password() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        create_admin(db, "admin", "a long password").await.unwrap();
        set_signup_open(db, true).await.unwrap();
        let bea = sign_up(db, "bea", "another password").await.unwrap();

        assert!(matches!(
            verify(db, "BEA", "another password").await.unwrap(),
            SignIn::Ok(u) if u.id == bea.id
        ));
        assert_eq!(
            verify(db, "bea", "nope nope nope").await.unwrap(),
            SignIn::Wrong
        );
        assert_eq!(
            verify(db, "nobody", "another password").await.unwrap(),
            SignIn::Wrong
        );

        set_disabled(db, bea.id, true).await.unwrap();
        assert_eq!(
            verify(db, "bea", "another password").await.unwrap(),
            SignIn::Disabled
        );
        t.finish().await;
    }

    #[tokio::test]
    async fn a_reset_is_temporary_and_signs_the_account_out() {
        let Some(t) = testdb::fresh().await else {
            return;
        };
        let db = &t.db;
        let admin = create_admin(db, "admin", "a long password").await.unwrap();
        set_signup_open(db, true).await.unwrap();
        let bea = sign_up(db, "bea", "another password").await.unwrap();
        let token = crate::sessions::create(db, bea.id).await.unwrap();

        set_password(db, bea.id, "temporary pass", true)
            .await
            .unwrap();
        assert!(crate::sessions::lookup(db, &token).await.unwrap().is_none());
        let SignIn::Ok(u) = verify(db, "bea", "temporary pass").await.unwrap() else {
            panic!("the temporary password signs in");
        };
        assert!(u.must_change_password);

        change_password(db, bea.id, "temporary pass", "my own password")
            .await
            .unwrap();
        let SignIn::Ok(u) = verify(db, "bea", "my own password").await.unwrap() else {
            panic!("the new password signs in");
        };
        assert!(!u.must_change_password);

        assert!(set_disabled(db, admin.id, true).await.is_err());
        t.finish().await;
    }
}
