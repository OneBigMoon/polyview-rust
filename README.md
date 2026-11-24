# PolyView

PolyView is a self-hosted media hub for a NAS: browse and watch from a desktop web browser, track ongoing series, download episodes into your library, and provide a subscription feed to a TVBox / 影视仓 client.

## Included

- Online catalog browsing, multi-source search, title details, playback lines, and episode selection using MacCMS-compatible JSON APIs.
- Administrator-managed online sources: add, edit, enable, disable, and remove sources.
- Same-origin authenticated playback proxy for HTTP(S) video and HLS, including nested playlists, segments, keys, and byte-range requests.
- Online series subscriptions with watched-episode counts, manual checks, and checks every ten minutes. Automatic downloads are optional and deduplicate episodes already queued for that subscription.
- Direct video downloads and FFmpeg HLS-to-MP4 downloads, with queue progress, cancellation, retry, restart recovery, atomic publication, and automatic library scanning.
- Local video scanning with movie and `S01E02` / `Series/Season 1` episode detection; per-user favorites, watched state, and resume position.
- M3U text/file import, channel browsing, and HLS playback with locally bundled hls.js and native browser fallback.
- Per-user TVBox / 影视仓 feeds for the local library, enabled online sources, and live channels. Feed tokens can be rotated to revoke previous links.
- Argon2id password hashing, HttpOnly sessions, login throttling, and administrator/member roles.
- Responsive Chinese web interface with light and dark themes, a mobile navigation drawer, and keyboard focus states.

Online CMS sources and media must be supplied by the administrator. PolyView does not execute APKs, TVBox spider/JAR adapters, or arbitrary source JavaScript.

## Run

Requires Rust 1.89 or newer. Install **FFmpeg** on the server for HLS downloads. The web interface and HLS player are embedded in the binary; Node.js is not required to run it.

```sh
cargo run --locked --release
```

Open `http://127.0.0.1:8080`. The first visit creates the administrator, unless accounts have already been imported from an original PolyView database. The default folders are `./data` for the new `polyview.sqlite3` database and `./media` for the library. Completed downloads are written under `./media/downloads`.

| Environment variable     | Default          | Purpose                                                                                                 |
| ------------------------ | ---------------- | ------------------------------------------------------------------------------------------------------- |
| `POLYVIEW_BIND`          | `127.0.0.1:8080` | Listen address                                                                                          |
| `POLYVIEW_DATA_DIR`      | `./data`         | New database and import-report directory                                                                |
| `POLYVIEW_MEDIA_DIR`     | `./media`        | Video library directory                                                                                 |
| `POLYVIEW_PUBLIC_URL`    | Request origin   | HTTP(S) origin reachable by the TV client, such as `http://NAS_IP:8080`; no path, query, or credentials |
| `POLYVIEW_SECURE_COOKIE` | `false`          | Set `true` when served through HTTPS                                                                    |
| `POLYVIEW_LEGACY_DB`     | Unset            | Explicit path to a consistent backup of an original `polyview.db` to import                             |
| `POLYVIEW_LEGACY_CONFIG` | Unset            | Optional original JSON configuration used with `POLYVIEW_LEGACY_DB`                                     |
| `RUST_LOG`               | `polyview=info`  | Log filter                                                                                              |

Use the NAS HTTPS reverse proxy for HTTPS access and set `POLYVIEW_SECURE_COOKIE=true` there. PolyView uses `polyview.sqlite3` for its own schema and does not automatically open or modify a neighboring `polyview.db`.

## Docker / NAS

The image includes FFmpeg and runs as a non-root user.

```sh
mkdir -p data media
POLYVIEW_UID=$(id -u) POLYVIEW_GID=$(id -g) docker compose up -d --build
```

Open `http://YOUR_NAS_IP:8080` and create the first administrator before sharing the address. `POLYVIEW_PORT` changes the published host port. `POLYVIEW_DATA_DIR` and `POLYVIEW_MEDIA_DIR` can be absolute host paths; the container uses `/data` and `/media` internally. Set `POLYVIEW_PUBLIC_URL` to the address that the TV can reach, rather than the container address or `127.0.0.1`.

The container runs as UID/GID 1000 by default. The selected UID/GID must have write access to the data directory and `media/downloads`, and read access to the rest of the media library. NAS container managers can set these values in their environment configuration. Keep the same values when recreating the container.

To update, pull the source and run the same Compose command again. To stop this Compose project, run `docker compose down`; its bind-mounted folders remain on the host. For a consistent backup, stop this instance and copy its entire data directory, together with any media you want to preserve.

### Continuing from the original NAS service

Keep the original service, configuration, database, media, and Telegram storage until the replacement has been checked against your data. Use a separate port and separate destination data directory for the new instance; do not replace the original container or its mounts merely to test an import.

The importer requires an explicit `POLYVIEW_LEGACY_DB` path. Supply a consistent SQLite backup, not a lone copy of a live database that still depends on its WAL file. The source database is opened read-only; changes are committed to the new database in a transaction.

For example, stage a native instance with a prepared backup:

```sh
POLYVIEW_BIND=127.0.0.1:8081 \
POLYVIEW_DATA_DIR=./data-next \
POLYVIEW_MEDIA_DIR=./media-next \
POLYVIEW_LEGACY_DB=/path/to/backup/polyview.db \
POLYVIEW_LEGACY_CONFIG=/path/to/backup/config.json \
cargo run --locked --release
```

For a container import, mount that backup directory read-only at a separate path, then set the two legacy variables to the corresponding container paths in the NAS container manager or a Compose override. The default Compose file does not mount legacy files or forward the legacy variables automatically.

The importer can carry over eligible active accounts with supported Argon2 password hashes, compatible CMS sources, subscriptions that map to those accounts and sources, and compatible TV feed tokens. Existing users with matching usernames are matched without overwriting their credentials. Unsupported or unmapped records are counted or described in `POLYVIEW_DATA_DIR/legacy-import-report.json`.

**Imported automatic downloads remain off.** Review each imported subscription and its watched count before enabling downloads, to avoid downloading episodes already held by the original service. Import is recorded once per canonical source database path; rerunning that path does not resynchronize later changes, and importing a different original database into the same destination is rejected. Remove the legacy environment variables after reviewing a successful import.

The following remain in the original service and are **not migrated or resumed** by this importer:

- Old download jobs, their history, and unfinished transfers.
- Favorites, viewing history/progress, search history, skip settings, and login sessions.
- Telegram login/session storage and Telegram download execution. A recorded Telegram namespace is informational; it is not a working Telegram integration in the new instance.

Media files are not moved or deleted. The original service remains the place to access those retained records and Telegram functions until they are explicitly replaced. Importing records is not evidence that their remote sources or authorization still work; verify them before switching traffic.

## Online viewing and series tracking

An administrator adds a MacCMS-compatible HTTP(S) JSON API in the source settings. Source URLs must not contain embedded username/password credentials. Enabled sources appear in online browsing, search, and the TV feed. Disabled sources are unavailable for lookup and tracking; an in-use source can be disabled while its subscriptions are retained. Sources can be marked **成人源**; they stay hidden from non-admin accounts in browsing, search, details, tracking, and the TV feed unless an administrator enables the global adult-source switch. Only administrators can view or change source configuration and that switch.

Online browsing aggregates all enabled sources by default. Results with the same media kind, title, and year are merged into one card showing the number of sources; movies, series, variety shows, anime, and other categories stay separate. Opening a card offers a source switcher, so playback and tracking use the selected source. Results load as the page is scrolled. Media-kind, category, and year filters are available; the media-kind filter queries every cached source category of that kind, while category and year filters are translated to each source's own numeric category IDs where possible, and selecting a single source remains possible.

Choose a title, playback line, and episode to watch. The browser fetches online media through the authenticated playback proxy, which resolves relative HLS URLs without relying on the source's browser CORS settings. Source reachability, authorization, video codecs, and any source restrictions still apply.

Add an online title to the tracking list to retain its current episode count and your watched count. Checks run after adding it, manually from the tracking page, and every ten minutes while PolyView is running. Automatic downloads are off by default. When enabled, the first play line whose episodes all end in a media extension (`.m3u8`, `.mp4`, and similar) supplies episodes after the watched count that have not already been queued for that subscription. Lines made of CMS share/parser pages are labelled **非直链** and skipped, because PolyView does not run source spiders. Failed or cancelled download jobs can be retried on the downloads page.

Only directly playable HTTP(S) episodes are supported. Parser-only links, unsupported source adapters, magnet links, and DRM services are not handled. HLS downloads require a `.m3u8` URL and FFmpeg; FFmpeg remuxes supported streams to MP4 without re-encoding, so this is not a general transcoding service.

## Local library and live channels

Place videos in the configured media directory, then select **扫描媒体目录** as an administrator. Startup and completed downloads also trigger scanning. Scanning does not move or delete media files. A removed file disappears from the catalog; restoring it at the same path restores its saved local favorites and progress.

For episodes, use names such as `三体.S01E02.mp4`, or put videos under `三体/Season 1/`. Accounts share the library and channel list while keeping separate local progress, favorites, online subscriptions, and download queues. Removing an account revokes its sessions and feeds and removes its queued tasks; completed video files stay in the shared library.

Channel imports accept a UTF-8 M3U playlist containing `#EXTM3U`, `#EXTINF` metadata, and HTTP(S) stream URLs. Re-importing the same name and group with a different URL adds a backup source to that channel instead of creating a duplicate; the player switches automatically when a source fails, and the **线路** button cycles sources manually. Browser live playback uses the same authenticated media proxy as online titles, so the server must be able to reach the channel and HLS playlists/segments are rewritten through PolyView; browser CORS and HTTPS mixed-content restrictions no longer apply to the channel itself. TV clients still fetch live URLs from the NAS address, which must be reachable by the device.
The live page is a TV guide: channels stay pinned on the left while a forward-looking programme grid scrolls horizontally, starting at the current programme and hiding past entries. Channels without XMLTV data are hidden from the guide until a matching feed is configured. Group chips filter CCTV, satellite, local, and international channels; clicking a channel or programme starts playback. Programme data comes from XMLTV feeds (default `http://epg.51zmt.top:8000/e.xml`, `https://epg.112114.xyz/pp.xml`, `https://live.fanmingming.com/e.xml`, and the gzipped `https://raw.githubusercontent.com/litiande03/epg/master/pl.xml.gz`), merged by channel with earlier feeds taking precedence; names are matched after normalizing CCTV aliases, common local-channel aliases, and `4K`/高清 suffixes. Administrators can edit up to four feed URLs, one per line, and refresh them from **账户与设置 → 电视节目单**; the result is cached in SQLite and shown until refreshed.
The live player uses compact controls without a playback timeline and shows the channel's current and upcoming programmes on the right; the fullscreen button fills the screen with both the stream and the programme list.

PolyView serves original local media without transcoding. MP4 with H.264/AAC and supported WebM codecs work in common browsers; MKV, AVI, and other codecs depend on the browser or TV client.

## TVBox / 影视仓 feed

Open the TV feed section while signed in, copy the configuration URL, and enter it in the TV client's configuration/subscription field. The configuration contains a local-library site, a site for each enabled online source, and the live-channel playlist. Local series are grouped into episode lists, and local video endpoints support Range and HEAD requests. Online play lines whose episodes are direct media URLs are listed before share/parser pages, so the client's default line is the one PolyView can actually play.

The link grants access to the shared catalogs, media, and channels without a browser login. Keep it private. Rotating the token revokes previous configuration, catalog, and media links; removing the account also revokes its token. After rotation, update the TV client's subscription URL. Existing original-service URLs are not automatically redirected to the new service.

The server-side feed and token playback are integration-tested. Actual 影视仓 device setup and playback are a separate check and may be deferred while desktop web and NAS deployment are verified.

## API entry points

Media, catalog, tracking, download, and TV-feed APIs require a login. Initial setup, login, and authentication status are available without a session. Writes require `X-PolyView-Request: 1`; source management and library scanning also require an administrator.

| Endpoint                                                                                    | Purpose                                                                        |
| ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| `GET/POST /api/sources`, `PUT/DELETE /api/sources/{id}`                                     | List and manage CMS sources                                                    |
| `GET/PUT /api/catalog/settings`                                                             | Read or update the admin-only adult-source switch                              |
| `GET /api/catalog/discover`, `/api/catalog/categories`, `/api/catalog/search`, `/api/catalog/{source_id}/detail` | Multi-source browsing with media-kind, category, and year filters, client-side title aggregation, search, and title details |
| `GET /api/playback?url=...`                                                                 | Authenticated HLS/video playback proxy                                         |
| `GET/POST /api/tracking`, `PUT/DELETE /api/tracking/{id}`, `POST /api/tracking/check`       | Online subscriptions, watched counts, automatic download selection, and checks |
| `GET/POST /api/downloads`, `DELETE /api/downloads/{id}`, `POST /api/downloads/{id}/retry`   | Queue, inspect, cancel, and retry downloads                                    |
| `GET /api/tvbox`, `POST /api/tvbox/token`                                                   | Obtain or rotate the current account's TV feed                                 |
| `GET/POST /api/channels`, `POST /api/channels/import`, `DELETE /api/channels/{id}`           | Live channel list, M3U import with backup sources, and removal                 |
| `GET /api/epg`, `POST /api/epg/refresh`, `GET/PUT /api/epg/config`                          | TV guide data, XMLTV refresh, and feed configuration                           |
| `GET /tvbox/{token}/config.json`, `/vod`, `/source/{source_id}`, `/live.m3u`, `/media/{id}` | Token-authenticated TV client endpoints under the same `/tvbox/{token}` prefix |

## Checks

Install FFmpeg/FFprobe to run the real HLS integration test; it prints an explicit skip message when either executable is unavailable.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
node --check web/app.js
node --test tests/frontend.test.mjs
cargo run -- --help
```

Tests cover account/session lifecycle, permissions, per-user state, CMS search/details and partial source failure, tracking deduplication, read-only legacy import, HLS rewriting, Range/HEAD playback, scan recovery, download cancellation/retry, TV feed revocation, and restart recovery. `tests/hls_flow.rs` generates an H.264/AAC HLS fixture with FFmpeg, downloads and probes its MP4 output, and verifies automatic library entry, progress, Range playback, and TV token playback.

The CI workflow installs FFmpeg and runs formatting, strict Clippy, Rust tests, frontend behavior/syntax tests, a release build, and Compose validation. Passing local checks does not establish that the NAS deployment, real source availability, Telegram authorization, or TV device playback have been verified.

## License

[MIT](LICENSE)

The bundled hls.js 1.7.4 is MIT licensed; its notice is in [web/vendor/hls.LICENSE](web/vendor/hls.LICENSE).
