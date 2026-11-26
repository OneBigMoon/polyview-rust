//! Tianyi web session protocol. Routes and fields follow otokaze/189Cloud-Downloader
//! (dao/share.go) and tickstep/cloudpan189-api (cloudpan/{share,batch_task}.go).
//! The web AppKey signature covers the raw query for GET and raw form for POST.
use super::accounts::{CheckedShare, DriveAccount, Folder, SavedShare};
use crate::AppError;
use axum::{
    body::{Body, to_bytes},
    http::StatusCode,
};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_KEY: &str = "600100422";
const INFO: &str = "/api/open/share/getShareInfoByCodeV2.action";
const ACCESS: &str = "/api/open/share/checkAccessCode.action";
const LIST: &str = "/api/open/file/listFiles.action";
const SHARE_LIST: &str = "/api/open/share/listShareDir.action";
const CREATE: &str = "/api/open/batch/createBatchTask.action";
const TASK: &str = "/api/open/batch/checkBatchTask.action";
const PAGE: usize = 100;

fn malformed() -> AppError {
    AppError(StatusCode::BAD_GATEWAY, "天翼云盘响应不完整".into())
}
fn refused() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "天翼云盘拒绝请求，请在网盘中检查登录、分享或完成验证".into(),
    )
}
fn unconfirmed() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "天翼云盘尚未确认转存成功，请在网盘中检查任务".into(),
    )
}

fn parse_link(raw: &str, code: Option<&str>) -> Result<(String, String), AppError> {
    let invalid = || AppError::bad_request("无效的天翼云盘分享链接或提取码");
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(invalid());
    }
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let key = match (url.host_str(), url.path().trim_end_matches('/')) {
        (Some("cloud.189.cn"), "/web/share") => query("code"),
        (Some("cloud.189.cn"), path) => path.strip_prefix("/t/").map(str::to_owned),
        (Some("h5.cloud.189.cn"), "/share.html") => url
            .fragment()
            .and_then(|s| s.strip_prefix("/t/"))
            .map(str::to_owned),
        _ => None,
    }
    .ok_or_else(invalid)?;
    let pass = code
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| query("accessCode"))
        .unwrap_or_default();
    if key.is_empty()
        || key.len() > 128
        || pass.len() > 32
        || !key
            .bytes()
            .chain(pass.bytes())
            .all(|c| c.is_ascii_alphanumeric())
    {
        return Err(invalid());
    }
    Ok((key, pass))
}

fn signature(params: &[(&str, &str)], timestamp: &str) -> String {
    let mut fields = params.to_vec();
    fields.extend([("AppKey", APP_KEY), ("Timestamp", timestamp)]);
    fields.sort_unstable();
    let mut digest = md5::Context::new();
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            digest.consume(b"&");
        }
        digest.consume(key);
        digest.consume(b"=");
        digest.consume(value);
    }
    format!("{:x}", digest.finalize())
}

async fn request(
    account: &DriveAccount,
    method: Method,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Value, AppError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| malformed())?
        .as_millis()
        .to_string();
    let builder = account
        .request(method.clone(), path)?
        .timeout(Duration::from_secs(8))
        .header("Accept", "application/json;charset=UTF-8")
        .header("Referer", "https://cloud.189.cn/web/main/")
        .header("Origin", "https://cloud.189.cn")
        .header("Sign-Type", "1")
        .header("AppKey", APP_KEY)
        .header("Signature", signature(params, &timestamp))
        .header("Timestamp", timestamp);
    let builder = if method == Method::GET {
        builder.query(params)
    } else {
        builder.form(params)
    };
    let response = builder.send().await.map_err(|_| refused())?;
    if !response.status().is_success() {
        return Err(refused());
    }
    let bytes = to_bytes(Body::from_stream(response.bytes_stream()), 2 * 1024 * 1024)
        .await
        .map_err(|_| malformed())?;
    let data: Value = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    if !data.is_object()
        || data.get("errorCode").is_some_and(|x| !x.is_null())
        || data.get("errorVO").is_some_and(|x| !x.is_null())
        || data.get("res_code").is_some_and(|v| v != 0 && v != "0")
    {
        return Err(refused());
    }
    Ok(data)
}

fn identifier(v: &Value) -> Result<String, AppError> {
    if let Some(n) = v.as_u64() {
        return Ok(n.to_string());
    }
    v.as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
        .map(str::to_owned)
        .ok_or_else(malformed)
}

struct Share {
    id: String,
    file_id: String,
    title: String,
    folder: bool,
    mode: String,
    pass: String,
}

async fn discover(
    account: &DriveAccount,
    raw: &str,
    code: Option<&str>,
) -> Result<Share, AppError> {
    let (key, pass) = parse_link(raw, code)?;
    let data = request(account, Method::GET, INFO, &[("shareCode", &key)]).await?;
    let id = if matches!(data["needAccessCode"].as_i64(), Some(1))
        || data["needAccessCode"] == true
        || data["needAccessCode"] == "1"
    {
        if pass.is_empty() {
            return Err(AppError::bad_request("此天翼分享需要提取码"));
        }
        let access = request(
            account,
            Method::GET,
            ACCESS,
            &[("shareCode", &key), ("accessCode", &pass)],
        )
        .await?;
        identifier(&access["shareId"])?
    } else {
        identifier(&data["shareId"])?
    };
    if id == "0" {
        return Err(refused());
    }
    Ok(Share {
        id,
        file_id: identifier(&data["fileId"])?,
        title: data["fileName"].as_str().ok_or_else(malformed)?.into(),
        folder: data["isFolder"].as_bool().ok_or_else(malformed)?,
        mode: data
            .get("shareMode")
            .map(identifier)
            .transpose()?
            .unwrap_or_else(|| "1".into()),
        pass,
    })
}

pub(super) async fn check(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
) -> Result<CheckedShare, AppError> {
    let share = discover(account, url, code).await?;
    Ok(CheckedShare {
        valid: true,
        title: Some(share.title),
    })
}

#[derive(Deserialize)]
struct Entry {
    id: Value,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listing {
    count: usize,
    #[serde(default)]
    file_list: Vec<Entry>,
    #[serde(default)]
    folder_list: Vec<Entry>,
}

async fn listing(
    account: &DriveAccount,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Listing, AppError> {
    let mut result = Listing {
        count: 0,
        file_list: Vec::new(),
        folder_list: Vec::new(),
    };
    for number in 1..=100 {
        let page = number.to_string();
        let mut query = params.to_vec();
        query.extend([
            ("pageNum", &page[..]),
            ("pageSize", "100"),
            ("orderBy", "lastOpTime"),
            ("descending", "true"),
            ("iconOption", "5"),
        ]);
        let mut data = request(account, Method::GET, path, &query).await?;
        let list: Listing =
            serde_json::from_value(data["fileListAO"].take()).map_err(|_| malformed())?;
        let received = list.folder_list.len() + list.file_list.len();
        if received > PAGE || (received == 0 && list.count > result.count) {
            return Err(malformed());
        }
        result.folder_list.extend(list.folder_list);
        result.file_list.extend(list.file_list);
        result.count += received;
        if received < PAGE && result.count >= list.count {
            return Ok(result);
        }
    }
    Err(AppError::bad_request("天翼目录中的项目过多"))
}

pub(super) async fn folders(account: &DriveAccount, dir: &str) -> Result<Vec<Folder>, AppError> {
    let list = listing(account, LIST, &[("folderId", dir), ("mediaType", "0")]).await?;
    list.folder_list
        .into_iter()
        .map(|entry| {
            Ok(Folder {
                id: identifier(&entry.id)?,
                name: entry.name,
            })
        })
        .collect()
}

pub(super) async fn save(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
    dir: &str,
) -> Result<SavedShare, AppError> {
    let share = discover(account, url, code).await?;
    let mut query = vec![
        ("shareId", share.id.as_str()),
        ("shareMode", &share.mode),
        ("fileId", &share.file_id),
        ("accessCode", &share.pass),
        ("isFolder", if share.folder { "true" } else { "false" }),
    ];
    if share.folder {
        query.push(("shareDirFileId", &share.file_id));
    }
    let list = listing(account, SHARE_LIST, &query).await?;
    if list.count == 0 {
        return Err(AppError::bad_request("天翼分享中没有可转存的文件"));
    }
    let mut tasks = Vec::with_capacity(list.count);
    for (kind, entries) in [(0, list.file_list), (1, list.folder_list)] {
        for entry in entries {
            tasks.push(
                json!({"fileId":identifier(&entry.id)?, "fileName":entry.name,"isFolder":kind}),
            );
        }
    }
    let task_infos = serde_json::to_string(&tasks).map_err(|_| malformed())?;
    let created = request(
        account,
        Method::POST,
        CREATE,
        &[
            ("type", "SHARE_SAVE"),
            ("targetFolderId", dir),
            ("shareId", &share.id),
            ("taskInfos", &task_infos),
        ],
    )
    .await?;
    let task = identifier(&created["taskId"])?;
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let state = request(
                account,
                Method::POST,
                TASK,
                &[("type", "SHARE_SAVE"), ("taskId", &task)],
            )
            .await?;
            if state
                .get("taskId")
                .map(identifier)
                .transpose()?
                .is_some_and(|id| id != task)
            {
                return Err(unconfirmed());
            }
            match state["taskStatus"].as_i64() {
                Some(4)
                    if ["failedCount", "skipCount"]
                        .iter()
                        .all(|k| state.get(*k).is_none_or(|v| v == 0))
                        && state
                            .get("successedCount")
                            .is_none_or(|v| v.as_u64().is_some_and(|n| n > 0)) =>
                {
                    return Ok(());
                }
                Some(1 | 3) => tokio::time::sleep(Duration::from_millis(500)).await,
                _ => return Err(unconfirmed()),
            }
        }
    })
    .await
    .map_err(|_| unconfirmed())??;
    Ok(SavedShare {
        path: if dir == "-11" {
            "/".into()
        } else {
            format!("tianyi://folder/{dir}")
        },
        task_id: Some(task),
        media_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Form, Json, Router,
        extract::Query,
        http::HeaderMap,
        routing::{get, post},
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    type Params = HashMap<String, String>;

    #[test]
    fn web_signature_uses_sorted_unescaped_fields_and_timestamp() {
        assert_eq!(
            signature(&[("shareCode", "abc"), ("accessCode", "Rx99")], "123"),
            "87659c9535bfdd70441a77e4a01a49fe"
        );
    }

    async fn fixture(status: u64) -> DriveAccount {
        let polls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/api/open/file/listFiles.action", get(|headers: HeaderMap, Query(q): Query<Params>| async move {
                assert_eq!(headers["cookie"], "session=fixture");
                assert_eq!(headers["sign-type"], "1");
                assert_eq!(headers["appkey"], APP_KEY);
                let params = q.iter().map(|(k,v)| (k.as_str(),v.as_str())).collect::<Vec<_>>();
                assert_eq!(headers["signature"].to_str().unwrap(), signature(&params, headers["timestamp"].to_str().unwrap()));
                assert_eq!(q["folderId"], "-11");
                Json(json!({"res_code":0,"fileListAO":{"count":2,"folderList":[{"id":41,"name":"Films"}],"fileList":[{"id":42,"name":"Clip"}]}}))
            }))
            .route("/api/open/share/getShareInfoByCodeV2.action", get(|Query(q): Query<Params>| async move {
                assert_eq!(q["shareCode"],"abc");
                Json(json!({"res_code":0,"needAccessCode":1,"isFolder":true,"shareMode":1,"fileId":"31","fileName":"Share"}))
            }))
            .route("/api/open/share/checkAccessCode.action", get(|Query(q): Query<Params>| async move {
                assert_eq!(q["shareCode"],"abc");
                Json(if q["accessCode"] == "Rx99" { json!({"res_code":0,"shareId":76}) } else { json!({"res_code":"ShareAccessCodeError","res_message":"session=secret"}) })
            }))
            .route("/api/open/share/listShareDir.action", get(|Query(q): Query<Params>| async move {
                assert_eq!((&q["shareId"][..], &q["shareDirFileId"][..], &q["accessCode"][..]), ("76", "31", "Rx99"));
                Json(json!({"res_code":0,"fileListAO":{"count":1,"fileList":[{"id":9223372036854775807_i64,"name":"Episode 1.mkv"}]}}))
            }))
            .route("/api/open/batch/createBatchTask.action", post(|headers:HeaderMap, Form(q): Form<Params>| async move {
                assert_eq!(headers["cookie"], "session=fixture");
                assert_eq!((&q["targetFolderId"][..], &q["shareId"][..], &q["type"][..]), ("41", "76", "SHARE_SAVE"));
                assert_eq!(serde_json::from_str::<Value>(&q["taskInfos"]).unwrap(), json!([{"fileId":"9223372036854775807","fileName":"Episode 1.mkv","isFolder":0}]));
                Json(json!({"res_code":0,"taskId":"123"}))
            }))
            .route("/api/open/batch/checkBatchTask.action", post(move |Form(q): Form<Params>| {
                let polls = polls.clone();
                async move {
                    assert_eq!((&q["type"][..], &q["taskId"][..]), ("SHARE_SAVE", "123"));
                    let state = if polls.fetch_add(1, Ordering::Relaxed) == 0 { 1 } else { status };
                    Json(json!({"res_code":0,"taskId":"123","taskStatus":state,"failedCount":0,"skipCount":0}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        DriveAccount::fixture("tianyi", origin, "session=fixture")
    }

    #[tokio::test]
    async fn signed_session_lists_checks_code_and_confirms_transfer() {
        let account = fixture(4).await;
        let rows = folders(&account, "-11").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "41");
        let link = "https://cloud.189.cn/web/share?code=abc&accessCode=Rx99";
        assert_eq!(
            check(&account, link, None).await.unwrap().title.as_deref(),
            Some("Share")
        );
        let saved = save(&account, link, None, "41").await.unwrap();
        assert_eq!(saved.task_id.as_deref(), Some("123"));
        assert_eq!(saved.path, "tianyi://folder/41");
    }

    #[tokio::test]
    async fn rejects_wrong_codes_untrusted_origins_and_task_conflicts() {
        let account = fixture(2).await;
        let link = "https://cloud.189.cn/t/abc";
        assert!(
            check(&account, "https://cloud.189.cn@attacker.test/t/abc", None)
                .await
                .is_err()
        );
        assert!(check(&account, link, None).await.is_err());
        let err = check(&account, link, Some("wrong")).await.err().unwrap();
        assert!(!err.1.contains("secret"));
        assert!(save(&account, link, Some("Rx99"), "41").await.is_err());
    }
}
