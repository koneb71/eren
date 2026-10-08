//! Signing in and out, signing up, and the admin's account list.
//!
//! See [`crate::auth`] for when a session is needed and
//! `eren_core::users` for the rules. A password goes in a request body and
//! nowhere else: never into a log line, the ledger (which never reads a
//! body), or a response — except the temporary one an admin's reset makes,
//! returned once to the admin who asked for it.

use super::{internal, require_write, ApiError};
use crate::auth::{cookie, Admin, Caller};
use crate::AppState;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use eren_core::users::{self, Refusal, SignIn};
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/status", get(status))
        .route("/auth/login", post(login))
        .route("/auth/signup", post(signup))
        .route("/auth/logout", post(logout))
        .route("/auth/password", post(change_password))
        .route("/auth/signup-open", put(set_signup))
        .route("/auth/users", get(list_users))
        .route("/auth/users/{id}", patch(update_user))
        .route("/auth/users/{id}/reset-password", post(reset_password))
}

fn refused(e: Refusal) -> ApiError {
    let status = match &e {
        Refusal::Invalid(_) => StatusCode::BAD_REQUEST,
        Refusal::Taken | Refusal::AdminExists => StatusCode::CONFLICT,
        Refusal::NotFound => StatusCode::NOT_FOUND,
        Refusal::SignupClosed => StatusCode::FORBIDDEN,
        Refusal::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

fn peer(ext: &Extensions) -> Option<IpAddr> {
    ext.get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| a.ip().to_canonical())
}

/// Whether accounts are on, whether sign-up is open, and who this is. Asked
/// by the dashboard before anything else, signed in or not.
async fn status(State(state): State<AppState>, ext: Extensions) -> Result<Response, ApiError> {
    let on = state.accounts.is_on(&state.db).await;
    let signup = on && users::signup_open(&state.db).await.map_err(internal)?;
    let user = ext.get::<Caller>().and_then(|c| c.user().cloned());
    Ok(Json(json!({ "accounts": on, "signup": signup, "user": user })).into_response())
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

/// A signed-in response: the cookie, and the account.
async fn signed_in(state: &AppState, user: users::User) -> Result<Response, ApiError> {
    let token = eren_core::sessions::create(&state.db, user.id)
        .await
        .map_err(internal)?;
    let mut res = Json(json!({ "user": user })).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, cookie(Some(&token)));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(res)
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    ext: Extensions,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    require_write(&headers, "signing in starts a session")?;
    if !state.accounts.is_on(&state.db).await {
        return Err((
            StatusCode::CONFLICT,
            "accounts are off; run `eren admin create` to turn them on".into(),
        ));
    }
    let ip = peer(&ext);
    let throttle = state.accounts.throttle();
    if let Some(wait) = throttle.wait(&body.username, ip) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many wrong passwords; try again in {} seconds",
                wait.as_secs().max(1)
            ),
        ));
    }
    match users::verify(&state.db, &body.username, &body.password)
        .await
        .map_err(internal)?
    {
        SignIn::Ok(user) => {
            throttle.succeeded(&body.username, ip);
            signed_in(&state, user).await
        }
        SignIn::Wrong => {
            throttle.failed(&body.username, ip);
            Err((
                StatusCode::UNAUTHORIZED,
                "wrong username or password".into(),
            ))
        }
        SignIn::Disabled => Err((
            StatusCode::FORBIDDEN,
            "this account is disabled; ask the admin".into(),
        )),
    }
}

/// The name the sign-up throttle counts under. Sign-ups are throttled per
/// address rather than per username, because the refusal a guesser is after
/// ("that username is taken") comes from trying many names from one place.
const SIGNUP_THROTTLE_KEY: &str = "\0signup";

async fn signup(
    State(state): State<AppState>,
    headers: HeaderMap,
    ext: Extensions,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    require_write(&headers, "signing up makes an account")?;
    let ip = peer(&ext);
    let throttle = state.accounts.throttle();
    if let Some(wait) = throttle.wait(SIGNUP_THROTTLE_KEY, ip) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many sign-up attempts; try again in {} seconds",
                wait.as_secs().max(1)
            ),
        ));
    }
    match users::sign_up(&state.db, &body.username, &body.password).await {
        Ok(user) => {
            throttle.succeeded(SIGNUP_THROTTLE_KEY, ip);
            signed_in(&state, user).await
        }
        Err(e) => {
            // Every refusal counts — a taken name is the one a guesser wants.
            throttle.failed(SIGNUP_THROTTLE_KEY, ip);
            Err(refused(e))
        }
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    require_write(&headers, "signing out ends a session")?;
    if let Some(token) = crate::auth::session_token(&headers) {
        eren_core::sessions::revoke(&state.db, token)
            .await
            .map_err(internal)?;
    }
    let mut res = Json(json!({ "signedOut": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, cookie(None));
    Ok(res)
}

#[derive(Deserialize)]
struct NewPassword {
    current: String,
    new: String,
}

/// Change one's own password. Every session of the account ends, this one
/// included — so the response signs this browser back in.
async fn change_password(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Json(body): Json<NewPassword>,
) -> Result<Response, ApiError> {
    require_write(&headers, "changing a password")?;
    let Some(user) = caller.user() else {
        return Err((
            StatusCode::CONFLICT,
            "accounts are off, so there is no password to change".into(),
        ));
    };
    users::change_password(&state.db, user.id, &body.current, &body.new)
        .await
        .map_err(refused)?;
    let user = users::get(&state.db, user.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| refused(Refusal::NotFound))?;
    signed_in(&state, user).await
}

async fn list_users(
    State(state): State<AppState>,
    _admin: Admin,
) -> Result<Json<serde_json::Value>, ApiError> {
    let users = users::list(&state.db).await.map_err(internal)?;
    Ok(Json(json!({ "users": users })))
}

#[derive(Deserialize)]
struct UpdateUser {
    disabled: Option<bool>,
}

async fn update_user(
    State(state): State<AppState>,
    _admin: Admin,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_write(&headers, "disabling an account signs it out")?;
    if let Some(disabled) = body.disabled {
        users::set_disabled(&state.db, id, disabled)
            .await
            .map_err(refused)?;
    }
    Ok(Json(json!({ "updated": true })))
}

/// Give an account a temporary password, returned once, here. It signs the
/// account out everywhere and works only to choose a new one.
async fn reset_password(
    State(state): State<AppState>,
    _admin: Admin,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    require_write(&headers, "resetting a password signs the account out")?;
    let password = users::temporary_password();
    users::set_password(&state.db, id, &password, true)
        .await
        .map_err(refused)?;
    let mut res = Json(json!({ "password": password })).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(res)
}

#[derive(Deserialize)]
struct SignupOpen {
    open: bool,
}

async fn set_signup(
    State(state): State<AppState>,
    _admin: Admin,
    headers: HeaderMap,
    Json(body): Json<SignupOpen>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_write(&headers, "opening or closing sign-up")?;
    users::set_signup_open(&state.db, body.open)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "open": body.open })))
}
