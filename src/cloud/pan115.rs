//! Web session protocol documented by `ChenyangGao/p115client` (fs_files, share_snap, share_receive).
use super::accounts::{CheckedShare, DriveAccount, Folder, SavedShare};
use crate::AppError;
use axum::{
    body::{Body, to_bytes},
    http::StatusCode,
};
use reqwest::{Method, RequestBuilder, Url};
use serde_json::Value;
use std::time::Duration;

const PAGE: usize = 100;
const MAX_ITEMS: usize = 10_000;

fn malformed() -> AppError {
    AppError(StatusCode::BAD_GATEWAY, "115 网盘响应不完整".into())
}

fn share(raw: &str, code: Option<&str>) -> Result<(String, String), AppError> {
    let invalid = || AppError::bad_request("无效的 115 分享链接或提取码");
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let key = url
        .path()
        .trim_end_matches('/')
        .strip_prefix("/s/")
        .ok_or_else(invalid)?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("115.com" | "115cdn.com" | "anxia.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || key.is_empty()
        || key.len() > 128
        || !key.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(invalid());
    }
    let password = code
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            url.query_pairs()
                .find(|(k, _)| k == "password" || k == "receive_code")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_default();
    if password.len() > 32 || !password.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err(invalid());
    }
    Ok((key.into(), password))
}

fn id(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.into());
    }
    value.as_u64().map(|n| n.to_string())
}

fn validate_dir(dir: &str) -> Result<(), AppError> {
    if dir.is_empty() || !dir.bytes().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad_request("无效的 115 网盘目录编号"));
    }
    Ok(())
}

async fn receive(request: RequestBuilder) -> Result<Value, AppError> {
    let rejected = || {
        AppError(
            StatusCode::BAD_GATEWAY,
            "115 网盘拒绝请求，请在网盘中检查登录或完成验证".into(),
        )
    };
    let res = request
        .header("Referer", "https://115.com/")
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .map_err(|_| rejected())?;
    if !res.status().is_success() {
        return Err(rejected());
    }
    let bytes = to_bytes(Body::from_stream(res.bytes_stream()), 1024 * 1024)
        .await
        .map_err(|_| malformed())?;
    let data: Value = serde_json::from_slice(&bytes).map_err(|_| rejected())?;
    if data["state"] != true {
        if data["errno"] == 4100008 {
            return Err(AppError::bad_request("115 分享提取码缺失或不正确"));
        }
        return Err(rejected());
    }
    Ok(data)
}

fn page_end(offset: usize, rows: &[Value], total: &Value) -> Result<bool, AppError> {
    let total: usize = id(total)
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)?;
    let end = offset + rows.len();
    if total > MAX_ITEMS || end > total || (rows.is_empty() && end < total) {
        return Err(malformed());
    }
    Ok(end == total)
}

pub(super) async fn folders(account: &DriveAccount, dir: &str) -> Result<Vec<Folder>, AppError> {
    validate_dir(dir)?;
    let mut folders = Vec::new();
    let mut offset = 0;
    loop {
        let res = receive(account.request(Method::GET, "/files")?.query(&[
            ("cid", dir),
            ("offset", &offset.to_string()),
            ("limit", &PAGE.to_string()),
            ("show_dir", "1"),
            ("cur", "1"),
            ("nf", "1"),
            ("o", "file_name"),
            ("asc", "1"),
        ]))
        .await?;
        let rows = res["data"].as_array().ok_or_else(malformed)?;
        for row in rows {
            if id(&row["fid"]).is_none_or(|file| file == "0") {
                folders.push(Folder {
                    id: id(&row["cid"]).ok_or_else(malformed)?,
                    name: row["n"].as_str().ok_or_else(malformed)?.into(),
                });
            }
        }
        if page_end(offset, rows, &res["count"])? {
            return Ok(folders);
        }
        offset += rows.len();
    }
}

async fn snapshot(
    account: &DriveAccount,
    key: &str,
    pass: &str,
    offset: usize,
) -> Result<Value, AppError> {
    let mut res = receive(account.request(Method::GET, "/share/snap")?.query(&[
        ("share_code", key),
        ("receive_code", pass),
        ("cid", "0"),
        ("offset", &offset.to_string()),
        ("limit", &PAGE.to_string()),
    ]))
    .await?;
    res.get_mut("data").map(Value::take).ok_or_else(malformed)
}

pub(super) async fn check(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
) -> Result<CheckedShare, AppError> {
    let (key, pass) = share(url, code)?;
    let data = snapshot(account, &key, &pass, 0).await?;
    let rows = data["list"].as_array().ok_or_else(malformed)?;
    Ok(CheckedShare {
        valid: !rows.is_empty(),
        title: data["shareinfo"]["share_title"].as_str().map(str::to_owned),
    })
}

pub(super) async fn save(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
    dir: &str,
) -> Result<SavedShare, AppError> {
    validate_dir(dir)?;
    let (key, pass) = share(url, code)?;
    let mut ids = Vec::new();
    loop {
        let offset = ids.len();
        let data = snapshot(account, &key, &pass, offset).await?;
        let rows = data["list"].as_array().ok_or_else(malformed)?;
        for row in rows {
            let item = id(&row["fid"])
                .filter(|id| id != "0")
                .or_else(|| id(&row["cid"]))
                .filter(|id| id != "0")
                .ok_or_else(malformed)?;
            ids.push(item);
        }
        if page_end(offset, rows, &data["count"])? {
            break;
        }
    }
    if ids.is_empty() {
        return Err(AppError::bad_request("115 分享中没有可转存的文件"));
    }
    receive(account.request(Method::POST, "/share/receive")?.form(&[
        ("share_code", key.as_str()),
        ("receive_code", &pass),
        ("file_id", &ids.join(",")),
        ("cid", dir),
    ]))
    .await?;
    Ok(SavedShare {
        path: if dir == "0" {
            "/".into()
        } else {
            format!("pan115://folder/{dir}")
        },
        task_id: None,
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
    use serde_json::json;
    use std::collections::HashMap;
    type Params = HashMap<String, String>;

    async fn fixture(app: Router) -> DriveAccount {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        DriveAccount::fixture("115", origin, "UID=fixture")
    }

    #[tokio::test]
    async fn lists_folders_and_receives_share_files_and_folders() {
        let app = Router::new().route("/files", get(|Query(q): Query<Params>| async move {
            assert_eq!(q["cid"], "0");
            Json(json!({"state":true,"count":2,"data":[{"cid":"5","n":"Movies"},{"fid":"6","cid":"0","n":"Film"}]}))
        })).route("/share/snap", get(|Query(q): Query<Params>, headers: HeaderMap| async move {
            assert_eq!(headers["cookie"], "UID=fixture");
            assert_eq!((&q["share_code"][..], &q["receive_code"][..]), ("abc", "pwd1"));
            let items = if q["offset"] == "0" { json!([{"cid":"10","n":"folder"}]) } else { json!([{"fid":"11","cid":"0","n":"file"}]) };
            Json(json!({"state":true,"data":{"list":items,"count":2,"shareinfo":{"share_title":"Films"}}}))
        })).route("/share/receive", post(|Form(q): Form<Params>| async move {
            assert_eq!((&q["file_id"][..], &q["cid"][..], &q["receive_code"][..], &q["share_code"][..]), ("10,11", "5", "pwd1", "abc"));
            Json(json!({"state":true}))
        }));
        let account = fixture(app).await;
        let rows = folders(&account, "0").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Movies");
        let link = "https://115cdn.com/s/abc?password=pwd1";
        assert_eq!(
            check(&account, link, None).await.unwrap().title.as_deref(),
            Some("Films")
        );
        assert_eq!(
            save(&account, link, None, "5").await.unwrap().path,
            "pan115://folder/5"
        );
    }

    #[tokio::test]
    async fn rejects_untrusted_links_and_provider_refusal() {
        let app = Router::new()
            .route(
                "/share/snap",
                get(|| async {
                    Json(json!({"state":true,"data":{"count":1,"list":[{"fid":"9"}]}}))
                }),
            )
            .route(
                "/share/receive",
                post(|| async { Json(json!({"state":false,"error":"UID=secret"})) }),
            );
        let account = fixture(app).await;
        assert!(
            check(&account, "https://115.com.attacker.test/s/abc", None)
                .await
                .is_err()
        );
        let err = save(&account, "https://115.com/s/abc", None, "0")
            .await
            .err()
            .unwrap();
        assert!(!err.1.contains("secret"));
    }
}
