use crate::{AppError, Shared, now};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    routing::{get, post, put},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

const SESSION_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, Serialize)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub role: String,
}

impl FromRequestParts<Shared> for AuthUser {
    type Rejection = AppError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        let conn = state.conn()?;
        current_user(&conn, &parts.headers)?.ok_or_else(unauthorized)
    }
}

pub fn require_admin(user: &AuthUser) -> Result<(), AppError> {
    if user.role == "admin" {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::FORBIDDEN,
            "Administrator access required".into(),
        ))
    }
}

fn unauthorized() -> AppError {
    AppError(StatusCode::UNAUTHORIZED, "Please sign in".into())
}

fn invalid_credentials() -> AppError {
    AppError(
        StatusCode::UNAUTHORIZED,
        "Invalid username or password".into(),
    )
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('admin','user')),
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sessions_expiry ON sessions(expires_at);
        CREATE TABLE IF NOT EXISTS login_attempts (
            username TEXT PRIMARY KEY, attempts INTEGER NOT NULL, started_at INTEGER NOT NULL
        );
    ",
    )?;
    crate::ensure_column(conn, "users", "disabled", "INTEGER NOT NULL DEFAULT 0")
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/password", post(change_password))
        .route("/api/users", get(users).post(create_user))
        .route("/api/users/{id}", put(update_user).delete(delete_user))
        .route("/api/users/{id}/password", post(reset_password))
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == "polyview_session").then_some(value))
}

pub(crate) fn current_user(
    conn: &Connection,
    headers: &HeaderMap,
) -> Result<Option<AuthUser>, AppError> {
    let Some(token) = cookie_token(headers) else {
        return Ok(None);
    };
    Ok(conn.query_row(
        "SELECT u.id,u.username,u.role FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token=? AND s.expires_at>? AND u.disabled=0",
        params![token, now()],
        |row| Ok(AuthUser {id: row.get(0)?, username: row.get(1)?, role: row.get(2)?}),
    ).optional()?)
}

fn current_admin(conn: &Connection, headers: &HeaderMap) -> Result<AuthUser, AppError> {
    let user = current_user(conn, headers)?.ok_or_else(unauthorized)?;
    require_admin(&user)?;
    Ok(user)
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct AuthStatus {
    needs_setup: bool,
    user: Option<AuthUser>,
}

async fn status(
    State(state): State<Shared>,
    headers: HeaderMap,
) -> Result<Json<AuthStatus>, AppError> {
    let conn = state.conn()?;
    let needs_setup = conn.query_row("SELECT COUNT(*)=0 FROM users", [], |r| r.get(0))?;
    Ok(Json(AuthStatus {
        needs_setup,
        user: current_user(&conn, &headers)?,
    }))
}

fn validate_credentials(username: &str, password: &str) -> Result<(), AppError> {
    if !(3..=32).contains(&username.len())
        || !username
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(AppError::bad_request(
            "Username must be 3–32 letters, digits, underscores or hyphens",
        ));
    }
    validate_password(password)
}

fn validate_password(password: &str) -> Result<(), AppError> {
    if !(10..=128).contains(&password.chars().count()) {
        return Err(AppError::bad_request(
            "Password must contain 10–128 characters",
        ));
    }
    Ok(())
}

async fn hash_password(password: String) -> Result<String, AppError> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|hash| hash.to_string())
            .map_err(|e| AppError::internal(e.to_string()))
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?
}

async fn verify_password(password: String, hash: String) -> Result<Option<String>, AppError> {
    tokio::task::spawn_blocking(move || {
        let valid = PasswordHash::new(&hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        valid.then_some(hash)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))
}

fn session_cookie(state: &Shared, token: &str, seconds: i64) -> String {
    format!(
        "polyview_session={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={seconds}{}",
        if state.secure_cookie { "; Secure" } else { "" }
    )
}

fn sign_in(
    state: &Shared,
    tx: rusqlite::Transaction<'_>,
    id: i64,
    verified_hash: &str,
) -> Result<(HeaderMap, Json<AuthUser>), AppError> {
    // Verification runs outside SQLite. Recheck the same credentials and current role
    // in the transaction that issues the session, so a reset or disable takes effect.
    let (username, role, disabled): (String, String, bool) = tx
        .query_row(
            "SELECT username,role,disabled FROM users WHERE id=? AND password_hash=?",
            params![id, verified_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(invalid_credentials)?;
    tx.execute("DELETE FROM login_attempts WHERE username=?", [&username])?;
    if disabled {
        tx.commit()?;
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Account is disabled".into(),
        ));
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    tx.execute("DELETE FROM sessions WHERE expires_at<=?", [now()])?;
    tx.execute(
        "INSERT INTO sessions(token,user_id,expires_at) VALUES(?,?,?)",
        params![token, id, now() + SESSION_SECONDS],
    )?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        session_cookie(state, &token, SESSION_SECONDS)
            .parse()
            .map_err(|_| AppError::internal("Invalid session cookie"))?,
    );
    tx.commit()?;
    Ok((headers, Json(AuthUser { id, username, role })))
}

async fn setup(
    State(state): State<Shared>,
    Json(input): Json<Credentials>,
) -> Result<(HeaderMap, Json<AuthUser>), AppError> {
    validate_credentials(&input.username, &input.password)?;
    if state
        .conn()?
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?
        != 0
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Setup has already been completed".into(),
        ));
    }
    let hash = hash_password(input.password).await?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    if tx.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))? != 0 {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Setup has already been completed".into(),
        ));
    }
    tx.execute(
        "INSERT INTO users(username,password_hash,role,created_at) VALUES(?,?,'admin',?)",
        params![input.username, hash, now()],
    )?;
    let id = tx.last_insert_rowid();
    sign_in(&state, tx, id, &hash)
}

async fn login(
    State(state): State<Shared>,
    Json(input): Json<Credentials>,
) -> Result<(HeaderMap, Json<AuthUser>), AppError> {
    if input.username.is_empty()
        || input.username.chars().count() > 128
        || input.username.chars().any(char::is_control)
        || input.password.is_empty()
        || input.password.chars().count() > 128
    {
        return Err(AppError::bad_request(
            "Enter a username and password of at most 128 characters",
        ));
    }
    let record = {
        let conn = state.conn()?;
        conn.execute(
            "DELETE FROM login_attempts WHERE started_at<?",
            [now() - 900],
        )?;
        let attempts: i64 = conn
            .query_row(
                "SELECT attempts FROM login_attempts WHERE username=?",
                [&input.username],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if attempts >= 10 {
            return Err(AppError(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts; try again in 15 minutes".into(),
            ));
        }
        // Reserve the attempt before hashing so concurrent requests share the same limit.
        conn.execute("INSERT INTO login_attempts(username,attempts,started_at) VALUES(?,1,?) ON CONFLICT(username) DO UPDATE SET attempts=attempts+1", params![input.username, now()])?;
        conn.query_row(
            "SELECT id,password_hash FROM users WHERE username=?",
            [&input.username],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    };
    let (id, hash) = record.ok_or_else(invalid_credentials)?;
    let verified = verify_password(input.password, hash)
        .await?
        .ok_or_else(invalid_credentials)?;
    let mut conn = state.conn()?;
    sign_in(&state, conn.transaction()?, id, &verified)
}

async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Result<HeaderMap, AppError> {
    if let Some(token) = cookie_token(&headers) {
        state
            .conn()?
            .execute("DELETE FROM sessions WHERE token=?", [token])?;
    }
    let mut response = HeaderMap::new();
    response.insert(
        header::SET_COOKIE,
        session_cookie(&state, "", 0)
            .parse()
            .map_err(|_| AppError::internal("Invalid session cookie"))?,
    );
    Ok(response)
}

#[derive(Serialize)]
struct Member {
    #[serde(flatten)]
    user: AuthUser,
    disabled: bool,
}

async fn users(
    State(state): State<Shared>,
    headers: HeaderMap,
) -> Result<Json<Vec<Member>>, AppError> {
    let conn = state.conn()?;
    current_admin(&conn, &headers)?;
    let mut stmt = conn.prepare("SELECT id,username,role,disabled FROM users ORDER BY id")?;
    let users = stmt
        .query_map([], |r| {
            Ok(Member {
                user: AuthUser {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    role: r.get(2)?,
                },
                disabled: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(users))
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    role: String,
}

fn validate_role(role: &str) -> Result<(), AppError> {
    if !matches!(role, "admin" | "user") {
        return Err(AppError::bad_request("Invalid role"));
    }
    Ok(())
}

async fn create_user(
    State(state): State<Shared>,
    user: AuthUser,
    headers: HeaderMap,
    Json(input): Json<NewUser>,
) -> Result<(StatusCode, Json<AuthUser>), AppError> {
    require_admin(&user)?;
    validate_credentials(&input.username, &input.password)?;
    validate_role(&input.role)?;
    let hash = hash_password(input.password).await?;
    let conn = state.conn()?;
    current_admin(&conn, &headers)?;
    match conn.execute(
        "INSERT INTO users(username,password_hash,role,created_at) VALUES(?,?,?,?)",
        params![input.username, hash, input.role, now()],
    ) {
        Err(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(AppError(
                StatusCode::CONFLICT,
                "Username is already taken".into(),
            ));
        }
        result => {
            result?;
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(AuthUser {
            id: conn.last_insert_rowid(),
            username: input.username,
            role: input.role,
        }),
    ))
}

fn protect_last_admin(conn: &Connection, id: i64) -> Result<(), AppError> {
    let last = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND role='admin' AND disabled=0)
         AND NOT EXISTS(SELECT 1 FROM users WHERE id<>?1 AND role='admin' AND disabled=0)",
        [id],
        |row| row.get::<_, bool>(0),
    )?;
    if last {
        return Err(AppError::bad_request(
            "Keep at least one active administrator",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct MemberUpdate {
    role: Option<String>,
    disabled: Option<bool>,
}

async fn update_user(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(input): Json<MemberUpdate>,
) -> Result<StatusCode, AppError> {
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let admin = current_admin(&tx, &headers)?;
    if let Some(role) = &input.role {
        validate_role(role)?;
    } else if input.disabled.is_none() {
        return Err(AppError::bad_request("Provide a role or disabled flag"));
    }
    if input.role.as_deref() == Some("user") || input.disabled == Some(true) {
        if id == admin.id {
            return Err(AppError::bad_request(
                "You cannot demote or disable your own account",
            ));
        }
        protect_last_admin(&tx, id)?;
    }
    if tx.execute(
        "UPDATE users SET role=COALESCE(?,role),disabled=COALESCE(?,disabled) WHERE id=?",
        params![input.role, input.disabled, id],
    )? == 0
    {
        return Err(AppError::not_found("User not found"));
    }
    if input.disabled == Some(true) {
        tx.execute("DELETE FROM sessions WHERE user_id=?", [id])?;
    }
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let admin = current_admin(&tx, &headers)?;
    if admin.id == id {
        return Err(AppError::bad_request("You cannot delete your own account"));
    }
    protect_last_admin(&tx, id)?;
    if tx.execute("DELETE FROM users WHERE id=?", [id])? == 0 {
        return Err(AppError::not_found("User not found"));
    }
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PasswordReset {
    password: String,
}

async fn reset_password(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(input): Json<PasswordReset>,
) -> Result<StatusCode, AppError> {
    let old_hash: String = {
        let conn = state.conn()?;
        let admin = current_admin(&conn, &headers)?;
        if id == admin.id {
            return Err(AppError::bad_request(
                "Use account settings to change your own password",
            ));
        }
        validate_password(&input.password)?;
        conn.query_row("SELECT password_hash FROM users WHERE id=?", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| AppError::not_found("User not found"))?
    };
    let new_hash = hash_password(input.password).await?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    current_admin(&tx, &headers)?;
    if tx.execute(
        "UPDATE users SET password_hash=? WHERE id=? AND password_hash=?",
        params![new_hash, id, old_hash],
    )? == 0
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Account changed during reset".into(),
        ));
    }
    tx.execute("DELETE FROM sessions WHERE user_id=?", [id])?;
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PasswordChange {
    current_password: String,
    password: String,
}

async fn change_password(
    State(state): State<Shared>,
    user: AuthUser,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> Result<StatusCode, AppError> {
    validate_password(&input.password)?;
    if input.current_password.len() > 512 {
        return Err(AppError::bad_request("Invalid current password"));
    }
    let old_hash: String = state.conn()?.query_row(
        "SELECT password_hash FROM users WHERE id=?",
        [user.id],
        |r| r.get(0),
    )?;
    let old_hash = verify_password(input.current_password, old_hash)
        .await?
        .ok_or_else(|| {
            AppError(
                StatusCode::UNAUTHORIZED,
                "Current password is incorrect".into(),
            )
        })?;
    let new_hash = hash_password(input.password).await?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    current_user(&tx, &headers)?.ok_or_else(unauthorized)?;
    if tx.execute(
        "UPDATE users SET password_hash=? WHERE id=? AND password_hash=?",
        params![new_hash, user.id, old_hash],
    )? == 0
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Password changed; please sign in again".into(),
        ));
    }
    tx.execute(
        "DELETE FROM sessions WHERE user_id=? AND token<>?",
        params![user.id, cookie_token(&headers).unwrap_or("")],
    )?;
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_migration_preserves_existing_users_and_counts_only_active_admins() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, username TEXT UNIQUE, password_hash TEXT, role TEXT, created_at INTEGER);
             INSERT INTO users VALUES(1,'first','hash','admin',0),(2,'second','hash','admin',0);",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        assert!(protect_last_admin(&conn, 1).is_ok());
        conn.execute("UPDATE users SET disabled=1 WHERE id=2", [])
            .unwrap();
        init_schema(&conn).unwrap();
        assert_eq!(
            protect_last_admin(&conn, 1).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert!(protect_last_admin(&conn, 2).is_ok());
    }

    #[test]
    fn validates_accounts_and_parses_only_our_cookie() {
        assert!(validate_credentials("admin", "long-pass-123").is_ok());
        assert!(validate_credentials("../etc", "long-pass-123").is_err());
        assert!(validate_credentials("admin", "short").is_err());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=a; polyview_session=secret; third=c".parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers), Some("secret"));
    }

    #[tokio::test]
    async fn passwords_are_salted_and_verified() {
        let first = hash_password("long-pass-123".into()).await.unwrap();
        let second = hash_password("long-pass-123".into()).await.unwrap();
        assert_ne!(first, second);
        assert!(
            verify_password("long-pass-123".into(), first.clone())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            verify_password("wrong-pass-123".into(), first)
                .await
                .unwrap()
                .is_none()
        );
    }
}
