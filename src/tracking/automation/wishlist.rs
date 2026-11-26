use crate::{AppError, Shared, auth::AuthUser, catalog, now};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::LazyLock, time::Duration};
use url::Url;

pub(super) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS douban_wishlists (
        user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
        profile TEXT NOT NULL, enabled INTEGER NOT NULL, interval_hours INTEGER NOT NULL,
        auto_download INTEGER NOT NULL, last_checked INTEGER, last_error TEXT,
        run_token TEXT, run_until INTEGER NOT NULL DEFAULT 0
    ); CREATE TABLE IF NOT EXISTS douban_wish_bookmarks (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        profile TEXT NOT NULL, subject TEXT NOT NULL, PRIMARY KEY(user_id,profile,subject)
    );",
    )?;
    Ok(())
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/tracking/douban", get(read).put(save))
        .route("/api/tracking/douban/sync", post(manual))
}

fn day() -> i64 {
    24
}

#[derive(Deserialize, Serialize)]
struct Config {
    #[serde(default)]
    profile: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default = "day")]
    interval_hours: i64,
    #[serde(default)]
    auto_download: bool,
}

#[derive(Serialize)]
struct Status {
    #[serde(flatten)]
    config: Config,
    last_checked: Option<i64>,
    last_error: Option<String>,
    imported: i64,
}

fn status(conn: &Connection, user: i64) -> Result<Status, AppError> {
    Ok(conn.query_row("SELECT profile,enabled,interval_hours,auto_download,last_checked,last_error,
        (SELECT count(*) FROM douban_wish_bookmarks b WHERE b.user_id=w.user_id AND b.profile=w.profile)
        FROM douban_wishlists w WHERE user_id=?", [user], |r| Ok(Status {
        config: Config { profile: r.get(0)?, enabled: r.get(1)?, interval_hours: r.get(2)?, auto_download: r.get(3)? },
        last_checked: r.get(4)?, last_error: r.get(5)?, imported: r.get(6)?,
    })).optional()?.unwrap_or_else(|| Status {
        config: Config { profile: String::new(), enabled: false, interval_hours: day(), auto_download: false },
        last_checked: None, last_error: None, imported: 0,
    }))
}

async fn read(State(state): State<Shared>, user: AuthUser) -> Result<Json<Status>, AppError> {
    let conn = state.conn()?;
    status(&conn, user.id).map(Json)
}

async fn save(
    State(state): State<Shared>,
    user: AuthUser,
    Json(mut input): Json<Config>,
) -> Result<Json<Status>, AppError> {
    input.profile = input.profile.trim().to_owned();
    if input.profile.len() > 80
        || (!input.profile.is_empty()
            && !input
                .profile
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')))
        || (input.profile.is_empty() && input.enabled)
    {
        return Err(AppError::bad_request(
            "请填写豆瓣公开个人主页的用户 ID（字母、数字、下划线或短横线）",
        ));
    }
    if !(1..=168).contains(&input.interval_hours) {
        return Err(AppError::bad_request("同步间隔须为 1 至 168 小时"));
    }
    let conn = state.conn()?;
    conn.execute("INSERT INTO douban_wishlists(user_id,profile,enabled,interval_hours,auto_download) VALUES(?,?,?,?,?)
        ON CONFLICT(user_id) DO UPDATE SET profile=excluded.profile,enabled=excluded.enabled,
        interval_hours=excluded.interval_hours,auto_download=excluded.auto_download,
        run_token=NULL,run_until=0,last_checked=NULL,last_error=NULL",
        params![user.id,input.profile,input.enabled,input.interval_hours,input.auto_download])?;
    status(&conn, user.id).map(Json)
}

fn upstream(message: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_GATEWAY, message.into())
}
fn superseded() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "豆瓣同步配置已更改，请重新同步".into(),
    )
}

struct Run {
    user: i64,
    token: String,
    config: Config,
}

fn claim(state: &Shared, user: i64, scheduled: bool) -> Result<Option<Run>, AppError> {
    let conn = state.conn()?;
    let config = status(&conn, user)?.config;
    if config.profile.is_empty() {
        return Err(AppError::bad_request("请先设置豆瓣公开主页 ID"));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let changed = conn.execute("UPDATE douban_wishlists SET run_token=?1,run_until=?2+900
        WHERE user_id=?3 AND run_until<=?2 AND (?4=0 OR (enabled AND (last_checked IS NULL OR last_checked+3600*interval_hours<=?2)))",
        params![token,now(),user,scheduled])?;
    if changed == 0 && !scheduled {
        return Err(AppError(
            StatusCode::CONFLICT,
            "同步正在运行，请稍后刷新".into(),
        ));
    }
    Ok((changed != 0).then_some(Run {
        user,
        token,
        config,
    }))
}

impl Run {
    fn renew(&self, conn: &Connection) -> Result<(), AppError> {
        if conn.execute(
            "UPDATE douban_wishlists SET run_until=? WHERE user_id=? AND run_token=?",
            params![now() + 900, self.user, self.token],
        )? == 0
        {
            return Err(superseded());
        }
        Ok(())
    }
}

struct Wish {
    id: String,
    title: String,
    year: Option<String>,
}
struct Page {
    items: Vec<Wish>,
    next: Option<usize>,
}

static GRID: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".grid-view").unwrap());
static ITEM: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".item").unwrap());
static LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".info .title a[href]").unwrap());
static NEXT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".paginator .next a[href]").unwrap());
static INTRO: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".intro").unwrap());

fn parse_page(bytes: &[u8], url: &Url) -> Result<Page, AppError> {
    let html =
        Html::parse_document(std::str::from_utf8(bytes).map_err(|_| upstream("豆瓣页面编码异常"))?);
    let grid = html.select(&GRID).next().ok_or_else(|| {
        upstream("无法读取公开想看列表，请确认主页 ID、列表公开状态以及豆瓣访问限制")
    })?;
    let mut items = Vec::new();
    for item in grid.select(&ITEM) {
        let invalid = || upstream("豆瓣想看条目格式已更改，请检查页面后重试");
        let link = item.select(&LINK).next().ok_or_else(invalid)?;
        let target = url
            .join(link.value().attr("href").unwrap_or(""))
            .map_err(|_| invalid())?;
        let id = target
            .path()
            .trim_end_matches('/')
            .strip_prefix("/subject/")
            .filter(|s| !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(invalid)?;
        let name = link.text().collect::<String>();
        let title = name.split('/').next().unwrap_or("").trim();
        if title.is_empty() || title.chars().count() > 100 {
            return Err(invalid());
        }
        let intro = item
            .select(&INTRO)
            .next()
            .map(|v| v.text().collect::<String>())
            .unwrap_or_default();
        let year = intro
            .trim()
            .get(..4)
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned);
        items.push(Wish {
            id: id.to_owned(),
            title: title.to_owned(),
            year,
        });
    }
    let next = if let Some(link) = html.select(&NEXT).next() {
        let dest = url
            .join(link.value().attr("href").unwrap_or(""))
            .map_err(|_| upstream("豆瓣分页地址无效"))?;
        if dest.path().trim_end_matches('/') != url.path().trim_end_matches('/') {
            return Err(upstream("豆瓣分页地址异常"));
        }
        Some(
            dest.query_pairs()
                .find_map(|(k, v)| (k == "start").then(|| v.parse().ok()).flatten())
                .ok_or_else(|| upstream("豆瓣分页缺少起始位置"))?,
        )
    } else {
        None
    };
    Ok(Page { items, next })
}

async fn fetch_page(state: &Shared, profile: &str, start: usize) -> Result<Page, AppError> {
    let base = state
        .cloud_fixture
        .clone()
        .unwrap_or_else(|| Url::parse("https://movie.douban.com").unwrap());
    let mut url = base
        .join(&format!("/people/{profile}/wish"))
        .map_err(|_| AppError::internal("Invalid wishlist origin"))?;
    url.query_pairs_mut()
        .append_pair("start", &start.to_string())
        .append_pair("sort", "time")
        .append_pair("mode", "grid");
    let mut response = state
        .client
        .get(url.clone())
        .header("User-Agent", "Mozilla/5.0 (compatible; PolyView)")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| upstream("无法访问豆瓣，请检查网络并重试"))?;
    if !response.status().is_success() {
        return Err(upstream(format!(
            "豆瓣返回 HTTP {}，请确认想看列表公开并稍后重试",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| upstream("读取豆瓣页面失败"))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(upstream("豆瓣页面过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_page(&bytes, &url)
}

async fn import_wish(state: &Shared, run: &Run, wish: &Wish) -> Result<bool, AppError> {
    {
        let conn = state.conn()?;
        run.renew(&conn)?;
        if conn.query_row("SELECT EXISTS(SELECT 1 FROM douban_wish_bookmarks WHERE user_id=? AND profile=? AND subject=?)",
            params![run.user,run.config.profile,wish.id], |r| r.get::<_,bool>(0))? { return Ok(true); }
    }
    let found = catalog::search(state, &wish.title).await?;
    let matches =
        |entry: &&catalog::CatalogSummary| entry.title.trim().eq_ignore_ascii_case(&wish.title);
    let mut exact = found.results.iter().filter(matches);
    let selected = match &wish.year {
        Some(year) => exact
            .clone()
            .find(|s| &s.year == year)
            .or_else(|| exact.find(|s| s.year.is_empty())),
        None => exact.next(),
    };
    let Some(item) = selected else {
        if !found.errors.is_empty() {
            return Err(upstream("影视源搜索失败，请检查已启用的影视源"));
        }
        return Ok(false);
    };
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    run.renew(&tx)?;
    let inserted = tx.execute("INSERT INTO tracking_subscriptions(user_id,source_id,media_id,title,cover,auto_download)
        SELECT ?1,id,?2,?3,?4,?5 FROM catalog_sources WHERE id=?6 AND enabled AND searchable AND (NOT adult OR ?7)
        ON CONFLICT(user_id,source_id,media_id) DO NOTHING",
        params![run.user,item.id,item.title,item.cover,run.config.auto_download,item.source_id,catalog::adult_sources_enabled(&tx)?])?;
    if inserted == 0 && !tx.query_row("SELECT EXISTS(SELECT 1 FROM tracking_subscriptions WHERE user_id=? AND source_id=? AND media_id=?)",
        params![run.user,item.source_id,item.id], |r| r.get::<_,bool>(0))? { return Ok(false); }
    tx.execute(
        "INSERT OR IGNORE INTO douban_wish_bookmarks(user_id,profile,subject) VALUES(?,?,?)",
        params![run.user, run.config.profile, wish.id],
    )?;
    tx.commit()?;
    Ok(true)
}

async fn synchronize(state: &Shared, run: &Run) -> Result<(), AppError> {
    let mut start = 0;
    let mut seen = HashSet::new();
    let mut unmatched = 0;
    let mut first_missing = String::new();
    for _ in 0..200 {
        {
            let conn = state.conn()?;
            run.renew(&conn)?;
        }
        let page = fetch_page(state, &run.config.profile, start).await?;
        for wish in page.items {
            if seen.insert(wish.id.clone()) && !import_wish(state, run, &wish).await? {
                if unmatched == 0 {
                    first_missing = wish.title;
                }
                unmatched += 1;
            }
        }
        let Some(next) = page.next else {
            return if unmatched == 0 {
                Ok(())
            } else {
                Err(upstream(format!(
                    "{unmatched} 部想看未匹配到影视源（含《{first_missing}》），请添加或启用相应影视源后重试"
                )))
            };
        };
        if next <= start {
            return Err(upstream("豆瓣分页重复，请稍后重试"));
        }
        start = next;
    }
    Err(upstream("豆瓣想看列表超出 200 页同步上限"))
}

async fn sync(state: &Shared, user: i64, scheduled: bool) -> Result<(), AppError> {
    let Some(run) = claim(state, user, scheduled)? else {
        return Ok(());
    };
    let outcome = synchronize(state, &run).await;
    let message = outcome.as_ref().err().map(|e| {
        if e.0 == StatusCode::INTERNAL_SERVER_ERROR {
            "豆瓣同步内部错误，请查看服务日志"
        } else {
            &e.1
        }
    });
    if state.conn()?.execute("UPDATE douban_wishlists SET run_token=NULL,run_until=0,last_checked=?,last_error=? WHERE user_id=? AND run_token=?",
        params![now(),message,user,run.token])? == 0 { return Err(superseded()); }
    outcome
}

async fn manual(State(state): State<Shared>, user: AuthUser) -> Result<Json<Status>, AppError> {
    sync(&state, user.id, false).await?;
    read(State(state), user).await
}

pub(crate) async fn sync_due(state: &Shared) -> Result<(), AppError> {
    let users = {
        let conn = state.conn()?;
        conn.prepare("SELECT user_id FROM douban_wishlists WHERE enabled AND run_until<=?1 AND (last_checked IS NULL OR last_checked+3600*interval_hours<=?1) ORDER BY user_id")?
            .query_map([now()], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?
    };
    for user in users {
        if let Err(error) = sync(state, user, true).await {
            tracing::warn!(user_id=user, status=%error.0, "Douban wishlist sync failed");
        }
    }
    Ok(())
}
