#[cfg(unix)]
#[path = "terminal.rs"]
mod terminal;
#[cfg(unix)]
pub use terminal::terminal_login;

#[cfg(not(unix))]
pub async fn terminal_login(
    _: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("Native terminal enrollment requires a Unix controlling terminal".into())
}

use super::native::{AuthState, NativeAccount};
use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, set_setting_json},
};
use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    routing::get,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

const SETTINGS: &str = "telegram_native";
const LIFETIME: i64 = 120;
const COOLDOWN: i64 = 15;

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS native_telegram_accounts (
            user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            account_key TEXT NOT NULL UNIQUE, attempt TEXT NOT NULL UNIQUE,
            phase TEXT NOT NULL, telegram_id INTEGER, expires_at INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS native_telegram_host_gate (
            id INTEGER PRIMARY KEY CHECK(id=1), last_started INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

#[derive(Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeBackup {
    api_id: i32,
    api_hash: String,
}

impl NativeBackup {
    fn configured(&self) -> bool {
        self.api_id > 0
            && self.api_hash.len() == 32
            && self.api_hash.bytes().all(|c| c.is_ascii_hexdigit())
    }
}

pub(crate) fn export_config(conn: &Connection) -> Result<NativeBackup, AppError> {
    get_setting_json(conn, SETTINGS)
}

pub(crate) fn validate_config(_: &Connection, config: &NativeBackup) -> Result<(), AppError> {
    if config.configured() || (config.api_id == 0 && config.api_hash.is_empty()) {
        Ok(())
    } else {
        Err(AppError::bad_request("Telegram API ID 或 Hash 格式错误"))
    }
}

pub(crate) fn restore_config(conn: &Connection, config: NativeBackup) -> Result<(), AppError> {
    validate_config(conn, &config)?;
    if export_config(conn)? != config {
        conn.execute(
            "UPDATE native_telegram_accounts SET phase='expired' WHERE phase IN ('confirm','scan','terminal','claimed')",
            [],
        )?;
        set_setting_json(conn, SETTINGS, &config)?;
    }
    Ok(())
}

pub(super) fn verified_account(
    conn: &Connection,
    user_id: i64,
) -> Result<Option<NativeAccount>, AppError> {
    let pair: Option<(String,String)> = conn.query_row(
        "SELECT account_key, attempt FROM native_telegram_accounts a JOIN users u ON u.id=a.user_id
         WHERE a.user_id=? AND u.disabled=0 AND a.phase='authorized' AND a.telegram_id>0",
        [user_id], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let Some((key, attempt)) = pair else {
        return Ok(None);
    };
    let config = export_config(conn)?;
    if !config.configured() {
        return Ok(None);
    }
    let parse = |id: &str| {
        Uuid::parse_str(id).map_err(|_| AppError::internal("Invalid native account scope"))
    };
    Ok(Some(NativeAccount {
        user_id,
        account_key: parse(&key)?,
        attempt: parse(&attempt)?,
        api_id: config.api_id,
        api_hash: config.api_hash,
    }))
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/api/telegram/native/login",
            get(login).post(login).delete(login),
        )
        .route(
            "/api/settings/telegram/native",
            get(read_config).put(save_config),
        )
}

async fn read_config(State(state): State<Shared>, user: AuthUser) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    Ok(Json(
        json!({"configured":export_config(&*state.conn()?)?.configured()}),
    ))
}

async fn save_config(
    State(state): State<Shared>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Value>, AppError> {
    require_admin(&super::signed_in(&*state.conn()?, &headers)?)?;
    let invalid = || AppError::bad_request("Telegram API ID 或 Hash 格式错误");
    let bytes = to_bytes(request.into_body(), 4096)
        .await
        .map_err(|_| invalid())?;
    let config: NativeBackup = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !config.configured() {
        return Err(invalid());
    }
    let mut conn = state.conn()?;
    require_admin(&super::signed_in(&conn, &headers)?)?;
    let tx = conn.transaction()?;
    restore_config(&tx, config)?;
    tx.commit()?;
    Ok(Json(json!({"configured":true})))
}

fn status(phase: &str) -> Json<Value> {
    let phase = if phase == "claimed" {
        "terminal"
    } else {
        phase
    };
    let message = match phase {
        "unconfigured" => "请管理员先配置 Telegram API ID 和 Hash。",
        "idle" => "可以申请在主机授权。",
        "confirm" => "请在主机桌面核对并确认 PolyView 成员名称和编号。",
        "scan" => "请使用 Telegram 官方手机应用的设备扫码功能扫描主机屏幕，并在主机确认账户绑定。",
        "terminal" => "请在运行 PolyView 的主机终端核对成员身份并完成授权。",
        "authorized" => "Telegram 账户已绑定。",
        "official" => "Telegram 要求额外验证。本次授权已停止，请使用官方客户端在本机完成验证。",
        "expired" => "本次授权已过期，请重新申请。",
        "declined" => "主机已拒绝本次授权。",
        _ => "授权失败，请管理员检查主机桌面、TDLib 和本地授权工具。",
    };
    Json(json!({"state":phase,"message":message}))
}

// A binding survives process restarts, but Telegram can revoke the device
// elsewhere. A transient provider failure leaves the on-disk association intact.
async fn bound_status(
    state: &Shared,
    headers: &HeaderMap,
    account: NativeAccount,
) -> Result<Json<Value>, AppError> {
    let attempt = account.attempt;
    let member = account.user_id;
    let Ok(client) = state.telegram_native.client(account).await else {
        return Ok(status("error"));
    };
    let auth = client.auth().borrow().clone();
    let identity = match auth {
        AuthState::Ready => {
            let me = match client.request(json!({"@type":"getMe"})).await {
                Ok(me) => me,
                Err(_) => return Ok(status("error")),
            };
            let Some(id) = me["id"]
                .as_i64()
                .filter(|id| *id > 0 && me["@type"] == "user")
            else {
                return Ok(status("error"));
            };
            Some(id)
        }
        AuthState::Phone
        | AuthState::Code
        | AuthState::Password
        | AuthState::Unsupported
        | AuthState::Qr(_) => None,
        _ => return Ok(status("error")),
    };
    {
        let conn = state.conn()?;
        let user = super::signed_in(&conn, headers)?;
        if user.id != member
            || verified_account(&conn, member)?.is_none_or(|a| a.attempt != attempt)
        {
            return Ok(status("expired"));
        }
        let reset = conn.execute(
            "UPDATE native_telegram_accounts SET phase='expired',telegram_id=NULL
            WHERE user_id=? AND attempt=? AND phase='authorized' AND telegram_id IS NOT ?",
            params![member, attempt.to_string(), identity],
        )?;
        if reset == 0 {
            return Ok(status("authorized"));
        }
    }
    state.telegram_native.abandon(member, attempt).await;
    Ok(status("expired"))
}

async fn login(
    State(state): State<Shared>,
    prompt: Option<Extension<HostPrompt>>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Value>, AppError> {
    super::signed_in(&*state.conn()?, &headers)?;
    let (parts, body) = request.into_parts();
    if parts.uri.query().is_some() || to_bytes(body, 0).await.is_err() {
        return Err(AppError::bad_request("此操作不接收参数"));
    }
    if parts.method == Method::GET || parts.method == Method::POST {
        let account = {
            let conn = state.conn()?;
            let member = super::signed_in(&conn, &headers)?;
            verified_account(&conn, member.id)?
        };
        if let Some(account) = account {
            return bound_status(&state, &headers, account).await;
        }
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let user = super::signed_in(&tx, &headers)?;
    let config = export_config(&tx)?;
    if !config.configured() {
        return Ok(status("unconfigured"));
    }
    let now = crate::now();
    tx.execute("UPDATE native_telegram_accounts SET phase='expired' WHERE phase IN ('confirm','scan','terminal','claimed') AND expires_at<=?", [now])?;
    if parts.method == Method::DELETE {
        tx.execute("UPDATE native_telegram_accounts SET phase='idle' WHERE user_id=? AND phase IN ('confirm','scan','terminal','claimed')", [user.id])?;
    }
    let phase: String = tx
        .query_row(
            "SELECT phase FROM native_telegram_accounts WHERE user_id=?",
            [user.id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "idle".into());
    if parts.method != Method::POST
        || matches!(
            phase.as_str(),
            "confirm" | "scan" | "terminal" | "claimed" | "authorized"
        )
    {
        tx.commit()?;
        return Ok(status(&phase));
    }
    let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM native_telegram_accounts WHERE phase IN ('confirm','scan','terminal','claimed')) OR EXISTS(SELECT 1 FROM native_telegram_host_gate WHERE last_started>?)", [now-COOLDOWN], |r| r.get(0))?;
    if busy {
        return Err(AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "主机授权正在使用或刚刚完成，请稍后再试。".into(),
        ));
    }
    let account = NativeAccount {
        user_id: user.id,
        account_key: Uuid::new_v4(),
        attempt: Uuid::new_v4(),
        api_id: config.api_id,
        api_hash: config.api_hash,
    };
    let attempt = account.attempt.to_string();
    let prompt = prompt.map(|ext| ext.0).unwrap_or_default();
    let desktop = cfg!(target_os = "macos") || prompt.helper().is_some();
    let phase = if !cfg!(unix) || desktop {
        "confirm"
    } else {
        "terminal"
    };
    tx.execute("INSERT INTO native_telegram_accounts(user_id,account_key,attempt,phase,expires_at) VALUES(?,?,?,?,?)
        ON CONFLICT(user_id) DO UPDATE SET account_key=excluded.account_key,attempt=excluded.attempt,phase=excluded.phase,telegram_id=NULL,expires_at=excluded.expires_at", params![user.id,account.account_key.to_string(),attempt,phase,now+LIFETIME])?;
    tx.execute(
        "INSERT OR REPLACE INTO native_telegram_host_gate VALUES(1,?)",
        [now],
    )?;
    tx.commit()?;
    if phase == "confirm" {
        tokio::spawn(run(state.clone(), user, account, prompt));
    }
    Ok(status(phase))
}

// This is a host-side dependency. HTTP handlers never accept executable paths,
// native client arguments, QR payloads, or Telegram credentials from members.
#[derive(Clone, Default)]
pub struct HostPrompt(Option<PathBuf>);

impl HostPrompt {
    pub fn with_helper(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn helper(&self) -> Option<PathBuf> {
        self.0.clone().or_else(|| {
            std::env::var_os("POLYVIEW_TELEGRAM_HOST_HELPER")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
        })
    }

    async fn dialog(&self, member: &AuthUser, event: &str, value: Value) -> Result<bool, AppError> {
        let error = || AppError::internal("Local Telegram dialog unavailable");
        let mut cmd = match self.helper() {
            Some(path) if path.is_absolute() => Command::new(path),
            None if cfg!(target_os = "macos") => {
                let mut cmd = Command::new("/usr/bin/osascript");
                cmd.args(["-l", "JavaScript", "-e", MAC_DIALOG]);
                cmd
            }
            _ => return Err(error()),
        };
        let mut child = cmd
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| error())?;
        // Escape controls and directional overrides in imported member names.
        let name: String = member
            .username
            .chars()
            .flat_map(char::escape_debug)
            .collect();
        let input = json!({"event":event,"member_id":member.id,"member_name":name,"value":value});
        let mut stdin = child.stdin.take().ok_or_else(error)?;
        stdin
            .write_all(input.to_string().as_bytes())
            .await
            .map_err(|_| error())?;
        drop(stdin);
        match child.wait().await.map_err(|_| error())?.code() {
            Some(0) => Ok(true),
            Some(2) => Ok(false),
            _ => Err(error()),
        }
    }
}

// Keep the QR in the local process's stdin and Core Image memory. Closing or
// dropping this process (token rotation, cancellation, timeout) removes its UI.
const MAC_DIALOG: &str = r#"
ObjC.import('AppKit'); ObjC.import('CoreGraphics'); ObjC.import('CoreImage'); ObjC.import('stdlib');
const raw = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
const p = JSON.parse(ObjC.unwrap($.NSString.alloc.initWithDataEncoding(raw, $.NSUTF8StringEncoding)));
// Also expire the native window if the hosting server exits abruptly.
ObjC.bindFunction('alarm', ['unsigned int', ['unsigned int']]);
$.alarm(p.event === 'scan' ? 45 : 120);
const member = JSON.stringify(p.member_name) + ' (#' + p.member_id + ')';
const app = $.NSApplication.sharedApplication;
app.setActivationPolicy($.NSApplicationActivationPolicyAccessory);
const alert = $.NSAlert.alloc.init;
alert.messageText = 'PolyView · Telegram';
let message;
if (p.event === 'scan') {
    const qr = $.CIFilter.filterWithName('CIQRCodeGenerator');
    qr.setValueForKey($(p.value).dataUsingEncoding($.NSUTF8StringEncoding), 'inputMessage');
    const image = qr.outputImage;
    const padded = 280;
    const view = $.NSImageView.alloc.initWithFrame($.NSMakeRect(0, 0, padded, padded));
    const scale = Math.floor(248 / image.extent.size.width);
    const rep = $.NSCIImageRep.imageRepWithCIImage(image.imageByApplyingTransform($.CGAffineTransformMakeScale(scale, scale)));
    const nsImage = $.NSImage.alloc.initWithSize(rep.size);
    nsImage.addRepresentation(rep);
    view.image = nsImage;
    view.wantsLayer = true;
    view.layer.backgroundColor = $.NSColor.whiteColor.CGColor;
    alert.accessoryView = view;
    message = '请仅用你自己的 Telegram 官方应用扫描此码，将账户绑定至 PolyView 成员 ' + member + '。';
} else if (p.event === 'bind') {
    message = '确认将 Telegram 账户 #' + p.value + ' 绑定至 PolyView 成员 ' + member + '？';
} else if (p.event === 'confirm') {
    message = 'PolyView 成员 ' + member + ' 请求使用你的 Telegram 账户。确认此人身份并允许显示本机二维码？';
} else { $.exit(1); }
alert.informativeText = message;
alert.addButtonWithTitle('取消');
if (p.event !== 'scan') alert.addButtonWithTitle('确认');
app.activateIgnoringOtherApps(true);
$.exit(alert.runModal === 1001 ? 0 : 2);
"#;

struct Attempt<'a> {
    state: &'a Shared,
    member: &'a AuthUser,
    key: String,
}

impl Attempt<'_> {
    // Require the same active member, attempt and deadline at every write,
    // including the final verified binding after the host dialog returns.
    fn update(&self, phase: &str, identity: Option<i64>) -> Result<bool, AppError> {
        Ok(self.state.conn()?.execute(
            "UPDATE native_telegram_accounts SET phase=?,telegram_id=?
            WHERE user_id=? AND attempt=? AND phase IN ('confirm','scan') AND expires_at>?
            AND EXISTS(SELECT 1 FROM users WHERE id=? AND username=? AND disabled=0)",
            params![
                phase,
                identity,
                self.member.id,
                self.key,
                crate::now(),
                self.member.id,
                self.member.username
            ],
        )? == 1)
    }

    fn current(&self) -> bool {
        let result = || -> Result<bool, AppError> {
            Ok(self.state.conn()?.query_row("SELECT EXISTS(SELECT 1 FROM native_telegram_accounts a JOIN users u ON u.id=a.user_id
                WHERE a.user_id=? AND a.attempt=? AND a.phase IN ('confirm','scan') AND a.expires_at>? AND u.username=? AND u.disabled=0)",
                params![self.member.id,self.key,crate::now(),self.member.username], |r| r.get(0))?)
        };
        result().unwrap_or(false)
    }

    async fn until_stale(&self) {
        while self.current() {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}

async fn run(state: Shared, member: AuthUser, account: NativeAccount, prompt: HostPrompt) {
    let id = account.attempt;
    let attempt = Attempt {
        state: &state,
        member: &member,
        key: id.to_string(),
    };
    let result = tokio::select! {
        biased;
        _ = attempt.until_stale() => Err("expired"),
        _ = tokio::time::sleep(Duration::from_secs(LIFETIME as u64)) => Err("expired"),
        result = authorize(&attempt, account, &prompt) => result,
    };
    match result {
        Ok(identity)
            if attempt
                .update("authorized", Some(identity))
                .unwrap_or(false) =>
        {
            return;
        }
        Err(phase) => {
            let _ = attempt.update(phase, None);
        }
        _ => (),
    }
    state.telegram_native.abandon(member.id, id).await;
}

async fn authorize(
    attempt: &Attempt<'_>,
    account: NativeAccount,
    prompt: &HostPrompt,
) -> Result<i64, &'static str> {
    let member = attempt.member;
    if !prompt
        .dialog(member, "confirm", Value::Null)
        .await
        .map_err(|_| "error")?
    {
        return Err("declined");
    }
    if !attempt.current() {
        return Err("expired");
    }
    let client = attempt
        .state
        .telegram_native
        .client(account)
        .await
        .map_err(|_| "error")?;
    if !attempt.current() {
        return Err("expired");
    }
    let mut auth = client.auth();
    let mut asked = false;
    let mut presented = false;
    loop {
        let current = auth.borrow_and_update().clone();
        match current {
            AuthState::Phone if !asked => {
                client.request_qr().await.map_err(|_| "error")?;
                asked = true;
            }
            AuthState::Starting | AuthState::Phone => (),
            AuthState::Qr(link) if asked => {
                if !attempt.update("scan", None).map_err(|_| "error")? {
                    return Err("expired");
                }
                presented = true;
                tokio::select! {
                    result = prompt.dialog(member,"scan",Value::String(link)) => return Err(if result.is_ok() { "declined" } else { "error" }),
                    result = auth.changed() => result.map_err(|_| "error")?,
                    _ = tokio::time::sleep(Duration::from_secs(45)) => return Err("expired"),
                }
                continue;
            }
            AuthState::Ready if presented => {
                let me = client
                    .request(json!({"@type":"getMe"}))
                    .await
                    .map_err(|_| "error")?;
                let id = me["id"]
                    .as_i64()
                    .filter(|id| *id > 0 && me["@type"] == "user")
                    .ok_or("error")?;
                if !attempt.current() {
                    return Err("expired");
                }
                if !prompt
                    .dialog(member, "bind", json!(id))
                    .await
                    .map_err(|_| "error")?
                {
                    return Err("declined");
                }
                if *auth.borrow() != AuthState::Ready {
                    return Err("error");
                }
                return Ok(id);
            }
            AuthState::Code | AuthState::Password | AuthState::Unsupported => {
                return Err("official");
            }
            _ => return Err("error"),
        }
        auth.changed().await.map_err(|_| "error")?;
    }
}
