use super::*;

async fn ok(app: &TestApp, cookie: &str, method: &str, uri: &str, body: Option<Value>) -> Value {
    let reply = app.request(method, uri, Some(cookie), body).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{method} {uri}: {}",
        String::from_utf8_lossy(&reply.body)
    );
    reply.json()
}

#[tokio::test]
async fn merged_progress_respects_completion_visibility_and_direct_lookup() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    app.state.conn().unwrap().execute_batch("INSERT INTO catalog_sources(id,name,url) VALUES (101,'Offline','https://example.invalid');
        INSERT INTO library_media(id,title,relative_path,root,kind,size) VALUES(12,'Local','local.mp4','media','movie',0);
        INSERT INTO library_user_media(user_id,media_id,position,duration,updated_at) VALUES(1,12,3,100,1);").unwrap();
    let mut p = json!({"source_id":101,"media_id":"a","title":"Show","episode_index":2,"episode_name":"Episode 3","remote_url":"https://example.invalid/3.mp4","position":17.0,"duration":100.0});
    ok(&app, &cookie, "PUT", "/api/progress", Some(p.clone())).await;
    let list = ok(&app, &cookie, "GET", "/api/progress", None).await;
    assert_eq!(list[0]["position"], 17.0);
    assert_eq!(list[0]["remote_url"], p["remote_url"]);
    assert_eq!(list[1]["local_id"], 12);
    p["position"] = json!(95.0);
    ok(&app, &cookie, "PUT", "/api/progress", Some(p.clone())).await;
    assert_eq!(
        ok(&app, &cookie, "GET", "/api/progress", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    app.state.conn().unwrap().execute_batch("INSERT INTO tracking_subscriptions(user_id,source_id,media_id,title) VALUES(1,101,'a','Show')").unwrap();
    assert_eq!(
        ok(&app, &cookie, "GET", "/api/progress", None).await[0]["media_id"],
        "a"
    );
    app.state.conn().unwrap().execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<24) INSERT INTO online_progress(user_id,source_id,media_id,title,position,duration,updated_at) SELECT 1,101,i,'Other',1,2,4102444800 FROM n").unwrap();
    let list = ok(&app, &cookie, "GET", "/api/progress", None).await;
    assert_eq!(list.as_array().unwrap().len(), 24);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|item| item["media_id"] != "a")
    );
    let exact = "/api/progress?source_id=101&media_id=a";
    assert_eq!(
        ok(&app, &cookie, "GET", exact, None).await[0]["episode_name"],
        "Episode 3"
    );
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET adult=1 WHERE id=101", [])
        .unwrap();
    assert_eq!(
        app.request("GET", exact, Some(&cookie), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.request("PUT", "/api/progress", Some(&cookie), Some(p.clone()))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET adult=0 WHERE id=101", [])
        .unwrap();
    p["duration"] = json!(0);
    assert_eq!(
        app.request("PUT", "/api/progress", Some(&cookie), Some(p))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    ok(&app, &cookie, "DELETE", "/api/progress/online/101/a", None).await;
    assert_eq!(ok(&app, &cookie, "GET", exact, None).await, json!([]));
    ok(&app, &cookie, "DELETE", "/api/progress/local/12", None).await;
    assert_eq!(
        app.state
            .conn()
            .unwrap()
            .query_row(
                "SELECT position FROM library_user_media WHERE media_id=12",
                [],
                |r| r.get::<_, f64>(0)
            )
            .unwrap(),
        0.0
    );
}

#[tokio::test]
async fn favorites_hydrate_channels_and_filter_by_user_and_source() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    app.create_viewer(&cookie).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    app.state.conn().unwrap().execute_batch("INSERT INTO catalog_sources(id,name,url) VALUES (101,'Offline','https://example.invalid');
        INSERT INTO channels(id,name,group_name,logo,url) VALUES(5,'TV','','','https://example.invalid/tv');
        INSERT INTO library_media(id,title,relative_path,root,kind,size) VALUES(12,'Local','local.mp4','media','movie',0);").unwrap();
    let f = json!({"source_id":101,"media_id":"movie","title":"Movie"});
    for _ in 0..2 {
        ok(&app, &cookie, "POST", "/api/favorites", Some(f.clone())).await;
    }
    ok(
        &app,
        &cookie,
        "PUT",
        "/api/channels/5/favorite",
        Some(json!({"favorite":true})),
    )
    .await;
    ok(
        &app,
        &cookie,
        "PUT",
        "/api/library/12/favorite",
        Some(json!({"favorite":true})),
    )
    .await;
    let list = ok(&app, &cookie, "GET", "/api/favorites", None).await;
    assert_eq!(list["catalog"].as_array().unwrap().len(), 1);
    assert_eq!(list["local"][0]["title"], "Local");
    assert_eq!(
        list["channels"][0]["sources"][0]["url"],
        "https://example.invalid/tv"
    );
    assert_eq!(
        ok(&app, &viewer, "GET", "/api/favorites", None).await,
        json!({"catalog":[],"local":[],"channels":[]})
    );
    assert_eq!(
        ok(&app, &cookie, "GET", "/api/channels", None).await[0]["favorite"],
        true
    );
    assert_eq!(
        ok(&app, &viewer, "GET", "/api/channels", None).await[0]["favorite"],
        false
    );
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET adult=1 WHERE id=101", [])
        .unwrap();
    assert_eq!(
        ok(&app, &cookie, "GET", "/api/favorites", None).await["catalog"],
        json!([])
    );
    assert_eq!(
        app.request("POST", "/api/favorites", Some(&cookie), Some(f))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE settings SET value='1' WHERE key='adult_sources_enabled'",
            [],
        )
        .unwrap();
    assert_eq!(
        ok(&app, &cookie, "GET", "/api/favorites", None).await["catalog"][0]["media_id"],
        "movie"
    );
    ok(&app, &cookie, "DELETE", "/api/favorites/101/movie", None).await;
    ok(
        &app,
        &cookie,
        "PUT",
        "/api/channels/5/favorite",
        Some(json!({"favorite":false})),
    )
    .await;
    let list = ok(&app, &cookie, "GET", "/api/favorites", None).await;
    assert_eq!(list["catalog"], json!([]));
    assert_eq!(list["channels"], json!([]));
}

#[tokio::test]
async fn history_refresh_evict_and_delete_are_per_user() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    app.create_viewer(&cookie).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let path = "/api/search/history";
    ok(
        &app,
        &viewer,
        "POST",
        path,
        Some(json!({"keyword":"private"})),
    )
    .await;
    for i in 0..20 {
        ok(
            &app,
            &cookie,
            "POST",
            path,
            Some(json!({"keyword":format!("k{i}")})),
        )
        .await;
    }
    for word in ["k0", "new"] {
        ok(&app, &cookie, "POST", path, Some(json!({"keyword":word}))).await;
    }
    let list = ok(&app, &cookie, "GET", path, None).await;
    assert_eq!(list.as_array().unwrap().len(), 20);
    assert_eq!(list[0], "new");
    assert_eq!(list[1], "k0");
    assert!(!list.as_array().unwrap().contains(&json!("k1")));
    ok(&app, &cookie, "DELETE", "/api/search/history?q=k0", None).await;
    assert_eq!(
        ok(&app, &cookie, "GET", path, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        19
    );
    ok(&app, &cookie, "DELETE", path, None).await;
    assert_eq!(ok(&app, &cookie, "GET", path, None).await, json!([]));
    assert_eq!(
        ok(&app, &viewer, "GET", path, None).await,
        json!(["private"])
    );
}
