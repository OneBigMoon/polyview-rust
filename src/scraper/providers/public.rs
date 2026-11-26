use super::{
    body, endpoint, items, json, no_match, primary, public_image, text, unavailable, year,
};
use crate::{
    AppError,
    scraper::{Credit, Episode, Metadata, Prepared, Season},
};
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::{Value, json as json_value};

fn positive_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| *n > 0)
}

pub(super) async fn bangumi(
    client: &Client,
    base: &str,
    title: &str,
    requested_year: &str,
    season: Option<i64>,
) -> Result<Prepared, AppError> {
    let mut filter = json_value!({"type":[2],"nsfw":false});
    if !requested_year.is_empty() {
        filter["air_date"] = json_value!([
            format!(">={requested_year}-01-01"),
            format!("<={requested_year}-12-31")
        ]);
    }
    let results = json(
        client
            .post(endpoint(base, "v0/search/subjects"))
            .query(&[("limit", "10")])
            .header("content-type", "application/json")
            .body(json_value!({"keyword":title,"filter":filter}).to_string()),
    )
    .await?;
    let candidates = &results["data"];
    let subject = items(candidates)
        .find(|v| {
            ["name", "name_cn"].iter().any(|key| {
                v[*key]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(title))
            })
        })
        .or_else(|| items(candidates).next())
        .ok_or_else(no_match)?;
    let id = positive_id(&subject["id"]).ok_or_else(no_match)?;
    let detail_url = endpoint(base, &format!("v0/subjects/{id}"));
    let (subject, characters, persons) = tokio::join!(
        json(client.get(&detail_url)),
        json(client.get(format!("{detail_url}/characters"))),
        json(client.get(format!("{detail_url}/persons")))
    );
    let subject = subject?;
    let name = primary(&subject, &["name_cn", "name"]);
    if name.is_empty() {
        return Err(no_match());
    }
    let mut cast = Vec::new();
    if let Ok(characters) = characters {
        for character in items(&characters) {
            for actor in items(&character["actors"]) {
                let name = text(actor, "name");
                if !name.is_empty() {
                    cast.push(Credit {
                        name,
                        role: text(character, "name"),
                    });
                }
            }
        }
    }
    let people = persons.unwrap_or(Value::Null);
    let crew = items(&people)
        .filter_map(|p| {
            let name = text(p, "name");
            (!name.is_empty()).then(|| Credit {
                name,
                role: text(p, "relation"),
            })
        })
        .collect();
    let mut seasons = Vec::new();
    if let Some(number) = season {
        let mut offset = 0;
        let mut episodes = Vec::new();
        loop {
            let page = json(client.get(endpoint(base, "v0/episodes")).query(&[
                ("subject_id", id),
                ("type", 0),
                ("limit", 100),
                ("offset", offset),
            ]))
            .await?;
            let rows = page["data"].as_array().ok_or_else(unavailable)?;
            episodes.extend(rows.iter().filter_map(|v| {
                let index = v["ep"].as_f64().or_else(|| v["sort"].as_f64())?;
                (index >= 0. && index.fract() == 0.).then(|| Episode {
                    number: index as i64,
                    title: primary(v, &["name_cn", "name"]),
                    description: text(v, "desc"),
                })
            }));
            offset += rows.len() as i64;
            if rows.is_empty()
                || page["total"]
                    .as_i64()
                    .map_or(rows.len() < 100, |total| offset >= total)
            {
                break;
            }
            if offset >= 10_000 {
                return Err(unavailable());
            }
        }
        seasons.push(Season {
            number,
            title: name.clone(),
            episodes,
        });
    }
    let image = primary(&subject["images"], &["large", "common", "medium"]);
    Ok(Prepared {
        metadata: Metadata {
            provider: "bangumi".into(),
            provider_id: id.to_string(),
            title: name,
            original_title: text(&subject, "name"),
            year: year(&text(&subject, "date")),
            rating: subject["rating"]["score"].as_f64(),
            genres: items(&subject["tags"])
                .take(12)
                .map(|tag| text(tag, "name"))
                .filter(|s| !s.is_empty())
                .collect(),
            description: text(&subject, "summary"),
            cast,
            crew,
            seasons,
            ..Default::default()
        },
        poster_url: public_image(base, &image, &["bgm.tv", "bangumi.tv", "bgm38.tv"]),
        backdrop_url: None,
    })
}

fn strings(value: &Value) -> Vec<String> {
    if let Some(s) = value.as_str() {
        vec![s.to_owned()]
    } else {
        items(value)
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect()
    }
}
fn douban_people(value: &Value, role: &str) -> Vec<Credit> {
    let people = items(value).chain(value.is_object().then_some(value));
    people
        .filter_map(|p| {
            let name = text(p, "name");
            (!name.is_empty()).then(|| Credit {
                name,
                role: role.into(),
            })
        })
        .collect()
}

pub(super) async fn douban(
    client: &Client,
    base: &str,
    title: &str,
    requested_year: &str,
) -> Result<Prepared, AppError> {
    let results = json(
        client
            .get(endpoint(base, "j/subject_suggest"))
            .query(&[("q", title)])
            .header("referer", base),
    )
    .await?;
    let candidates = || {
        items(&results).filter(|v| {
            positive_id(&v["id"]).is_some()
                && matches!(v["type"].as_str(), None | Some("movie" | "tv"))
                && (requested_year.is_empty() || text(v, "year") == requested_year)
        })
    };
    let selected = candidates()
        .find(|v| {
            v["title"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(title))
        })
        .or_else(|| candidates().next())
        .ok_or_else(no_match)?;
    let id = positive_id(&selected["id"]).ok_or_else(no_match)?;
    let html = body(
        client
            .get(endpoint(base, &format!("subject/{id}/")))
            .header("referer", base),
        4 * 1024 * 1024,
    )
    .await?;
    let doc = Html::parse_document(std::str::from_utf8(&html).map_err(|_| unavailable())?);
    let scripts = Selector::parse("script[type='application/ld+json']").expect("static selector");
    let detail = doc
        .select(&scripts)
        .filter_map(|s| serde_json::from_str::<Value>(&s.text().collect::<String>()).ok())
        .find(|v| v["name"].is_string())
        .ok_or_else(unavailable)?;
    let summary = Selector::parse("[property='v:summary']").expect("static selector");
    let description = doc
        .select(&summary)
        .next()
        .map(|s| s.text().collect::<String>().trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| text(&detail, "description"));
    let published = year(&text(&detail, "datePublished"));
    let image = detail["image"]
        .as_str()
        .or_else(|| detail["image"]["url"].as_str())
        .or_else(|| selected["img"].as_str())
        .unwrap_or_default();
    let score = &detail["aggregateRating"]["ratingValue"];
    let mut crew = douban_people(&detail["director"], "Director");
    crew.extend(douban_people(&detail["author"], "Writer"));
    Ok(Prepared {
        metadata: Metadata {
            provider: "douban".into(),
            provider_id: id.to_string(),
            title: primary(selected, &["title", "name"]),
            original_title: primary(selected, &["sub_title", "original_title"]),
            year: if published.is_empty() {
                text(selected, "year")
            } else {
                published
            },
            rating: score.as_f64().or_else(|| score.as_str()?.parse().ok()),
            genres: strings(&detail["genre"]),
            description,
            cast: douban_people(&detail["actor"], ""),
            crew,
            ..Default::default()
        },
        poster_url: public_image(base, image, &["doubanio.com", "douban.com"]),
        backdrop_url: None,
    })
}
