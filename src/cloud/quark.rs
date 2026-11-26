use super::accounts::{CheckedShare, DriveAccount, Folder, SavedShare};
use crate::AppError;
use axum::http::StatusCode;
use reqwest::{Method, RequestBuilder, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};
use tokio::time::{sleep, timeout};

const FILE_SORT: &str = "/1/clouddrive/file/sort";
const TOKEN: &str = "/1/clouddrive/share/sharepage/token";
const DETAIL: &str = "/1/clouddrive/share/sharepage/detail";
const SAVE: &str = "/1/clouddrive/share/sharepage/save";
const TASK: &str = "/1/clouddrive/task";
const PAGE_SIZE: usize = 50;
const PAGE_LIMIT: usize = 40;
const BODY_LIMIT: usize = 1024 * 1024;
const INVALID_SHARE: &str = "夸克分享已失效或没有可转存的文件";
const UNCONFIRMED: &str = "尚未确认夸克转存结果，请先在夸克网盘查看任务";

fn upstream(message: &str) -> AppError {
    AppError(StatusCode::BAD_GATEWAY, message.into())
}

fn malformed() -> AppError {
    upstream("夸克网盘返回的响应格式无效")
}

fn verification() -> AppError {
    AppError(StatusCode::FORBIDDEN, "请在夸克网盘中登录或完成验证".into())
}

fn is_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_share(raw: &str, code: Option<&str>) -> Result<(String, String), AppError> {
    let invalid = || AppError::bad_request("请输入有效的夸克网盘分享链接");
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let id = url
        .path()
        .trim_end_matches('/')
        .strip_prefix("/s/")
        .ok_or_else(invalid)?;
    if url.scheme() != "https"
        || url.host_str() != Some("pan.quark.cn")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !is_id(id)
        || url
            .fragment()
            .is_some_and(|fragment| !matches!(fragment.trim_end_matches('/'), "" | "/list/share"))
        || url
            .query_pairs()
            .any(|(k, v)| matches!(k.as_ref(), "fid" | "pdir_fid") && v != "0")
    {
        return Err(invalid());
    }
    let code = code
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            url.query_pairs()
                .find(|(k, _)| k == "pwd" || k == "passcode")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_default();
    if code.len() > 64 || !code.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(AppError::bad_request("无效的夸克分享提取码"));
    }
    Ok((id.into(), code))
}

fn request(account: &DriveAccount, method: Method, path: &str) -> Result<RequestBuilder, AppError> {
    Ok(account
        .request(method, path)?
        .header("Origin", "https://pan.quark.cn")
        .header("Referer", "https://pan.quark.cn/")
        .header("Accept", "application/json")
        .query(&[("pr", "ucpro"), ("fr", "pc"), ("uc_param_str", "")])
        .timeout(Duration::from_secs(8)))
}

fn post(account: &DriveAccount, path: &str, data: Value) -> Result<RequestBuilder, AppError> {
    Ok(request(account, Method::POST, path)?
        .header("Content-Type", "application/json")
        .body(data.to_string()))
}

#[derive(Deserialize)]
struct Envelope {
    status: Option<u16>,
    code: Option<i64>,
    data: Option<Value>,
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "_total")]
    total: Option<usize>,
}

async fn receive(builder: RequestBuilder) -> Result<Envelope, AppError> {
    let transport_error = || upstream("无法连接夸克网盘");
    let mut response = builder.send().await.map_err(|_| transport_error())?;
    let http = response.status();
    if http == StatusCode::TOO_MANY_REQUESTS {
        return Err(AppError(http, "夸克网盘请求频率过高，请稍后再试".into()));
    }
    if matches!(http.as_u16(), 401 | 403 | 412) || http.is_redirection() {
        return Err(verification());
    }
    if response
        .content_length()
        .is_some_and(|n| n > BODY_LIMIT as u64)
    {
        return Err(malformed());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| transport_error())? {
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return Err(malformed());
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    if matches!(envelope.status, Some(401 | 403 | 412)) {
        return Err(verification());
    }
    if envelope.status == Some(429) {
        return Err(AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "夸克网盘请求频率过高，请稍后再试".into(),
        ));
    }
    match envelope.code {
        Some(20002 | 31001 | 31002) => return Err(verification()),
        Some(24001 | 41010 | 41012) => return Err(AppError::not_found(INVALID_SHARE)),
        Some(24000 | 41007..=41009) => {
            return Err(AppError::bad_request("夸克分享提取码缺失或不正确"));
        }
        _ => (),
    }
    if !http.is_success()
        || envelope.status.is_some_and(|s| s != 200)
        || envelope.code.is_some_and(|code| code != 0)
    {
        return Err(upstream("夸克网盘拒绝了请求，请在网盘中查看详情"));
    }
    if envelope.code.is_none() && envelope.status.is_none() {
        return Err(malformed());
    }
    Ok(envelope)
}

impl Envelope {
    fn data<T: DeserializeOwned>(self) -> Result<T, AppError> {
        serde_json::from_value(self.data.ok_or_else(malformed)?).map_err(|_| malformed())
    }
}

struct Share {
    id: String,
    stoken: String,
}

async fn open_share(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
) -> Result<Share, AppError> {
    #[derive(Deserialize)]
    struct Token {
        stoken: String,
    }
    let (id, passcode) = parse_share(url, code)?;
    let token: Token = receive(post(
        account,
        TOKEN,
        json!({"pwd_id": id, "passcode": passcode}),
    )?)
    .await?
    .data()?;
    if token.stoken.is_empty() || token.stoken.len() > 8192 {
        return Err(malformed());
    }
    Ok(Share {
        id,
        stoken: token.stoken,
    })
}

#[derive(Deserialize)]
struct Entry {
    fid: String,
    file_name: String,
    dir: Option<bool>,
    file: Option<bool>,
    file_type: Option<u8>,
    share_fid_token: Option<String>,
    obj_category: Option<String>,
    category: Option<u8>,
}

impl Entry {
    fn folder(&self) -> bool {
        self.dir
            .or_else(|| self.file.map(|f| !f))
            .unwrap_or(self.file_type == Some(0))
    }

    fn video(&self) -> bool {
        !self.folder()
            && (self.category == Some(1) || self.obj_category.as_deref() == Some("video"))
    }
}

#[derive(Deserialize)]
struct ShareInfo {
    title: Option<String>,
}

#[derive(Deserialize)]
struct Page {
    list: Vec<Entry>,
    share: Option<ShareInfo>,
}

// metadata._total counts files and folders together. Keep the unfiltered count for pagination.
async fn list(
    account: &DriveAccount,
    dir: &str,
    share: Option<&Share>,
    first_page_only: bool,
) -> Result<Page, AppError> {
    if !is_id(dir) {
        return Err(AppError::bad_request("无效的夸克网盘目录 ID"));
    }
    let mut result = Page {
        list: Vec::new(),
        share: None,
    };
    let mut seen = HashSet::new();
    for page in 1..=PAGE_LIMIT {
        let path = if share.is_some() { DETAIL } else { FILE_SORT };
        let mut req = request(account, Method::GET, path)?
            .query(&[("_page", page), ("_size", PAGE_SIZE)])
            .query(&[
                ("pdir_fid", dir),
                ("_fetch_total", "1"),
                ("_sort", "file_type:asc,file_name:asc"),
            ]);
        if let Some(share) = share {
            req = req.query(&[
                ("pwd_id", share.id.as_str()),
                ("stoken", share.stoken.as_str()),
                ("force", "0"),
                ("ver", "2"),
                ("_fetch_share", "1"),
            ]);
        } else {
            req = req.query(&[("_fetch_sub_dirs", "0"), ("fetch_all_file", "1")]);
        }
        let response = receive(req).await?;
        let total = response.metadata.as_ref().and_then(|m| m.total);
        let data: Page = response.data()?;
        if page == 1 {
            result.share = data.share;
        }
        let count = data.list.len();
        if count > PAGE_SIZE {
            return Err(malformed());
        }
        for item in data.list {
            if !is_id(&item.fid) || item.file_name.is_empty() || !seen.insert(item.fid.clone()) {
                return Err(malformed());
            }
            if share.is_some() || item.folder() {
                result.list.push(item);
            }
        }
        if total.is_some_and(|t| seen.len() > t || (count == 0 && seen.len() < t)) {
            return Err(malformed());
        }
        if first_page_only || total.map_or(count < PAGE_SIZE, |t| seen.len() == t) {
            return Ok(result);
        }
    }
    Err(upstream("夸克网盘目录超过本次可处理的文件数量"))
}

pub(super) async fn folders(account: &DriveAccount, dir: &str) -> Result<Vec<Folder>, AppError> {
    let page = timeout(Duration::from_secs(45), list(account, dir, None, false))
        .await
        .map_err(|_| upstream("读取夸克网盘目录超时"))??;
    Ok(page
        .list
        .into_iter()
        .map(|f| Folder {
            id: f.fid,
            name: f.file_name,
        })
        .collect())
}

pub(super) async fn check(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
) -> Result<CheckedShare, AppError> {
    let page = async {
        let share = open_share(account, url, code).await?;
        list(account, "0", Some(&share), true).await
    }
    .await;
    match page {
        Ok(page) => Ok(CheckedShare {
            valid: !page.list.is_empty(),
            title: page
                .share
                .and_then(|s| s.title)
                .or_else(|| page.list.into_iter().next().map(|f| f.file_name)),
        }),
        Err(e) if e.0 == StatusCode::NOT_FOUND => Ok(CheckedShare {
            valid: false,
            title: None,
        }),
        Err(e) => Err(e),
    }
}

pub(super) async fn save(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
    dir: &str,
) -> Result<SavedShare, AppError> {
    if !is_id(dir) {
        return Err(AppError::bad_request("无效的夸克网盘目录 ID"));
    }
    let share = open_share(account, url, code).await?;
    let page = timeout(
        Duration::from_secs(45),
        list(account, "0", Some(&share), false),
    )
    .await
    .map_err(|_| upstream("读取夸克分享超时"))??;
    if page.list.is_empty() {
        return Err(AppError::not_found(INVALID_SHARE));
    }
    let single_video = page.list.len() == 1 && page.list[0].video();
    let mut fids = Vec::with_capacity(page.list.len());
    let mut tokens = Vec::with_capacity(page.list.len());
    for item in page.list {
        let token = item
            .share_fid_token
            .filter(|t| !t.is_empty() && t.len() <= 8192)
            .ok_or_else(malformed)?;
        fids.push(item.fid);
        tokens.push(token);
    }
    let payload = json!({"pwd_id": share.id, "stoken": share.stoken, "pdir_fid": "0", "scene": "link",
        "to_pdir_fid": dir, "fid_list": fids, "fid_token_list": tokens});
    #[derive(Deserialize)]
    struct Submission {
        task_id: String,
    }
    let submission: Submission = receive(post(account, SAVE, payload)?)
        .await?
        .data()
        .map_err(|_| upstream(UNCONFIRMED))?;
    if !is_id(&submission.task_id) {
        return Err(upstream(UNCONFIRMED));
    }
    let result = timeout(Duration::from_secs(30), poll(account, &submission.task_id))
        .await
        .map_err(|_| upstream(UNCONFIRMED))??;
    let media_id = result
        .pointer("/save_as/save_as_top_fids")
        .and_then(Value::as_array)
        .filter(|ids| single_video && ids.len() == 1)
        .and_then(|ids| ids[0].as_str())
        .filter(|id| is_id(id))
        .map(str::to_owned);
    Ok(SavedShare {
        path: if dir == "0" {
            "/".into()
        } else {
            format!("quark://folder/{dir}")
        },
        task_id: Some(submission.task_id),
        media_id,
    })
}

async fn poll(account: &DriveAccount, id: &str) -> Result<Value, AppError> {
    for attempt in 0..30 {
        if attempt > 0 {
            sleep(Duration::from_millis(500)).await;
        }
        let builder = request(account, Method::GET, TASK)?
            .query(&[("task_id", id)])
            .query(&[("retry_index", attempt)]);
        let task: Value = receive(builder).await?.data()?;
        match task["status"].as_u64() {
            Some(0 | 1) => (),
            Some(2) => return Ok(task),
            Some(3) => return Err(upstream("夸克网盘转存任务失败，请在网盘中查看详情")),
            _ => return Err(upstream(UNCONFIRMED)),
        }
    }
    Err(upstream(UNCONFIRMED))
}

pub(super) async fn media_url(account: &DriveAccount, fid: &str) -> Result<Url, AppError> {
    if !is_id(fid) {
        return Err(AppError::bad_request("无效的夸克视频文件 ID"));
    }
    // /file/download binds the CDN request to the account cookie. Playback projections
    // instead provide signed video URLs that our relay can fetch without credentials.
    let data: Value = receive(post(
        account,
        "/1/clouddrive/file/v2/play/project",
        json!({
            "fid": fid, "resolutions": "low,normal,high,super,2k,4k", "supports": "fmp4_av"
        }),
    )?)
    .await?
    .data()?;
    for variant in data["video_list"].as_array().ok_or_else(malformed)? {
        let info = &variant["video_info"];
        if info["success"] == false || info["finish"] == false {
            continue;
        }
        let Some(raw) = info["url"].as_str() else {
            continue;
        };
        let Ok(url) = Url::parse(raw) else {
            continue;
        };
        let format = info["format"].as_str().unwrap_or_default();
        if url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none()
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".quark.cn"))
            && !url.path().to_ascii_lowercase().ends_with(".m3u8")
            && (format.eq_ignore_ascii_case("mp4")
                || format.eq_ignore_ascii_case("fmp4")
                || url.path().to_ascii_lowercase().ends_with(".mp4"))
        {
            return Ok(url);
        }
    }
    Err(upstream("夸克网盘没有返回可播放的单文件视频"))
}

#[cfg(test)]
mod tests;
