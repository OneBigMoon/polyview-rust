use super::*;
use axum::extract::rejection::JsonRejection;
use rusqlite::Row;
use std::collections::HashSet;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Provider {
    Qbittorrent,
    Transmission,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
            Self::Transmission => "transmission",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Account {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) provider: Provider,
    pub(super) endpoint: String,
    pub(super) username: String,
    pub(super) password: String,
    pub(super) remote_root: String,
    pub(super) local_root: String,
    pub(super) members_allowed: bool,
}

pub(crate) type Backup = Vec<Account>;
const SELECT: &str = "SELECT id,name,provider,endpoint,username,password,remote_root,local_root,members_allowed FROM downloaders";

fn row(r: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        name: r.get(1)?,
        provider: match r.get::<_, String>(2)?.as_str() {
            "qbittorrent" => Provider::Qbittorrent,
            "transmission" => Provider::Transmission,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        endpoint: r.get(3)?,
        username: r.get(4)?,
        password: r.get(5)?,
        remote_root: r.get(6)?,
        local_root: r.get(7)?,
        members_allowed: r.get(8)?,
    })
}

impl Account {
    fn validate(&self) -> Result<(), AppError> {
        let endpoint = crate::network::http_url(&self.endpoint)?;
        if endpoint.query().is_some() || endpoint.fragment().is_some() || self.endpoint.len() > 2048
        {
            return Err(AppError::bad_request(
                "Use a downloader endpoint without a query or fragment",
            ));
        }
        if self.name.trim().is_empty()
            || self.name.len() > 160
            || self.name.chars().any(char::is_control)
        {
            return Err(AppError::bad_request("Invalid downloader name"));
        }
        if [&self.username, &self.password]
            .iter()
            .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err(AppError::bad_request("Invalid downloader credentials"));
        }
        if self.remote_root.is_empty() != self.local_root.is_empty()
            || [&self.remote_root, &self.local_root]
                .iter()
                .any(|v| !v.is_empty() && !super::worker::clean_absolute(v))
        {
            return Err(AppError::bad_request(
                "Mount mapping requires two absolute directory paths",
            ));
        }
        Ok(())
    }

    fn store(&self, conn: &Connection) -> Result<i64, AppError> {
        self.validate()?;
        Ok(conn.query_row("INSERT INTO downloaders(id,name,provider,endpoint,username,password,remote_root,local_root,members_allowed,revision)
            VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name, provider=excluded.provider, endpoint=excluded.endpoint,username=excluded.username,password=excluded.password,remote_root=excluded.remote_root,local_root=excluded.local_root,members_allowed=excluded.members_allowed,revision=excluded.revision RETURNING id",
            params![(self.id > 0).then_some(self.id),self.name,self.provider.name(),self.endpoint,self.username,self.password,self.remote_root,self.local_root,self.members_allowed,uuid::Uuid::new_v4().to_string()],|r|r.get(0))?)
    }

    fn view(&self) -> Value {
        json!({"id":self.id,"name":self.name,"provider":self.provider,"endpoint":self.endpoint,"has_credentials":!self.username.is_empty() || !self.password.is_empty(),"remote_root":self.remote_root,"local_root":self.local_root,"members_allowed":self.members_allowed})
    }
}

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    Ok(conn
        .prepare(&format!("{SELECT} ORDER BY id"))?
        .query_map([], row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn restore_config(conn: &Connection, entries: Backup) -> Result<(), AppError> {
    let mut ids = HashSet::new();
    for entry in &entries {
        entry.validate()?;
        if entry.id <= 0 || !ids.insert(entry.id) {
            return Err(AppError::bad_request("Invalid or repeated downloader ID"));
        }
    }
    for old in export_config(conn)? {
        if !entries
            .iter()
            .any(|v| v.id == old.id && v.provider == old.provider && v.endpoint == old.endpoint)
        {
            conn.execute("DELETE FROM downloaders WHERE id=?", [old.id])?;
        }
    }
    for entry in entries {
        entry.store(conn)?;
    }
    Ok(())
}

pub(super) fn load(conn: &Connection, id: i64) -> Result<(Account, String), AppError> {
    let account = conn
        .query_row(&format!("{SELECT} WHERE id=?"), [id], row)
        .optional()?
        .ok_or_else(missing)?;
    let revision = conn.query_row("SELECT revision FROM downloaders WHERE id=?", [id], |r| {
        r.get(0)
    })?;
    Ok((account, revision))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    name: String,
    provider: Provider,
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    #[serde(default)]
    remote_root: String,
    #[serde(default)]
    local_root: String,
    #[serde(default)]
    members_allowed: bool,
}

type Payload = Result<Json<Input>, JsonRejection>;

fn save(
    state: &Shared,
    user: &AuthUser,
    id: Option<i64>,
    input: Payload,
) -> Result<Value, AppError> {
    require_admin(user)?;
    let Json(input) =
        input.map_err(|_| AppError::bad_request("Invalid downloader configuration"))?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let mut username = String::new();
    let mut password = String::new();
    if let Some(id) = id {
        let (old, _) = load(&tx, id)?;
        if old.provider != input.provider || old.endpoint != input.endpoint.trim() {
            // A different server is a different authority for task ownership and credentials.
            tx.execute("DELETE FROM downloaders WHERE id=?", [id])?;
        } else {
            username = old.username;
            password = old.password;
        }
    }
    let mut account = Account {
        id: id.unwrap_or(0),
        name: input.name.trim().to_owned(),
        provider: input.provider,
        endpoint: input.endpoint.trim().to_owned(),
        username: input.username.unwrap_or(username),
        password: input.password.unwrap_or(password),
        remote_root: input.remote_root,
        local_root: input.local_root,
        members_allowed: input.members_allowed,
    };
    account.id = account.store(&tx)?;
    tx.commit()?;
    Ok(account.view())
}

pub(super) async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    body: Payload,
) -> Result<(StatusCode, Json<Value>), AppError> {
    Ok((StatusCode::CREATED, Json(save(&state, &user, None, body)?)))
}

pub(super) async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    body: Payload,
) -> Result<Json<Value>, AppError> {
    Ok(Json(save(&state, &user, Some(id), body)?))
}

pub(super) async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM downloaders WHERE id=?", [id])?
        == 0
    {
        return Err(missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn accounts(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Value>>, AppError> {
    require_admin(&user)?;
    Ok(Json(
        export_config(&*state.conn()?)?
            .iter()
            .map(Account::view)
            .collect(),
    ))
}

pub(super) async fn choices(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Value>>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id,name,provider FROM downloaders WHERE ? OR members_allowed ORDER BY id",
    )?;
    Ok(Json(stmt.query_map([user.role == "admin"], |r|Ok(json!({"id":r.get::<_,i64>(0)?,"name":r.get::<_,String>(1)?,"provider":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<_>>()?))
}
