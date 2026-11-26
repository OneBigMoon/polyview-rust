//! Account-scoped adapter for the official TDLib 1.x `td_json_client_*` ABI.
//! Each instance and its receive loop live on one thread. The web layer never
//! selects a library, a database path, or an arbitrary TDLib method.
use crate::{AppError, AppState, Shared};
use axum::http::StatusCode;
use libloading::Library;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_char, c_void},
    fs, io,
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);
const MAX_PENDING: usize = 32;
const MAX_CLIENTS: usize = 64;
const ACTIVE: u8 = 0;
const CLOSE: u8 = 1;
const ABANDON: u8 = 2;

fn unavailable() -> AppError {
    AppError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Telegram client unavailable".into(),
    )
}

fn denied() -> AppError {
    AppError(
        StatusCode::FORBIDDEN,
        "Telegram account is not authorized".into(),
    )
}

fn expired() -> AppError {
    AppError(
        StatusCode::GATEWAY_TIMEOUT,
        "Telegram request timed out".into(),
    )
}

fn busy() -> AppError {
    AppError(
        StatusCode::TOO_MANY_REQUESTS,
        "Telegram client is busy".into(),
    )
}

fn provider_error(response: &Value) -> AppError {
    // Telegram error messages can contain local paths, account data and secrets.
    match response["code"].as_i64() {
        Some(400) => AppError::bad_request("Telegram rejected the request"),
        Some(401 | 403) => denied(),
        Some(404) => AppError::not_found("Telegram resource not found"),
        Some(429) => busy(),
        _ => AppError(StatusCode::BAD_GATEWAY, "Telegram request failed".into()),
    }
}

#[derive(Clone)]
pub(crate) struct NativeAccount {
    pub user_id: i64,
    pub account_key: Uuid,
    pub attempt: Uuid,
    pub api_id: i32,
    pub api_hash: String,
}

// Deliberately neither serializable nor Debug: Qr holds host-only login data.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum AuthState {
    Starting,
    Phone,
    Code,
    Qr(String),
    Password,
    Ready,
    Closed,
    Unsupported,
    Failed,
}

fn private_child(parent: &Path, child: &str) -> io::Result<PathBuf> {
    let path = parent.join(child);
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir() {
        return Err(io::Error::other("Invalid Telegram directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("Telegram directory is not private"));
        }
    }
    Ok(path)
}

fn session_dir(data_dir: &Path, account: &NativeAccount) -> Result<PathBuf, AppError> {
    fs::create_dir_all(data_dir).map_err(|_| unavailable())?;
    let root = fs::canonicalize(data_dir).map_err(|_| unavailable())?;
    let path = private_child(&root, "telegram")
        .and_then(|p| private_child(&p, &account.account_key.simple().to_string()))
        .and_then(|p| private_child(&p, &account.attempt.simple().to_string()))
        .map_err(|_| unavailable())?;
    if path.to_str().is_none() {
        return Err(unavailable());
    }
    Ok(path)
}

type Create = unsafe extern "C" fn() -> *mut c_void;
type SendJson = unsafe extern "C" fn(*mut c_void, *const c_char);
type Receive = unsafe extern "C" fn(*mut c_void, f64) -> *const c_char;
type Execute = unsafe extern "C" fn(*mut c_void, *const c_char) -> *const c_char;
type Destroy = unsafe extern "C" fn(*mut c_void);

struct TdLibrary {
    create: Create,
    send: SendJson,
    receive: Receive,
    destroy: Destroy,
    _library: Library,
}

impl TdLibrary {
    fn from_host() -> Result<Self, AppError> {
        let path = std::env::var_os("POLYVIEW_TDJSON_PATH")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .ok_or_else(unavailable)?;
        // SAFETY: the host operator supplies an installed official library, a
        // trusted executable dependency. Signatures match td_json_client.h.
        unsafe {
            let library = Library::new(path).map_err(|_| unavailable())?;
            let execute: Execute = *library
                .get(b"td_json_client_execute\0")
                .map_err(|_| unavailable())?;
            let reply = execute(
                std::ptr::null_mut(),
                c"{\"@type\":\"setLogStream\",\"log_stream\":{\"@type\":\"logStreamEmpty\"}}"
                    .as_ptr(),
            );
            if reply.is_null()
                || serde_json::from_slice::<Value>(CStr::from_ptr(reply).to_bytes())
                    .ok()
                    .is_none_or(|r| r["@type"] != "ok")
            {
                return Err(unavailable());
            }
            Ok(Self {
                create: *library
                    .get(b"td_json_client_create\0")
                    .map_err(|_| unavailable())?,
                send: *library
                    .get(b"td_json_client_send\0")
                    .map_err(|_| unavailable())?,
                receive: *library
                    .get(b"td_json_client_receive\0")
                    .map_err(|_| unavailable())?,
                destroy: *library
                    .get(b"td_json_client_destroy\0")
                    .map_err(|_| unavailable())?,
                _library: library,
            })
        }
    }
}

trait Transport {
    fn send(&mut self, request: Value) -> Result<(), AppError>;
    fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, AppError>;
}

struct HostClient {
    library: Arc<TdLibrary>,
    handle: NonNull<c_void>,
}

impl Transport for HostClient {
    fn send(&mut self, request: Value) -> Result<(), AppError> {
        let bytes = serde_json::to_vec(&request).map_err(|_| unavailable())?;
        let request = CString::new(bytes).map_err(|_| unavailable())?;
        // SAFETY: this thread owns a live handle and the request outlives send.
        unsafe { (self.library.send)(self.handle.as_ptr(), request.as_ptr()) };
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, AppError> {
        // SAFETY: only this thread receives on this handle. TDLib owns the
        // returned NUL-terminated string until its next receive/execute here.
        unsafe {
            let ptr = (self.library.receive)(self.handle.as_ptr(), timeout.as_secs_f64());
            if ptr.is_null() {
                Ok(None)
            } else {
                serde_json::from_slice(CStr::from_ptr(ptr).to_bytes())
                    .map(Some)
                    .map_err(|_| unavailable())
            }
        }
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        // SAFETY: unique handle destroyed once on its owning thread; all
        // function pointers remain valid until the Arc's last reference drops.
        unsafe { (self.library.destroy)(self.handle.as_ptr()) };
    }
}

type Script = dyn Fn(i64, Value) -> Vec<Value> + Send + Sync;

struct LocalTransport {
    user_id: i64,
    script: Arc<Script>,
    updates: VecDeque<Value>,
    injected: std::sync::mpsc::Receiver<Value>,
}

impl Transport for LocalTransport {
    fn send(&mut self, request: Value) -> Result<(), AppError> {
        let extra = request["@extra"].clone();
        let shutdown = matches!(
            request["@type"].as_str(),
            Some("close" | "logOut" | "destroy")
        );
        let responses = (self.script)(self.user_id, request);
        let auto_close = shutdown
            && !responses.iter().any(|r| {
                r["@type"] == "error"
                    || r["authorization_state"]["@type"] == "authorizationStateClosed"
            });
        for mut value in responses {
            if !value["@type"]
                .as_str()
                .unwrap_or_default()
                .starts_with("update")
                && value.get("@extra").is_none()
            {
                value["@extra"] = extra.clone();
            }
            self.updates.push_back(value);
        }
        if auto_close {
            self.updates.push_back(json!({"@type":"updateAuthorizationState", "authorization_state":{"@type":"authorizationStateClosed"}}));
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, AppError> {
        Ok(self
            .updates
            .pop_front()
            .or_else(|| self.injected.recv_timeout(timeout).ok()))
    }
}

#[derive(Clone)]
enum Factory {
    Host,
    Local(Arc<Script>),
}

impl Factory {
    fn open(
        &self,
        user_id: i64,
        injected: Option<std::sync::mpsc::Receiver<Value>>,
    ) -> Result<Box<dyn Transport>, AppError> {
        match self {
            Self::Host => {
                // TDLib owns process-wide logging and scheduler state. Keep its
                // code loaded even when individual runtimes release all clients.
                static TDJSON: LazyLock<Option<Arc<TdLibrary>>> =
                    LazyLock::new(|| TdLibrary::from_host().ok().map(Arc::new));
                let library = TDJSON.as_ref().cloned().ok_or_else(unavailable)?;
                // SAFETY: a fresh client is owned by this thread until Drop.
                let handle = NonNull::new(unsafe { (library.create)() }).ok_or_else(unavailable)?;
                Ok(Box::new(HostClient { library, handle }))
            }
            Self::Local(script) => Ok(Box::new(LocalTransport {
                user_id,
                script: script.clone(),
                updates: VecDeque::new(),
                injected: injected.ok_or_else(unavailable)?,
            })),
        }
    }
}

struct Request {
    value: Value,
    reply: oneshot::Sender<Result<Value, AppError>>,
}

type FileLocks = Arc<[LazyLock<Arc<tokio::sync::Mutex<()>>>; 16]>;

#[derive(Clone)]
pub(super) struct NativeClient {
    user_id: i64,
    account_key: Uuid,
    attempt: Uuid,
    verifier: Option<Weak<AppState>>,
    requests: mpsc::Sender<Request>,
    auth: watch::Receiver<AuthState>,
    stop: Arc<AtomicU8>,
    file_locks: FileLocks,
}

impl NativeClient {
    pub(super) fn auth(&self) -> watch::Receiver<AuthState> {
        self.auth.clone()
    }

    pub(super) fn file_lock(&self, file_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.file_locks[file_id.rem_euclid(16) as usize].clone()
    }

    fn check_binding(&self) -> Result<(), AppError> {
        if *self.auth.borrow() != AuthState::Ready || self.stop.load(Ordering::Acquire) != ACTIVE {
            return Err(denied());
        }
        if let Some(shared) = &self.verifier {
            let shared = shared.upgrade().ok_or_else(denied)?;
            let account = super::login::verified_account(&*shared.conn()?, self.user_id)?;
            if !account
                .is_some_and(|a| a.account_key == self.account_key && a.attempt == self.attempt)
            {
                self.stop.fetch_max(CLOSE, Ordering::AcqRel);
                return Err(denied());
            }
        }
        Ok(())
    }

    pub(super) async fn request(&self, value: Value) -> Result<Value, AppError> {
        match value["@type"].as_str() {
            Some(
                "getMe"
                | "getChats"
                | "loadChats"
                | "getChat"
                | "getChatMember"
                | "getSupergroup"
                | "getBasicGroup"
                | "searchPublicChat"
                | "getMessage"
                | "getChatHistory"
                | "searchChatMessages"
                | "getFile"
                | "getFileDownloadedPrefixSize"
                | "downloadFile"
                | "readFilePart"
                | "cancelDownloadFile",
            ) => (),
            _ => return Err(AppError::bad_request("Unsupported Telegram request")),
        }
        self.check_binding()?;
        let result = self.call(value, RPC_TIMEOUT).await?;
        self.check_binding()?;
        Ok(result)
    }

    pub(super) async fn request_qr(&self) -> Result<(), AppError> {
        if self.verifier.is_some() || *self.auth.borrow() != AuthState::Phone {
            return Err(denied());
        }
        self.call(
            json!({"@type":"requestQrCodeAuthentication", "other_user_ids":[]}),
            RPC_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    #[cfg(unix)]
    pub(super) async fn submit_factor(
        &self,
        stage: &AuthState,
        input: String,
    ) -> Result<(), AppError> {
        if self.verifier.is_some() || *self.auth.borrow() != *stage {
            return Err(denied());
        }
        let (method, field) = match stage {
            AuthState::Phone => ("setAuthenticationPhoneNumber", "phone_number"),
            AuthState::Code => ("checkAuthenticationCode", "code"),
            AuthState::Password => ("checkAuthenticationPassword", "password"),
            _ => return Err(denied()),
        };
        let mut request = json!({"@type":method});
        request[field] = Value::String(input);
        let reply = self.call(request, RPC_TIMEOUT).await?;
        if reply["@type"] != "ok" {
            return Err(unavailable());
        }
        Ok(())
    }

    async fn call(&self, value: Value, timeout: Duration) -> Result<Value, AppError> {
        if self.stop.load(Ordering::Acquire) != ACTIVE {
            return Err(unavailable());
        }
        let (reply, rx) = oneshot::channel();
        self.requests
            .try_send(Request { value, reply })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Closed(_) => unavailable(),
                mpsc::error::TrySendError::Full(_) => busy(),
            })?;
        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| expired())?
            .map_err(|_| unavailable())?
    }
}

struct Entry {
    client: NativeClient,
    done: watch::Receiver<bool>,
    injection: Option<std::sync::mpsc::SyncSender<Value>>,
    worker: thread::JoinHandle<()>,
}

pub struct NativeRuntime {
    factory: Factory,
    data_dir: PathBuf,
    entries: Mutex<HashMap<(i64, Uuid), Entry>>,
}

impl Default for NativeRuntime {
    fn default() -> Self {
        Self {
            factory: Factory::Host,
            data_dir: std::env::var_os("POLYVIEW_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "data".into()),
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl NativeRuntime {
    /// Embed an in-process transport. No URL, socket, library or executable is
    /// involved. The callback receives the server-selected member and request.
    #[doc(hidden)]
    pub fn with_transport(
        root: PathBuf,
        f: impl Fn(i64, Value) -> Vec<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Factory::Local(Arc::new(f)),
            data_dir: root,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Deliver an asynchronous event to one in-process test transport. Host
    /// TDLib handles only receive messages from their own native receive loop.
    #[doc(hidden)]
    pub fn inject_update(
        &self,
        user_id: i64,
        attempt: Uuid,
        update: Value,
    ) -> Result<(), AppError> {
        let entries = self.entries.lock().map_err(|_| unavailable())?;
        entries
            .get(&(user_id, attempt))
            .and_then(|entry| entry.injection.as_ref())
            .ok_or_else(unavailable)?
            .try_send(update)
            .map_err(|_| unavailable())
    }

    pub(super) async fn client(&self, account: NativeAccount) -> Result<NativeClient, AppError> {
        let mut client = {
            let mut entries = self.entries.lock().map_err(|_| unavailable())?;
            entries.retain(|_, e| !e.worker.is_finished());
            let key = (account.user_id, account.attempt);
            if let Some(entry) = entries.get(&key) {
                if entry.client.account_key != account.account_key
                    || entry.client.stop.load(Ordering::Acquire) != ACTIVE
                {
                    return Err(unavailable());
                }
                entry.client.clone()
            } else {
                if entries.len() >= MAX_CLIENTS {
                    return Err(busy());
                }
                let (requests, receiver) = mpsc::channel(MAX_PENDING);
                let (auth_tx, auth) = watch::channel(AuthState::Starting);
                let (done_tx, done) = watch::channel(false);
                let stop = Arc::new(AtomicU8::new(ACTIVE));
                let client = NativeClient {
                    user_id: account.user_id,
                    account_key: account.account_key,
                    attempt: account.attempt,
                    verifier: None,
                    requests,
                    auth,
                    stop: stop.clone(),
                    file_locks: Arc::new(std::array::from_fn(|_| {
                        LazyLock::new(Default::default as fn() -> _)
                    })),
                };
                let factory = self.factory.clone();
                let root = self.data_dir.clone();
                let (injection, injected) = match factory {
                    Factory::Host => (None, None),
                    Factory::Local(_) => {
                        let (sender, receiver) = std::sync::mpsc::sync_channel(MAX_PENDING);
                        (Some(sender), Some(receiver))
                    }
                };
                let worker = thread::Builder::new()
                    .name("telegram-tdjson".into())
                    .spawn(move || {
                        let result =
                            factory
                                .open(account.user_id, injected)
                                .and_then(|transport| {
                                    run_client(transport, root, account, receiver, &auth_tx, &stop)
                                });
                        if result.is_err() {
                            auth_tx.send_replace(AuthState::Failed);
                        } else if *auth_tx.borrow() != AuthState::Failed {
                            auth_tx.send_replace(AuthState::Closed);
                        }
                        done_tx.send_replace(true);
                    })
                    .map_err(|_| unavailable())?;
                entries.insert(
                    key,
                    Entry {
                        client: client.clone(),
                        worker,
                        done,
                        injection,
                    },
                );
                client
            }
        };
        drop(
            tokio::time::timeout(
                RPC_TIMEOUT,
                client.auth.wait_for(|s| *s != AuthState::Starting),
            )
            .await
            .map_err(|_| expired())?
            .map_err(|_| unavailable())?,
        );
        if matches!(*client.auth.borrow(), AuthState::Closed | AuthState::Failed) {
            return Err(unavailable());
        }
        Ok(client)
    }

    pub(super) async fn close(&self, user_id: i64, attempt: Uuid) {
        self.stop(user_id, attempt, CLOSE).await;
    }

    pub(super) async fn abandon(&self, user_id: i64, attempt: Uuid) {
        self.stop(user_id, attempt, ABANDON).await;
    }

    // The terminal owns its runtime. Wait for TDLib to release this exact
    // database before publishing the binding to the independent HTTP process.
    #[cfg(unix)]
    pub(super) async fn release_session(&self, account: &NativeAccount) -> Result<(), AppError> {
        self.close(account.user_id, account.attempt).await;
        let entry = self.take_finished(account)?.ok_or_else(unavailable)?;
        let clean = *entry.client.auth.borrow() == AuthState::Closed;
        entry.worker.join().map_err(|_| unavailable())?;
        if clean { Ok(()) } else { Err(unavailable()) }
    }

    #[cfg(unix)]
    fn take_finished(&self, account: &NativeAccount) -> Result<Option<Entry>, AppError> {
        let mut entries = self.entries.lock().map_err(|_| unavailable())?;
        let key = (account.user_id, account.attempt);
        if entries.get(&key).is_some_and(|entry| !*entry.done.borrow()) {
            return Err(unavailable());
        }
        Ok(entries.remove(&key))
    }

    #[cfg(unix)]
    pub(super) async fn discard_session(&self, account: &NativeAccount) {
        self.abandon(account.user_id, account.attempt).await;
        if let Ok(Some(entry)) = self.take_finished(account) {
            let _ = entry.worker.join();
            // Transport failures may exit before run_client can perform its
            // normal logout cleanup. Remove only this stopped enrollment's DB.
            if *entry.client.auth.borrow() == AuthState::Failed
                && let Ok(dir) = session_dir(&self.data_dir, account)
            {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }

    async fn stop(&self, user_id: i64, attempt: Uuid, mode: u8) {
        let done = self.entries.lock().ok().and_then(|entries| {
            let entry = entries.get(&(user_id, attempt))?;
            entry.client.stop.fetch_max(mode, Ordering::AcqRel);
            Some(entry.done.clone())
        });
        if let Some(mut done) = done {
            let _ = tokio::time::timeout(2 * CLOSE_TIMEOUT + POLL * 4, done.wait_for(|d| *d)).await;
        }
    }
}

impl Drop for NativeRuntime {
    fn drop(&mut self) {
        let entries = self.entries.get_mut().unwrap_or_else(|e| e.into_inner());
        for entry in entries.values() {
            entry.client.stop.fetch_max(CLOSE, Ordering::AcqRel);
        }
        // All clients close concurrently. Keep the process alive long enough
        // for TDLib to flush its databases, even when the Tokio runtime exits.
        let deadline = Instant::now() + CLOSE_TIMEOUT * 2 + POLL * 4;
        while entries.values().any(|entry| !entry.worker.is_finished()) && Instant::now() < deadline
        {
            thread::sleep(POLL);
        }
        for (_, entry) in entries.drain() {
            if entry.worker.is_finished() {
                let _ = entry.worker.join();
            }
        }
    }
}

pub(super) async fn authorized(
    state: &Shared,
    user_id: i64,
) -> Result<Option<NativeClient>, AppError> {
    let Some(account) = super::login::verified_account(&*state.conn()?, user_id)? else {
        return Ok(None);
    };
    let mut client = state.telegram_native.client(account).await?;
    if *client.auth.borrow() != AuthState::Ready {
        state.telegram_native.close(user_id, client.attempt).await;
        return Err(denied());
    }
    client.verifier = Some(Arc::downgrade(state));
    client.check_binding()?;
    Ok(Some(client))
}

fn parameters(dir: &Path, account: &NativeAccount) -> Value {
    json!({
        "@type":"setTdlibParameters", "@extra":"parameters", "use_test_dc":false,
        "database_directory":dir, "files_directory":dir, "database_encryption_key":"",
        "use_file_database":true, "use_chat_info_database":true, "use_message_database":true,
        "use_secret_chats":false, "api_id":account.api_id, "api_hash":account.api_hash,
        "system_language_code":"en", "device_model":"PolyView", "system_version":std::env::consts::OS,
        "application_version":env!("CARGO_PKG_VERSION")
    })
}

fn auth_state(state: &Value) -> AuthState {
    match state["@type"].as_str() {
        Some("authorizationStateWaitTdlibParameters") => AuthState::Starting,
        Some("authorizationStateWaitPhoneNumber") => AuthState::Phone,
        Some("authorizationStateWaitCode") => AuthState::Code,
        Some("authorizationStateWaitOtherDeviceConfirmation") => {
            let link = state["link"].as_str().unwrap_or_default();
            if link.strip_prefix("tg://login?token=").is_some_and(|token| {
                !token.is_empty()
                    && token.len() <= 2048
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-=".contains(&b))
            }) {
                AuthState::Qr(link.into())
            } else {
                AuthState::Failed
            }
        }
        Some("authorizationStateWaitPassword") => AuthState::Password,
        Some("authorizationStateReady") => AuthState::Ready,
        Some(
            "authorizationStateClosed"
            | "authorizationStateLoggingOut"
            | "authorizationStateClosing",
        ) => AuthState::Closed,
        _ => AuthState::Unsupported,
    }
}

fn run_client(
    mut transport: Box<dyn Transport>,
    root: PathBuf,
    account: NativeAccount,
    mut requests: mpsc::Receiver<Request>,
    auth: &watch::Sender<AuthState>,
    stop: &AtomicU8,
) -> Result<(), AppError> {
    let dir = session_dir(&root, &account)?;
    let mut init = Some(parameters(&dir, &account));
    drop(account);
    transport.send(json!({"@type":"getAuthorizationState","@extra":"initial"}))?;
    let mut pending = HashMap::<u64, oneshot::Sender<Result<Value, AppError>>>::new();
    let mut next_id = 1_u64;
    let mut saw_update = false;
    let mut closing = ACTIVE;
    let mut close_deadline = Instant::now();
    let mut destroyed = false;
    let mut last_use = Instant::now();
    loop {
        if (*auth.borrow() == AuthState::Starting && last_use.elapsed() >= RPC_TIMEOUT)
            || last_use.elapsed() >= Duration::from_secs(15 * 60)
        {
            stop.fetch_max(CLOSE, Ordering::AcqRel);
        }
        let mode = stop.load(Ordering::Acquire);
        if mode > closing {
            closing = mode;
            pending.clear();
            requests.close();
            // An accepted scan can race cancellation. Try a proper logout
            // even before observing Ready. On failure, destroy local state.
            let method = if mode == ABANDON { "logOut" } else { "close" };
            transport.send(json!({"@type":method,"@extra":"shutdown"}))?;
            close_deadline = Instant::now() + CLOSE_TIMEOUT;
        }
        if closing != ACTIVE && Instant::now() >= close_deadline {
            if closing == CLOSE {
                return Err(expired());
            }
            if destroyed {
                break;
            }
            transport.send(json!({"@type":"destroy","@extra":"shutdown"}))?;
            destroyed = true;
            close_deadline = Instant::now() + CLOSE_TIMEOUT;
        }
        if closing == ACTIVE {
            pending.retain(|_, reply| !reply.is_closed());
            for _ in pending.len()..MAX_PENDING {
                let Ok(Request { mut value, reply }) = requests.try_recv() else {
                    break;
                };
                if reply.is_closed() {
                    continue;
                }
                value["@extra"] = next_id.into();
                transport.send(value)?;
                pending.insert(next_id, reply);
                next_id += 1;
                last_use = Instant::now();
            }
        }
        let Some(response) = transport.receive(POLL)? else {
            continue;
        };
        let kind = response["@type"].as_str();
        let initial = response["@extra"] == "initial";
        if kind == Some("updateAuthorizationState") || (initial && !saw_update) {
            let state = if initial {
                &response
            } else {
                saw_update = true;
                &response["authorization_state"]
            };
            if state["@type"] == "authorizationStateClosed" {
                break;
            }
            if closing != ACTIVE {
                continue;
            }
            let current = if state["@type"] == "error" {
                AuthState::Failed
            } else {
                auth_state(state)
            };
            if current == AuthState::Starting {
                if let Some(params) = init.take() {
                    transport.send(params)?;
                }
            } else if current == AuthState::Failed {
                stop.fetch_max(CLOSE, Ordering::AcqRel);
            }
            auth.send_replace(current);
        } else if response["@extra"] == "parameters" && kind == Some("error") {
            auth.send_replace(AuthState::Failed);
            stop.fetch_max(CLOSE, Ordering::AcqRel);
        } else if response["@extra"] == "shutdown" && kind == Some("error") {
            close_deadline = Instant::now();
        } else if let Some(reply) = response["@extra"]
            .as_u64()
            .and_then(|id| pending.remove(&id))
        {
            let _ = reply.send(if kind == Some("error") {
                Err(provider_error(&response))
            } else {
                Ok(response)
            });
        }
    }
    drop(transport);
    if closing == ABANDON {
        // The path was constructed exclusively from host configuration and
        // immutable UUIDs. Remove only this abandoned attempt after TDLib exits.
        match fs::remove_dir_all(dir) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(unavailable()),
            _ => (),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn account(user_id: i64) -> NativeAccount {
        NativeAccount {
            user_id,
            account_key: Uuid::new_v4(),
            attempt: Uuid::new_v4(),
            api_id: 12345,
            api_hash: "synthetic-local-test".into(),
        }
    }

    fn update(kind: &str) -> Value {
        json!({"@type":"updateAuthorizationState", "authorization_state":{"@type":kind}})
    }

    #[tokio::test]
    async fn ordered_updates_override_startup_snapshot_and_parameters_are_sent_once() {
        let temp = tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = NativeRuntime::with_transport(temp.path().into(), move |_, req| {
            let kind = req["@type"].as_str().unwrap().to_owned();
            tx.send(req).unwrap();
            match kind.as_str() {
                "getAuthorizationState" => vec![
                    update("authorizationStateWaitTdlibParameters"),
                    json!({"@type":"authorizationStateReady"}),
                    update("authorizationStateWaitTdlibParameters"),
                ],
                "setTdlibParameters" => vec![
                    json!({"@type":"ok"}),
                    update("authorizationStateWaitPhoneNumber"),
                ],
                "requestQrCodeAuthentication" => vec![
                    json!({"@type":"ok"}),
                    json!({"@type":"updateAuthorizationState", "authorization_state":{"@type":"authorizationStateWaitOtherDeviceConfirmation","link":"tg://login?token=LOCAL_only"}}),
                ],
                _ => vec![json!({"@type":"ok"})],
            }
        });
        let member = account(7);
        let dir = session_dir(temp.path(), &member).unwrap();
        let client = runtime.client(member.clone()).await.unwrap();
        assert!(*client.auth.borrow() == AuthState::Phone);
        assert_eq!(rx.recv().await.unwrap()["@type"], "getAuthorizationState");
        let params = rx.recv().await.unwrap();
        assert_eq!(params["@type"], "setTdlibParameters");
        assert_eq!(params["database_directory"], dir.to_str().unwrap());
        assert_eq!(params["use_test_dc"], false);
        assert_eq!(params["api_id"], 12345);
        let rejected = client.request(json!({"@type":"getMe"})).await.unwrap_err();
        assert_eq!(rejected.0, StatusCode::FORBIDDEN);
        client.request_qr().await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap()["@type"],
            "requestQrCodeAuthentication"
        );
        let mut states = client.auth();
        drop(
            states
                .wait_for(
                    |s| matches!(s, AuthState::Qr(link) if link == "tg://login?token=LOCAL_only"),
                )
                .await
                .unwrap(),
        );
        runtime.abandon(7, member.attempt).await;
        assert!(!dir.exists());
        assert_eq!(rx.recv().await.unwrap()["@type"], "logOut");
        assert!(*states.borrow() == AuthState::Closed);
    }

    #[tokio::test]
    async fn replies_stay_with_the_request_and_member_after_cancellation() {
        let temp = tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = NativeRuntime::with_transport(temp.path().into(), move |id, req| {
            match req["@type"].as_str().unwrap() {
                "getAuthorizationState" => vec![json!({"@type":"authorizationStateReady"})],
                "getMessage" => {
                    tx.send((id, req)).unwrap();
                    vec![]
                }
                _ => vec![json!({"@type":"ok"})],
            }
        });
        let one = account(1);
        let two = account(2);
        let a = runtime.client(one.clone()).await.unwrap();
        let b = runtime.client(two.clone()).await.unwrap();
        let ask = |client: NativeClient| {
            tokio::spawn(async move { client.request(json!({"@type":"getMessage"})).await })
        };
        let canceled = ask(a.clone());
        let (_, canceled_req) = rx.recv().await.unwrap();
        canceled.abort();
        assert!(canceled.await.unwrap_err().is_cancelled());
        let first = ask(a.clone());
        let (id, first_req) = rx.recv().await.unwrap();
        assert_eq!(id, 1);
        let second = ask(b.clone());
        let (id, second_req) = rx.recv().await.unwrap();
        assert_eq!(id, 2);
        let response = |req: Value, text: &str| json!({"@extra":req["@extra"], "@type":"message", "text":text});
        runtime
            .inject_update(1, one.attempt, response(canceled_req, "late"))
            .unwrap();
        assert!(runtime.inject_update(1, two.attempt, json!({})).is_err());
        runtime
            .inject_update(2, two.attempt, response(second_req, "two"))
            .unwrap();
        assert_eq!(second.await.unwrap().unwrap()["text"], "two");
        runtime
            .inject_update(1, one.attempt, response(first_req, "one"))
            .unwrap();
        assert_eq!(first.await.unwrap().unwrap()["text"], "one");
        runtime.close(1, one.attempt).await;
        assert!(a.request(json!({"@type":"getMe"})).await.is_err());
        assert_eq!(
            b.request(json!({"@type":"getMe"})).await.unwrap()["@type"],
            "ok"
        );
    }

    #[tokio::test]
    async fn masked_failures_and_logout_fallback_preserve_other_sessions() {
        let temp = tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = NativeRuntime::with_transport(temp.path().into(), move |_, req| {
            match req["@type"].as_str().unwrap() {
                "getAuthorizationState" => vec![json!({"@type":"authorizationStateReady"})],
                "getMe" | "logOut" => vec![
                    json!({"@type":"error","code":500,"message":"/private/fake/hypothetical-secret"}),
                ],
                "destroy" => {
                    tx.send(()).unwrap();
                    vec![]
                }
                _ => vec![],
            }
        });
        let one = account(1);
        let two = account(2);
        let a = runtime.client(one.clone()).await.unwrap();
        let b = runtime.client(two.clone()).await.unwrap();
        let dir_a = session_dir(temp.path(), &one).unwrap();
        let dir_b = session_dir(temp.path(), &two).unwrap();
        assert_ne!(dir_a, dir_b);
        let error = a.request(json!({"@type":"getMe"})).await.unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert_eq!(error.1, "Telegram request failed");
        let error = b
            .request(json!({"@type":"setTdlibParameters","database_directory":"/untrusted"}))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        runtime.abandon(1, one.attempt).await;
        rx.try_recv().unwrap();
        assert!(!dir_a.exists());
        assert!(dir_b.is_dir());
    }

    #[tokio::test]
    async fn unanswered_rpc_expires_and_client_can_still_serve() {
        let temp = tempdir().unwrap();
        let runtime =
            NativeRuntime::with_transport(temp.path().into(), |_, req| {
                match req["@type"].as_str() {
                    Some("getAuthorizationState") => {
                        vec![json!({"@type":"authorizationStateReady"})]
                    }
                    Some("getMe") => vec![json!({"@type":"user","id":42})],
                    _ => vec![],
                }
            });
        let client = runtime.client(account(1)).await.unwrap();
        let error = client
            .call(json!({"@type":"getMessage"}), Duration::from_millis(75))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            client.request(json!({"@type":"getMe"})).await.unwrap()["id"],
            42
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_paths_reject_symlinks_and_broad_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempdir().unwrap();
        let target = tempdir().unwrap();
        let member = account(1);
        symlink(target.path(), temp.path().join("telegram")).unwrap();
        assert!(session_dir(temp.path(), &member).is_err());
        assert!(fs::read_dir(target.path()).unwrap().next().is_none());
        fs::remove_file(temp.path().join("telegram")).unwrap();
        let session = session_dir(temp.path(), &member).unwrap();
        assert_eq!(
            fs::metadata(&session).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&session, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(session_dir(temp.path(), &member).is_err());
    }
}
