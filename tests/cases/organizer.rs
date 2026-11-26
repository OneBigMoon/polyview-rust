use super::*;
use polyview::organizer::{detect_filename, on_completion};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;

const SETTINGS: &str = "/api/settings/organizer";
const HISTORY: &str = "/api/organizer/history";
const RELEASE: &str = "downloads/三体.2023.S02E03.2160p.WEB-DL.H265.mkv";
const TARGET: &str = "TV/三体 (2023)/Season 02/三体 S02E03.mkv";
const BYTES: &[u8] = b"fixture media content";

async fn seed(app: &TestApp, admin: &str) -> i64 {
    fs::write(app.state.media_dir.join(RELEASE), BYTES).unwrap();
    assert_eq!(
        app.request("POST", "/api/library/scan", Some(admin), None)
            .await
            .status,
        StatusCode::OK
    );
    app.state
        .conn()
        .unwrap()
        .query_row(
            "SELECT id FROM library_media WHERE relative_path=?",
            [RELEASE],
            |r| r.get(0),
        )
        .unwrap()
}

async fn act(app: &TestApp, admin: &str, path: &str) -> Value {
    let r = app.request("POST", path, Some(admin), None).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    r.json()
}

#[test]
fn recognizes_unicode_scene_titles_seasons_years_resolution_and_codecs() {
    for (file, title, year, season, episode, resolution, codec) in [
        (
            RELEASE,
            "三体",
            Some(2023),
            Some(2),
            Some(3),
            Some("2160p"),
            Some("HEVC"),
        ),
        (
            "[PT] A.Dotted.Title.1999.BluRay.1080p.x264-GRP.mp4",
            "A Dotted Title",
            Some(1999),
            None,
            None,
            Some("1080p"),
            Some("H.264"),
        ),
        (
            "北海第十二集.720p.AV1.MKV",
            "北海",
            None,
            Some(1),
            Some(12),
            Some("720p"),
            Some("AV1"),
        ),
        (
            "三体 第003集.mkv",
            "三体",
            None,
            Some(1),
            Some(3),
            None,
            None,
        ),
        (
            "Show/Season 03/EP17.mkv",
            "Show",
            None,
            Some(3),
            Some(17),
            None,
            None,
        ),
        ("1917.2019.mkv", "1917", Some(2019), None, None, None, None),
    ] {
        let f = detect_filename(Path::new(file));
        assert_eq!(
            (
                f.title.as_str(),
                f.year,
                f.season,
                f.episode,
                f.resolution.as_deref(),
                f.codec.as_deref()
            ),
            (title, year, season, episode, resolution, codec),
            "{file}"
        );
        assert_eq!(
            f.ext,
            Path::new(file)
                .extension()
                .unwrap()
                .to_str()
                .unwrap()
                .to_ascii_lowercase()
        );
    }
}

#[tokio::test]
async fn hardlink_rescan_and_rollback_keep_identity_contents_and_user_progress() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = seed(&app, &admin).await;
    let action = format!("/api/library/{id}/organize");
    let progress = app
        .request(
            "PUT",
            &format!("/api/library/{id}/progress"),
            Some(&admin),
            Some(json!({"position":17,"duration":100,"watched":false})),
        )
        .await;
    assert_eq!(progress.status, StatusCode::OK);
    let rec = act(&app, &admin, &action).await;
    assert_eq!(rec["target_path"], TARGET);
    assert_eq!(rec["status"], "completed");
    assert_eq!(
        fs::metadata(app.state.media_dir.join(RELEASE))
            .unwrap()
            .ino(),
        fs::metadata(app.state.media_dir.join(TARGET))
            .unwrap()
            .ino()
    );
    assert_eq!(act(&app, &admin, &action).await, rec);
    act(&app, &admin, "/api/library/scan").await;
    let rows = app
        .request("GET", "/api/library", Some(&admin), None)
        .await
        .json();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], id);
    assert_eq!(rows[0]["position"], 17.0);
    let undo = format!("/api/organizer/{}/rollback", rec["id"]);
    assert_eq!(act(&app, &admin, &undo).await["status"], "rolled_back");
    assert_eq!(act(&app, &admin, &undo).await["status"], "rolled_back");
    assert!(!app.state.media_dir.join(TARGET).exists());
    assert_eq!(fs::read(app.state.media_dir.join(RELEASE)).unwrap(), BYTES);
    let after = app
        .request("GET", "/api/library", Some(&admin), None)
        .await
        .json();
    assert_eq!(after[0]["id"], id);
    assert_eq!(after[0]["relative_path"], RELEASE);
    assert_eq!(after[0]["position"], 17.0);
}

#[tokio::test]
async fn interrupted_move_and_rollback_resume_after_filesystem_commit() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = seed(&app, &admin).await;
    on_completion(&app.state, id, None, None).await.unwrap();
    assert!(
        app.request("GET", HISTORY, Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        app.request(
            "PUT",
            SETTINGS,
            Some(&admin),
            Some(json!({"auto_organize":true,"mode":"move"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let fail_commit = "CREATE TRIGGER organizer_crash BEFORE UPDATE OF relative_path ON library_media BEGIN SELECT RAISE(ABORT,'injected commit failure'); END;";
    app.state
        .conn()
        .unwrap()
        .execute_batch(fail_commit)
        .unwrap();
    assert!(
        on_completion(&app.state, id, Some("三体"), Some(2023))
            .await
            .is_err()
    );
    let first = app.request("GET", HISTORY, Some(&admin), None).await.json();
    assert_eq!(first[0]["status"], "pending");
    assert!(!app.state.media_dir.join(RELEASE).exists());
    assert_eq!(fs::read(app.state.media_dir.join(TARGET)).unwrap(), BYTES);
    app.state
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER organizer_crash")
        .unwrap();
    on_completion(&app.state, id, None, None).await.unwrap();
    let undo = format!("/api/organizer/{}/rollback", first[0]["id"]);
    app.state
        .conn()
        .unwrap()
        .execute_batch(fail_commit)
        .unwrap();
    assert_eq!(
        app.request("POST", &undo, Some(&admin), None).await.status,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(!app.state.media_dir.join(TARGET).exists());
    assert_eq!(fs::read(app.state.media_dir.join(RELEASE)).unwrap(), BYTES);
    app.state
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER organizer_crash")
        .unwrap();
    assert_eq!(act(&app, &admin, &undo).await["status"], "rolled_back");
    on_completion(&app.state, id, None, None).await.unwrap();
    assert_eq!(
        app.request("GET", HISTORY, Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn custom_movie_move_never_overwrites_a_recreated_source() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let source = app.state.media_dir.join("Dotted.Movie.1998.720p.x264.mkv");
    fs::write(&source, BYTES).unwrap();
    act(&app, &admin, "/api/library/scan").await;
    let id = app
        .request("GET", "/api/library", Some(&admin), None)
        .await
        .json()[0]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        app.request(
            "PUT",
            SETTINGS,
            Some(&admin),
            Some(json!({"mode":"move","movie_template":"Films/{year}/{title}.{resolution}.{ext}"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let rec = act(&app, &admin, &format!("/api/library/{id}/organize")).await;
    assert_eq!(rec["target_path"], "Films/1998/Dotted Movie.720p.mkv");
    assert!(!source.exists());
    fs::write(&source, b"unrelated replacement").unwrap();
    let undo = format!("/api/organizer/{}/rollback", rec["id"]);
    assert_eq!(
        app.request("POST", &undo, Some(&admin), None).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(&source).unwrap(), b"unrelated replacement");
    let user_copy = source.with_extension("reserved");
    fs::rename(&source, &user_copy).unwrap();
    act(&app, &admin, &undo).await;
    assert_eq!(fs::read(source).unwrap(), BYTES);
    assert_eq!(fs::read(user_copy).unwrap(), b"unrelated replacement");
}

#[tokio::test]
async fn same_size_replacements_collisions_and_changed_hardlinks_are_never_removed() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = seed(&app, &admin).await;
    let target = app.state.media_dir.join(TARGET);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, BYTES).unwrap();
    let organize = format!("/api/library/{id}/organize");
    assert_eq!(
        app.request("POST", &organize, Some(&admin), None)
            .await
            .status,
        StatusCode::CONFLICT
    );
    fs::remove_file(&target).unwrap();
    let rec = act(&app, &admin, &organize).await;
    let undo = format!("/api/organizer/{}/rollback", rec["id"]);
    fs::write(&target, b"other in-place content").unwrap();
    assert_eq!(
        app.request("POST", &undo, Some(&admin), None).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(&target).unwrap(), b"other in-place content");
    fs::remove_file(&target).unwrap();
    fs::write(&target, BYTES).unwrap();
    assert_eq!(
        app.request("POST", &undo, Some(&admin), None).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(&target).unwrap(), BYTES);
}

#[tokio::test]
async fn admin_scope_templates_and_parent_symlinks_protect_external_files() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = seed(&app, &admin).await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let action = format!("/api/library/{id}/organize");
    for (method, path, body) in [
        ("GET", SETTINGS, None),
        ("PUT", SETTINGS, Some(json!({}))),
        ("GET", HISTORY, None),
        ("POST", &action, None),
        ("POST", "/api/organizer/1/rollback", None),
    ] {
        assert_eq!(
            app.request(method, path, Some(&viewer), body).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request_with_headers("POST", &action, Some(&admin), None, &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for template in [
        "../{title}.{ext}",
        "/outside/{title}.{ext}",
        "{title}/{unexpected}.{ext}",
    ] {
        assert_eq!(
            app.request(
                "PUT",
                SETTINGS,
                Some(&admin),
                Some(json!({"episode_template":template}))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), app.state.media_dir.join("TV")).unwrap();
    assert_eq!(
        app.request("POST", &action, Some(&admin), None)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::remove_file(app.state.media_dir.join("TV")).unwrap();
    let rec = act(&app, &admin, &action).await;
    let target = app.state.media_dir.join(TARGET);
    fs::rename(target.parent().unwrap(), outside.path().join("real")).unwrap();
    let outside_file = outside
        .path()
        .join("real")
        .join(target.file_name().unwrap());
    symlink(outside.path().join("real"), target.parent().unwrap()).unwrap();
    let undo = format!("/api/organizer/{}/rollback", rec["id"]);
    assert_eq!(
        app.request("POST", &undo, Some(&admin), None).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(outside_file).unwrap(), BYTES);
}

#[tokio::test]
async fn typed_organizer_backup_restores_on_fresh_instance_and_rejects_traversal_atomically() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let config = app.request("PUT",SETTINGS,Some(&admin),Some(json!({"auto_organize":true,"mode":"move","movie_template":"Films/{year}/{title}.{ext}"}))).await.json();
    let mut backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(backup["automation"]["organizer"], config);
    let fresh = TestApp::new();
    let root = fresh.setup().await;
    assert_eq!(
        fresh
            .request(
                "POST",
                "/api/settings/import",
                Some(&root),
                Some(backup.clone())
            )
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        fresh
            .request("GET", SETTINGS, Some(&root), None)
            .await
            .json(),
        config
    );
    backup["app_settings"]["play"]["play_ua"] = json!("must roll back");
    backup["automation"]["organizer"]["movie_template"] = json!("../{title}.{ext}");
    assert_eq!(
        fresh
            .request("POST", "/api/settings/import", Some(&root), Some(backup))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fresh
            .request("GET", SETTINGS, Some(&root), None)
            .await
            .json(),
        config
    );
    assert_ne!(
        fresh
            .request("GET", "/api/settings/play", Some(&root), None)
            .await
            .json()["play_ua"],
        "must roll back"
    );
}
