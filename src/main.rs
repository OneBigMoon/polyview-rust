use polyview::{AppState, Shared, init_schema, network, router};
use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "PolyView — web viewing, series downloads, and TVBox / 影视仓 feeds\n\nConfiguration via environment:\n  POLYVIEW_BIND          Listen address (default 127.0.0.1:8080)\n  POLYVIEW_DATA_DIR      New database/report directory (default ./data)\n  POLYVIEW_MEDIA_DIR     Media library directory (default ./media)\n  POLYVIEW_PUBLIC_URL    HTTP(S) origin reachable by TV clients; no path\n  POLYVIEW_SECURE_COOKIE Set true behind HTTPS (default false)\n  POLYVIEW_LEGACY_DB     Explicit original database backup to import read-only\n  POLYVIEW_LEGACY_CONFIG Optional original JSON config used with LEGACY_DB\n  RUST_LOG              Log filter (default polyview=info)\n\nOpen the web interface to create the first administrator or use imported accounts.\nConfigure CMS sources for online viewing and tracking. FFmpeg is required for HLS\ndownloads; completed files are saved under the media directory.\n\nLegacy import uses a separate polyview.sqlite3 database and writes an import\nreport. Imported automatic downloads stay off. Old jobs, history, sessions, and\nTelegram execution are not migrated; retain the original service until verified."
        );
        return Ok(());
    }
    if env::args().len() > 1 {
        return Err("Unknown argument; use --help".into());
    }
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "polyview=info".into()))
        .init();
    let bind: SocketAddr = env::var("POLYVIEW_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()?;
    let data_dir = PathBuf::from(env::var_os("POLYVIEW_DATA_DIR").unwrap_or_else(|| "data".into()));
    let media_dir =
        PathBuf::from(env::var_os("POLYVIEW_MEDIA_DIR").unwrap_or_else(|| "media".into()));
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&media_dir)?;
    let media_dir = std::fs::canonicalize(media_dir)?;
    let download_dir = media_dir.join("downloads");
    std::fs::create_dir_all(&download_dir)?;
    let download_dir = std::fs::canonicalize(download_dir)?;
    if !download_dir.starts_with(&media_dir) {
        return Err("Download directory must remain inside the media library".into());
    }
    let conn = rusqlite::Connection::open(data_dir.join("polyview.sqlite3"))?;
    init_schema(&conn).map_err(|e| e.1)?;
    let secure_cookie = match env::var("POLYVIEW_SECURE_COOKIE").as_deref() {
        Ok("true" | "1") => true,
        Ok("false" | "0") | Err(_) => false,
        _ => return Err("POLYVIEW_SECURE_COOKIE must be true or false".into()),
    };
    let state: Shared = Arc::new(AppState {
        db: Mutex::new(conn),
        media_dir,
        download_dir,
        secure_cookie,
        client: reqwest::Client::builder()
            // Public IPTV gateways commonly serve HLS only to mobile players.
            .user_agent(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
            )
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?,
    });
    let legacy_path = env::var_os("POLYVIEW_LEGACY_DB").filter(|path| !path.is_empty());
    let import_state = state.clone();
    let report = tokio::task::spawn_blocking(move || {
        let Some(path) = legacy_path else {
            return polyview::legacy::supplement_legacy(&import_state);
        };
        let db_path = PathBuf::from(path);
        let config_path = env::var_os("POLYVIEW_LEGACY_CONFIG")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        polyview::legacy::import_legacy(&import_state, &db_path, config_path.as_deref()).map(Some)
    })
    .await?
    .map_err(|error| error.1)?;
    if let Some(report) = report {
        let report_path = data_dir.join("legacy-import-report.json");
        std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&report_path, std::fs::Permissions::from_mode(0o600))?;
        }
        tracing::info!(
            users = report.users_imported,
            sources = report.sources_imported,
            subscriptions = report.subscriptions_imported,
            held_automatic_downloads = report.auto_downloads_held,
            already_imported = report.already_imported,
            "Legacy import completed; details saved in data directory"
        );
    }
    let scan_state = state.clone();
    let scan =
        tokio::task::spawn_blocking(move || polyview::library::scan_library(&scan_state)).await?;
    if let Err(error) = scan {
        tracing::warn!(error = %error.1, "Initial scan failed; retry from the library page");
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(address = %listener.local_addr()?, "PolyView is ready");
    let worker = network::start_worker(state.clone());
    let tracking_worker = polyview::tracking::start_worker(state.clone());
    let live_worker = polyview::live_sources::start_worker(state.clone());
    let result = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    worker.abort();
    tracking_worker.abort();
    live_worker.abort();
    result?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
