use crate::AppError;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const MAX_URL: usize = 4096;
const MAX_DOCUMENT: usize = 2 * 1024 * 1024;
const MAX_TOTAL: usize = 8 * 1024 * 1024;
// Base58 conversion is quadratic. Keep its work bounded separately from JSON parsing.
const MAX_ENCODED_DOCUMENT: usize = 64 * 1024;
const MAX_DOCUMENTS: usize = 32;
const MAX_DEPTH: usize = 5;
const MAX_ENTRIES: usize = 4096;
const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub kind: String,
    pub name: String,
    pub url: String,
}

fn invalid() -> AppError {
    AppError::bad_request("Invalid or unsupported source subscription configuration")
}

fn limit() -> AppError {
    AppError::bad_request("Source subscription exceeds the import limits")
}

fn encoded_suffix(raw: &str) -> Option<&str> {
    let (prefix, payload) = raw.split_once(':')?;
    (prefix.eq_ignore_ascii_case("base58") || prefix.eq_ignore_ascii_case("b58"))
        .then(|| payload.strip_prefix("//").unwrap_or(payload))
}

fn decode_base58(raw: &str, max: usize) -> Result<String, AppError> {
    if raw.is_empty() || raw.len() > max {
        return Err(invalid());
    }
    let digits = raw.as_bytes();
    let zeros = digits.iter().take_while(|&&c| c == b'1').count();
    let mut words = Vec::<u32>::with_capacity(raw.len() / 5 + 1);
    // Five Base58 digits fit in 30 bits, so a multiply/carry fits in u64.
    for chunk in digits[zeros..].chunks(5) {
        let mut carry = 0u64;
        for c in chunk {
            let digit = ALPHABET.iter().position(|v| v == c).ok_or_else(invalid)?;
            carry = carry * 58 + digit as u64;
        }
        let radix = 58u64.pow(chunk.len() as u32);
        for word in &mut words {
            carry += u64::from(*word) * radix;
            *word = carry as u32;
            carry >>= 32;
        }
        if carry > 0 {
            words.push(carry as u32);
        }
    }
    let mut bytes = Vec::with_capacity(zeros + words.len() * 4);
    bytes.resize(zeros, 0);
    if let Some(high) = words.pop() {
        let high_bytes = high.to_be_bytes();
        bytes.extend_from_slice(&high_bytes[(high.leading_zeros() / 8) as usize..]);
        for word in words.into_iter().rev() {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
    }
    String::from_utf8(bytes).map_err(|_| invalid())
}

fn resolve(base: Option<&Url>, raw: &str) -> Result<Url, AppError> {
    if raw.is_empty()
        || raw.len() > MAX_URL
        || raw.starts_with('#')
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid());
    }
    if let Some((scheme, tail)) = raw.split_once(':')
        && (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
        && tail.strip_prefix("//").is_none_or(|s| s.starts_with('/'))
    {
        return Err(invalid());
    }
    let mut url = base
        .map_or_else(|| Url::parse(raw), |base| base.join(raw))
        .map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > MAX_URL
    {
        return Err(invalid());
    }
    url.set_fragment(None);
    Ok(url)
}

fn repository_url(base: Option<&Url>, raw: &str) -> Result<Url, AppError> {
    let raw = raw.trim();
    let decoded;
    let value = if let Some(encoded) = encoded_suffix(raw) {
        decoded = decode_base58(encoded, MAX_URL * 2)?;
        decoded.as_str()
    } else if !raw.contains(':') && base.is_none() {
        decoded = decode_base58(raw, MAX_URL * 2)?;
        decoded.as_str()
    } else if base.is_some()
        && raw.len() <= MAX_URL * 2
        && raw.bytes().all(|c| ALPHABET.contains(&c))
        && let Ok(decoded_url) = decode_base58(raw, MAX_URL * 2)
        && decoded_url.contains("://")
    {
        decoded = decoded_url;
        decoded.as_str()
    } else {
        raw
    };
    let url = resolve(base, value)?;
    if executable_path(&url) {
        return Err(invalid());
    }
    Ok(url)
}

pub(crate) fn validate_input(input: &str) -> Result<(), AppError> {
    repository_url(None, input).map(|_| ())
}

fn executable_path(url: &Url) -> bool {
    url.path().split('/').any(|segment| {
        segment.rsplit_once('.').is_some_and(|(_, ext)| {
            [
                "js", "mjs", "cjs", "py", "lua", "jar", "class", "dex", "apk", "so", "dll", "exe",
                "wasm", "sh",
            ]
            .iter()
            .any(|bad| ext.eq_ignore_ascii_case(bad))
        })
    })
}

fn xml_cms(url: &Url) -> bool {
    url.path().split('/').any(|segment| {
        segment.eq_ignore_ascii_case("xml")
            || segment
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("xml"))
    }) || url.query_pairs().any(|(key, value)| {
        ["at", "format", "type", "output"]
            .iter()
            .any(|k| key.eq_ignore_ascii_case(k))
            && value.eq_ignore_ascii_case("xml")
    })
}

fn text<'a>(row: &'a Value, field: &str) -> Option<&'a str> {
    row.get(field)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn tvbox_type(row: &Value) -> Result<u64, AppError> {
    let kind = &row["type"];
    kind.as_u64()
        .or_else(|| kind.as_str()?.parse().ok())
        .ok_or_else(invalid)
}

#[derive(Default)]
struct Import {
    candidates: Vec<Candidate>,
    known: HashSet<(&'static str, Url)>,
    entries: usize,
}

impl Import {
    fn count(&mut self, count: usize) -> Result<(), AppError> {
        self.entries += count;
        if self.entries > MAX_ENTRIES {
            Err(limit())
        } else {
            Ok(())
        }
    }

    fn candidate(
        &mut self,
        base: &Url,
        kind: &'static str,
        row: &Value,
        fallback: Option<&str>,
    ) -> Result<(), AppError> {
        let field = if kind == "cms" { "api" } else { "url" };
        let Some(raw) = text(row, field) else {
            // TVBox also permits inline groups of live channels.
            if kind == "live" && row.get("channels").is_some() {
                return Ok(());
            }
            return Err(invalid());
        };
        if raw.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("csp_"))
            || Url::parse(raw).is_ok_and(|url| !matches!(url.scheme(), "http" | "https"))
        {
            return Ok(());
        }
        let url = resolve(Some(base), raw)?;
        if executable_path(&url) || (kind == "cms" && xml_cms(&url)) {
            return Ok(());
        }
        let name = text(row, "name")
            .or(fallback)
            .or(url.host_str())
            .ok_or_else(invalid)?;
        if name.trim().is_empty()
            || name.chars().count() > 100
            || name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        if self.known.insert((kind, url.clone())) {
            self.candidates.push(Candidate {
                kind: kind.to_owned(),
                name: name.trim().to_owned(),
                url: url.into(),
            });
        }
        Ok(())
    }

    fn document(&mut self, base: &Url, input: &[u8]) -> Result<Vec<Url>, AppError> {
        let raw = std::str::from_utf8(input)
            .map_err(|_| invalid())?
            .trim()
            .trim_start_matches('\u{feff}')
            .trim_start();
        let decoded;
        let json = if raw.starts_with('{') {
            raw
        } else {
            decoded = decode_base58(encoded_suffix(raw).unwrap_or(raw), MAX_ENCODED_DOCUMENT)?;
            decoded.trim_start_matches('\u{feff}')
        };
        let root: Value = serde_json::from_str(json).map_err(|_| invalid())?;
        let root = root.as_object().ok_or_else(invalid)?;
        if !["api_site", "sites", "parses", "lives", "urls"]
            .iter()
            .any(|key| root.contains_key(*key))
        {
            return Err(invalid());
        }
        if let Some(sites) = root.get("api_site") {
            let sites = sites.as_object().ok_or_else(invalid)?;
            self.count(sites.len())?;
            for (key, row) in sites {
                self.candidate(base, "cms", row, Some(key))?;
            }
        }
        let mut children = Vec::new();
        for (key, kind) in [("sites", "cms"), ("parses", "parser"), ("lives", "live")] {
            let Some(rows) = root.get(key) else { continue };
            let rows = rows.as_array().ok_or_else(invalid)?;
            self.count(rows.len())?;
            for row in rows {
                if !row.is_object() {
                    return Err(invalid());
                }
                if kind == "live" || tvbox_type(row)? == 1 {
                    self.candidate(base, kind, row, text(row, "key"))?;
                }
            }
        }
        if let Some(rows) = root.get("urls") {
            let rows = rows.as_array().ok_or_else(invalid)?;
            self.count(rows.len())?;
            if rows.len() > MAX_DOCUMENTS {
                return Err(limit());
            }
            for row in rows {
                let raw = row
                    .as_str()
                    .or_else(|| text(row, "url"))
                    .ok_or_else(invalid)?;
                children.push(repository_url(Some(base), raw)?);
            }
        }
        Ok(children)
    }
}

async fn collect(client: &Client, root: Url) -> Result<Vec<Candidate>, AppError> {
    // A closing marker pops the current repository after all its children have been visited.
    let mut pending = vec![Some(root)];
    let mut ancestors = Vec::<(Url, Url)>::new();
    let mut seen = HashSet::new();
    let mut documents = 0;
    let mut total = 0;
    let mut import = Import::default();
    while let Some(next) = pending.pop() {
        let Some(url) = next else {
            ancestors.pop();
            continue;
        };
        let in_ancestry = |url: &Url| {
            ancestors
                .iter()
                .any(|(start, end)| url == start || url == end)
        };
        if in_ancestry(&url) {
            return Err(invalid());
        }
        if !seen.insert(url.clone()) {
            continue;
        }
        documents += 1;
        if documents > MAX_DOCUMENTS || ancestors.len() > MAX_DEPTH {
            return Err(limit());
        }
        // The shared client caps redirects at five. Relative URLs use the final location.
        let mut response = client
            .get(url.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| AppError::bad_request("Could not fetch source subscription"))?;
        if !response.status().is_success() {
            return Err(AppError::bad_request(format!(
                "Source subscription returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let base = resolve(None, response.url().as_str())?;
        if in_ancestry(&base) || executable_path(&base) {
            return Err(invalid());
        }
        if base != url && !seen.insert(base.clone()) {
            continue;
        }
        let remaining = MAX_DOCUMENT.min(MAX_TOTAL - total);
        if response
            .content_length()
            .is_some_and(|n| n > remaining as u64)
        {
            return Err(limit());
        }
        let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AppError::bad_request("Incomplete source subscription response"))?
        {
            if chunk.len() > remaining - bytes.len() {
                return Err(limit());
            }
            bytes.extend_from_slice(&chunk);
        }
        total += bytes.len();
        let children = import.document(&base, &bytes)?;
        if !children.is_empty() {
            ancestors.push((url, base));
            pending.push(None);
            pending.extend(children.into_iter().rev().map(Some));
        }
    }
    Ok(import.candidates)
}

pub(super) async fn preview(client: &Client, input: &str) -> Result<Vec<Candidate>, AppError> {
    let root = repository_url(None, input)?;
    tokio::time::timeout(Duration::from_secs(40), collect(client, root))
        .await
        .map_err(|_| AppError::bad_request("Source subscription preview timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, response::Redirect, routing::get};
    use serde_json::json;

    const ENCODED_URL: &str = "MvwLKznkcWQHFss1LeznTJoYbtrL6f5vD6FCC11bj";
    const ENCODED_DOCUMENT: &str =
        "22Ma5PpyhWvXjyhCt9Tx4v2sfWFAvpZJukqRrvDcdPeEAR16ewhC3XUEVvph3W4d2";

    fn base() -> Url {
        Url::parse("https://example.test/config/main.json").unwrap()
    }

    #[test]
    fn validates_plain_and_base58_configuration_links() {
        for input in [
            ENCODED_URL.to_owned(),
            format!("base58://{ENCODED_URL}"),
            format!("base58:{ENCODED_URL}"),
            "https://example.test/feed.json".to_owned(),
        ] {
            validate_input(&input).unwrap();
            assert_eq!(
                repository_url(None, &input).unwrap().as_str(),
                "https://example.test/feed.json"
            );
        }
        for invalid in [
            "ftp://example.test/config",
            "https:bad",
            "file:///config",
            "base58://0OIl",
            "relative.json",
            "https://user:pass@example.test/",
            "https://example.test/a b",
            "https://example.test/script.JS",
            "",
            "base58:1",
        ] {
            assert!(validate_input(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn imports_only_data_endpoints_and_deduplicates_within_kind() {
        let doc = json!({
            "api_site": {"luna": {"api": "../vod#unused", "name": " Luna "}},
            "spider": "https://example.test/ignored.jar",
            "sites": [
                {"type": 1, "api": "/vod", "name": "duplicate"},
                {"type": "1", "api": "cms.php?ac=list", "key": "CMS"},
                {"type": 0, "api": "xml.php"}, {"type": 3, "api": "csp_Demo"},
                {"type": 1, "api": "csp_Demo"}, {"type": 1, "api": "a.JS"},
                {"type": 1, "api": "https://example.test/api/at/XML/"},
                {"type": 1, "api": "catalog.php?at=xml"}, {"type": 1, "api": "data.xml"}
            ],
            "parses": [
                {"type": 0, "url": "/html?url="}, {"type": 1, "name": "JSON", "url": "/json?url="},
                {"type": 1, "url": "javascript:test()"}
            ],
            "lives": [
                {"type": 0, "url": "//live.test/channels.m3u", "name": "Live"},
                {"channels": []}, {"url": "file:///playlist"}
            ]
        });
        let mut import = Import::default();
        assert!(
            import
                .document(&base(), doc.to_string().as_bytes())
                .unwrap()
                .is_empty()
        );
        let rows: Vec<_> = import
            .candidates
            .iter()
            .map(|c| (c.kind.as_str(), c.name.as_str(), c.url.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("cms", "Luna", "https://example.test/vod"),
                ("cms", "CMS", "https://example.test/config/cms.php?ac=list"),
                ("parser", "JSON", "https://example.test/json?url="),
                ("live", "Live", "https://live.test/channels.m3u"),
            ]
        );
    }

    #[test]
    fn rejects_broken_documents_and_limits_entry_counts() {
        for input in [
            "<xml/>",
            "{}",
            r#"{"sites":null}"#,
            r#"{"urls":[{}]}"#,
            r#"{"sites":[{"type":1}]}"#,
            r#"{"urls":["file:///bad"]}"#,
            "0OOO",
        ] {
            assert!(
                Import::default()
                    .document(&base(), input.as_bytes())
                    .is_err()
            );
        }
        let oversized = json!({"sites": vec![json!({"type":3}); MAX_ENTRIES + 1]}).to_string();
        assert!(
            Import::default()
                .document(&base(), oversized.as_bytes())
                .is_err()
        );
        let children = json!({"urls": [ENCODED_URL, {"url": "b58:6avejyC9fhaRYZ"}]});
        let urls = Import::default()
            .document(&base(), children.to_string().as_bytes())
            .unwrap();
        assert_eq!(urls[0].as_str(), "https://example.test/feed.json");
        assert_eq!(urls[1].as_str(), "https://example.test/config/child.json");
    }

    #[tokio::test]
    async fn traverses_relative_repositories_and_rejects_partial_results() {
        let app = Router::new()
            .route("/start", get(|| async { Redirect::temporary("/d/index") }))
            .route(
                "/d/index",
                get(|| async { r#"{"urls":[{"url":"child"},"again"]}"# }),
            )
            .route("/d/child", get(|| async { ENCODED_DOCUMENT }))
            .route("/d/again", get(|| async { r#"{"urls":["child"]}"# }))
            .route(
                "/broken",
                get(|| async { r#"{"urls":["d/child","unavailable"]}"# }),
            )
            .route("/unavailable", get(|| async { StatusCode::BAD_GATEWAY }))
            .route(
                "/cycle",
                get(|| async { r#"{"urls":["./cycle#fragment"]}"# }),
            )
            .route("/loop", get(|| async { Redirect::temporary("/loop") }))
            .route("/oversize", get(|| async { vec![b' '; MAX_DOCUMENT + 1] }))
            .route(
                "/depth/{n}",
                get(
                    |axum::extract::Path(n): axum::extract::Path<usize>| async move {
                        json!({"urls": [(n + 1).to_string()]}).to_string()
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap();
        let rows = preview(&client, &format!("{origin}/start")).await.unwrap();
        assert_eq!(
            rows,
            [Candidate {
                kind: "cms".into(),
                name: "CMS".into(),
                url: format!("{origin}/d/vod")
            }]
        );
        for path in ["broken", "cycle", "loop", "oversize", "depth/0"] {
            assert!(
                preview(&client, &format!("{origin}/{path}")).await.is_err(),
                "{path}"
            );
        }
        server.abort();
    }
}
