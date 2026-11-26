# PolyView

PolyView is a self-hosted media hub for a NAS: browse and watch from a desktop web browser, track ongoing series, download episodes into your library, and provide a subscription feed to a TVBox / 影视仓 client.

## Included

- Online catalog browsing, multi-source search, title details, playback lines, and episode selection using MacCMS-compatible JSON APIs.
- Media-kind filters separate movies, series, variety shows, anime, children's content, and other categories.
- Administrator-managed online sources: add, edit, enable, disable, and remove sources.
- Source subscription previews, indirect playback resolution, searchable cloud shares, and member-scoped Telegram browsing and streaming.
- Same-origin authenticated playback proxy for HTTP(S) video and HLS, including nested playlists, segments, keys, and byte-range requests.
- Online series subscriptions with watched-episode counts, manual checks, and checks every ten minutes. Automatic downloads are optional and deduplicate episodes already queued for that subscription.
- Direct video downloads and FFmpeg HLS-to-MP4 downloads, with queue progress, cancellation, retry, restart recovery, atomic publication, and automatic library scanning.
- Local video scanning with movie and `S01E02` / `Series/Season 1` episode detection; per-user favorites, watched state, and resume position.
- Optional local artwork and NFO generation, journaled media organization, and completion imports from qBittorrent and Transmission.
- M3U text/file import, channel browsing, and HLS playback with locally bundled hls.js and native browser fallback.
- Per-user TVBox / 影视仓 feeds for the local library, enabled online sources, and live channels. Feed tokens can be rotated to revoke previous links.
- Argon2id password hashing, HttpOnly sessions, login throttling, and administrator/member roles.
- Responsive Chinese web interface with light and dark themes, a shared Three.js particle background, a mobile navigation drawer, and keyboard focus states.

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
| `POLYVIEW_TDJSON_PATH` | Optional | Absolute path to the official TDLib JSON library for native Telegram features |
| `POLYVIEW_TELEGRAM_HOST_HELPER` | Optional | Absolute path to the trusted local desktop confirmation helper for native Telegram login |
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

The supplementary migration imports mapped online resume positions, online and channel favorites, recent search terms, and mapped online intro/outro settings. An upgraded installation can read the previously registered backup path to finish these versioned migrations. Keep that backup readable until the import report is complete.

Old jobs and unfinished transfers, login sessions, Telegram session storage, and skip settings without a reliable media or account match remain in the original installation. The report records retained skip settings by reason. The legacy Telegram namespace is informational. Set up fresh authorization for each account in this instance. Media files stay at their existing paths.

## Online viewing and series tracking

An administrator adds a MacCMS-compatible HTTP(S) JSON API in the source settings. Source URLs must not contain embedded username/password credentials. Enabled sources appear in online browsing, search, and the TV feed. Disabled sources are unavailable for lookup and tracking; an in-use source can be disabled while its subscriptions are retained. Sources can be marked **成人源**; they stay hidden from non-admin accounts in browsing, search, details, tracking, and the TV feed unless an administrator enables the global adult-source switch. Only administrators can view or change source configuration and that switch.

An administrator can add a MacCMS JSON API directly or preview a LunaTV or TVBox subscription in **在线影视源**. Select CMS sites, JSON parsers and live playlists before syncing. Remote subscriptions support Base58 and multi-repository configurations. Enabled subscriptions refresh every twelve hours. Health, priority and availability determine which visible sources are preferred.

Online browsing aggregates all enabled sources by default. Results with the same media kind, title, and year are merged into one card showing the number of sources; movies, series, variety shows, anime, and other categories stay separate. Opening a card offers a source switcher, so playback and tracking use the selected source. The first twenty cards render immediately; more cards appear in batches of twenty as the page is scrolled, fetching the next source page only after the current batch is exhausted. Media-kind, language, category, and year filters are available; the media-kind filter queries every cached source category of that kind, the category list follows the selected media kind, and language filters use the CMS `vod_lang` field with aliases such as 国语/汉语普通话 merged. Category and year filters are translated to each source's own numeric category IDs where possible, and selecting a single source remains possible.

Choose a title, playback line, and episode to watch. The browser fetches online media through the authenticated playback proxy, which resolves relative HLS URLs without relying on the source's browser CORS settings. Source reachability, authorization, video codecs, and any source restrictions still apply.

Tracking checks run when a title is added, on demand and every ten minutes. Enable automatic downloads to queue unwatched episodes. Tracking prefers direct lines and can resolve share-page episodes through an enabled parser whose flag matches the CMS line. A failed resolution can be tried again on the next check. Download jobs retain their year and description when published to the library.

Under **账户与设置 → 解析接口**, create a JSON parser with its URL template, CMS line flags, priority, timeout and optional headers. Use the test button with an episode share URL. The server caches successful resolutions for ten minutes and grants the player access to the resolved stream. If a source fails, playback can switch to the matching episode on another visible source and resume at the previous time.

### Playback and discovery

The player remembers volume, mute and speed (1×, 1.25×, 1.5× or 2×). When an episode ends, playback advances on the selected line. Episode downloads use the visible sort order. **选择集数** supports batch selection, including unwatched episodes.

Local and online playback share a continue-watching view. Favorites include local titles, online titles and live channels. Library episodes can be expanded by series. Discovery offers recent searches, suggestions and a region filter. Spotlight dots switch featured titles. Enter in the top search opens discovery. The offline banner follows the browser’s connection status.

Administrators can prioritize sources, run latency checks, apply batch changes and configure per-source request headers. Failing sources enter a temporary cooldown. The player supports per-title intro and outro times and poster fallback through the authenticated image proxy. The optional HLS filter removes segments inside closed ad cue pairs. PolyView includes no bullet-comment (弹幕) feature.

Douban movie and series recommendations are cached for six hours. If the feeds fail, the rails use visible CMS titles. The broadcast calendar caches Bangumi results for twelve hours. Selecting a title searches for matching sources. Administrators can disable the calendar or change its upstream under **通知与日历**. A paused subscription skips checks until resumed. Bark, POST webhook and GET webhook endpoints can receive new-episode and completed-library-import notifications. The test button uses the saved endpoints.

## Local library and live channels

Place videos in the configured media directory. An administrator can select **扫描媒体目录** to index them immediately. Startup and completed native downloads also trigger a scan. An indexed item keeps its per-member progress through managed organization.

For episodes, use names such as `三体.S01E02.mp4`, or put videos under `三体/Season 1/`. Accounts share the library and channel list while keeping separate local progress, favorites, online subscriptions, and download queues. Removing an account revokes its sessions and feeds and removes its queued tasks; completed video files stay in the shared library.

Import a UTF-8 M3U playlist or a grouped TXT list (`Group,#genre#`). Separate alternative stream URLs with `#`. Each line can carry VLC options, pipe-delimited HTTP headers and a catch-up template. Administrators can probe one group or all groups, see first-byte latency and remove lines after three consecutive failures. The guide retains seven days of history. Click a past program marked **回看** to play its original time window. Web and TVBox clients receive scoped relay URLs for protected streams and artwork.
The all-channels toggle includes channels without XMLTV data. Group chips filter the guide, and the time controls move along the programme grid. Configure XMLTV feeds and channel-name aliases under **直播与节目单**.
The live player uses compact controls without a playback timeline and shows the channel's current and upcoming programmes on the right; the fullscreen button fills the screen with both the stream and the programme list.

PolyView serves original local media without transcoding. MP4 with H.264/AAC and supported WebM codecs work in common browsers; MKV, AVI, and other codecs depend on the browser or TV client.

### Automatic library organization

Under **账户与设置 → 媒体自动化**, enable metadata lookup, set the TMDB API key and optionally configure an HTTP proxy. Bangumi and Douban provide fallback lookups. Enable automatic file organization separately and choose hardlink or move mode. Give the service user write access to the destination directories and to library directories where it will publish NFOs and artwork. Both options are initially off.

With automation enabled, the server discovers new library files every 30 seconds. For example, a release named `Film.2022.1080p.WEB-DL.mp4` can be identified as *Film* and organized as `Movies/Film (2022)/Film (2022).mp4`. The default TV template creates a series folder and a numbered season folder. Hardlink mode preserves the source and falls back to a copy across filesystems. Open **影片详情** to inspect metadata, change season or correct a match by title, year or TMDB ID. **媒体整理记录** shows operations and lets an administrator roll back a completed organization after checking the recorded files.

To import external downloads, configure the qBittorrent Web UI or Transmission RPC endpoint and credentials. Map the download client's remote root to the same files under PolyView's mounted library. Allowed members can manage their own tasks in **下载任务**. Completed files enter the same organization and scraping queue.

On the tracking page, set quality and include/exclude terms before enabling automatic downloads. **豆瓣想看同步** can create subscriptions from a member's public wishlist. Set an optional Emby or Jellyfin endpoint and API key under **媒体自动化** to refresh the downstream library after processing. The existing notification settings control Bark and webhook delivery.

## TVBox / 影视仓 feed

Open the TV feed section while signed in, copy the configuration URL, and enter it in the TV client's configuration/subscription field. The configuration contains a local-library site, a site for each enabled online source, and the live-channel playlist. Local series are grouped into episode lists, and local video endpoints support Range and HEAD requests. Online play lines whose episodes are direct media URLs are listed before share/parser pages, so the client's default line is the one PolyView can actually play.

The link grants access to the shared catalogs, media, and channels without a browser login. Keep it private. Rotating the token revokes previous configuration, catalog, and media links; removing the account also revokes its token. After rotation, update the TV client's subscription URL. Existing original-service URLs are not automatically redirected to the new service.

The server-side feed and token playback are integration-tested. Actual 影视仓 device setup and playback are a separate check and may be deferred while desktop web and NAS deployment are verified.

## Live subscriptions, accounts and configuration backups

Administrators can subscribe to remote M3U playlists under **直播与节目单**, refresh enabled subscriptions immediately or let the server refresh them every twelve hours. Each source shows its imported channel count or last error. Channel aliases map an imported name to its XMLTV equivalent. In the guide, use the all-channels toggle for channels without programme data. The live player can collapse the guide, change channel, race its available sources and request a stream from 5, 15 or 30 minutes ago when the upstream supports the start parameter.

The TVBox settings supply both a standard configuration URL and a Bitcoin Base58-encoded configuration URL for older clients. Both contain the private feed token. Under **家庭成员**, an administrator can change another member’s role, disable the account or reset its password. Disabling revokes browser sessions and blocks the TV feed. Role and disable controls protect the current account and the last active administrator.

Use **备份与工具** to export or restore site configuration, including CMS sources, remote live playlists, programme guide configuration and application settings. Treat the exported JSON as private because headers, proxies and notification URLs can contain secrets. Import validates the configuration in a transaction. User accounts, passwords and sessions stay in the destination. The text tool extracts HTTP(S) links for copying or importing.

The administrator export also includes source subscriptions, parser settings, Telegram application credentials, cloud drive and Bot credentials, and media automation settings. Protect the exported file as a credential backup. Native Telegram sessions, members’ wishlists and watch histories are stored with their respective accounts. Use a full data and media backup to migrate these records and files.

## Download engine and Telegram

The administrator can configure the download engine under **下载**. Concurrency accepts 1–4 workers (default 1) and each job has 1–10 attempts (default 3). The engine can be disabled. Pausing a transfer preserves its queue entry. Resuming requeues it. Clearing completed and cancelled history preserves published videos. Downloads carry the source cover, year and description into the local library.

| Tool | Host or container configuration |
| --- | --- |
| FFmpeg | Install the ffmpeg package or set the FFmpeg executable path. It provides the HLS-to-MP4 fallback. |
| FFprobe | Install with FFmpeg to probe downloaded media and run the real HLS integration test. The test invokes `ffprobe` from PATH. |
| N_m3u8DL-RE | Optionally install the binary on the service PATH or configure its executable path. The worker uses FFmpeg if the preferred HLS tool is unavailable or fails. |
| tdl | Install the official CLI to enable authorized Telegram message downloads. Give the service user a private writable session directory. In Docker, select a persistent directory such as /data/telegram. |
| TDLib | Install the official JSON library for native channel browsing and streaming. The legacy tdl CLI uses separate authorization. |

### Native channel browsing

Configure the Telegram application API ID and hash as an administrator. A member can start, monitor or cancel their login in the web panel. A trusted host desktop confirms the requesting member, displays the QR for scanning in Telegram and confirms the verified Telegram identity before binding the session. The built-in desktop flow supports macOS. Other deployments can supply a trusted executable using `POLYVIEW_TELEGRAM_HOST_HELPER`. A headless browser-only deployment cannot complete this confirmation. The QR request expires after two minutes. Accounts that require an additional Telegram password cannot complete this native flow. The separate tdl path below supports terminal two-step verification for downloads.

Private native sessions live under `POLYVIEW_DATA_DIR/telegram/`. A connected member can browse chats, search messages, play videos with byte-range seeking, queue an album or subscribe to matching keywords. The member's TVBox token also permits streaming their Telegram videos.

### CLI message downloads

In **下载任务 → Telegram 视频**, request a personal account login. The administrator runs the pending request’s command in a terminal as the PolyView service user. Keep that terminal open. On macOS and desktop Linux the helper opens the host browser automatically, and it also prints a loopback URL that can be opened manually. Scan the current code on that screen with Telegram. If prompted, enter the two-step password in the original terminal. On a headless host the terminal QR remains available. For a container, the loopback viewer is inside the container’s network namespace. After login, refresh the authorization status and paste a message link to download its video. Keep the per-account session directory private and persistent. The administrator can configure an HTTP(S) or SOCKS5 proxy in download settings. A blank replacement preserves the saved proxy unless explicitly cleared.

The administrator can view pending host-login commands at `GET /api/telegram/login/requests`. The encoded TV client URL is `/tvbox/{token}/config.json?encode=base58`. For a configured Telegram proxy, set `TDL_PROXY` in the operator terminal as directed by the displayed command.

A settings backup includes the privileged downloader configuration, which can contain a proxy credential. Import only backups from a trusted administrator. For a container, configure executable paths as seen inside the container. Adding tools to the host PATH alone does not install them in the image.

## Cloud share search and transfer

Open **网盘搜片**, enter a title or choose one from the recommendation list. The search page groups links from public Telegram pages, the member's native Telegram session and administrator-configured search APIs. Filter by Quark, 115, Tianyi, Aliyun, Baidu, UC or Xunlei. Select **校验与转存** on a supported Quark, 115 or Tianyi result, validate the share and choose the destination folder.

Under **账户与设置 → 网盘与资源频道**, an administrator can import or export search channels and set the drive accounts available to members. A completed single-video Quark save can also produce a local STRM file. Set `POLYVIEW_PUBLIC_URL` to an address reachable by the player before exporting. The file contains a revocable PolyView playback URL.

For Telegram Bot commands, configure a bot token and bind each permitted chat and sender to a PolyView member. Use `/search TITLE`, `/save SHARE_URL` or `/download SHARE_URL`. Downloads use a configured parser for that provider. The bot updates its progress message as the transfer runs.

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
| `GET /api/channels`, `POST /api/channels/import`, `DELETE /api/channels/{id}` | List channels, import a playlist and remove a channel |
| `GET /api/epg`, `POST /api/epg/refresh`, `GET/PUT /api/epg/config`                          | TV guide data, XMLTV refresh, and feed configuration                           |
| `GET /tvbox/{token}/config.json`, `/vod`, `/source/{source_id}`, `/live.m3u`, `/media/{id}` | Token-authenticated TV client endpoints under the same `/tvbox/{token}` prefix |
| `GET/PUT /api/progress`, `DELETE /api/progress/local/{id}`, `DELETE /api/progress/online/{source_id}/{media_id}` | Continue watching and remove a saved position |
| `GET/POST /api/favorites`, `DELETE /api/favorites/{source_id}/{media_id}`, `PUT /api/channels/{id}/favorite` | Per-account favorites across sources |
| `GET/POST/DELETE /api/search/history`, `GET /api/catalog/suggest` | Recent queries and title suggestions |
| `POST /api/sources/probe`, `POST /api/sources/reorder`, `POST /api/sources/batch` | Admin source maintenance |
| `GET/PUT /api/skip`, `GET/PUT /api/settings/play`, `GET /api/image?url=...` | Per-title skip times, global playback options and authenticated poster proxy |
| `GET /api/catalog/recommend`, `GET /api/calendar`, `GET/PUT /api/settings/calendar` | Trending titles and weekly broadcast calendar |
| `GET/PUT /api/settings/notify`, `POST /api/settings/notify/test` | Configure and test notification destinations |
| `GET/POST /api/live-sources`, `PUT/DELETE /api/live-sources/{id}`, `POST /api/live-sources/refresh`, `GET/PUT /api/epg/aliases` | M3U subscriptions and channel-name mapping |
| `PUT /api/users/{id}`, `POST /api/users/{id}/password` | Role, availability and administrator password reset |
| `GET /api/settings/export`, `POST /api/settings/import` | Administrator configuration backup and transactional restore |
| `GET/PUT /api/settings/downloads`, `POST /api/downloads/{id}/pause`, `POST /api/downloads/{id}/resume`, `DELETE /api/downloads/completed` | Download engine configuration, transfer control and history cleanup |
| `GET/POST/DELETE /api/telegram/login`, `POST /api/downloads/telegram` | Account-specific authorization and message downloads |
| `GET/POST /api/source-subscriptions`, `POST /api/source-subscriptions/preview`, `POST /api/source-subscriptions/sync` | Manage, preview and synchronize source subscriptions |
| `GET/POST /api/parsers`, `POST /api/playback/resolve` | Configure parsers and resolve an episode for playback |
| `GET/POST/DELETE /api/telegram/native/login`, `GET /api/telegram/chats`, `GET /api/telegram/stream/{chat_id}/{msg_id}` | Native login status, member chats and byte-range video |
| `GET /api/cloud/search`, `POST /api/cloud/check`, `POST /api/cloud/save` | Search configured channels, validate and transfer a share |
| `GET/PUT /api/settings/scraper`, `GET/PUT /api/settings/organizer`, `GET /api/organizer/history` | Configure metadata, organization and review the journal |
| `GET/POST /api/downloaders`, `GET/POST /api/external-tasks` | Manage external clients and their torrents |
| `GET/PUT /api/tracking/douban`, `GET/PUT /api/settings/media-server` | Synchronize a member wishlist and configure downstream refresh |

## Checks

Install FFmpeg/FFprobe to run the real HLS integration test; it prints an explicit skip message when either executable is unavailable.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
node --check web/app.js
node --check web/three-bg.js
node --test tests/frontend.test.mjs
cargo build --locked --release
```

Tests cover account/session lifecycle, permissions, per-user state, CMS search/details and partial source failure, tracking deduplication, read-only legacy import, HLS rewriting, Range/HEAD playback, scan recovery, download cancellation/retry, TV feed revocation, and restart recovery. `tests/hls_flow.rs` generates an H.264/AAC HLS fixture with FFmpeg, downloads and probes its MP4 output, and verifies automatic library entry, progress, Range playback, and TV token playback.

Offline fixtures cover source subscription formats, resolver failover, live catch-up, TDLib messages and byte ranges, cloud transfer protocols, Bot progress, qBittorrent and Transmission, metadata sidecars, organization rollback, and token-scoped TVBox artwork.

The CI workflow installs FFmpeg and runs formatting, strict Clippy, Rust tests, frontend behavior/syntax tests, a release build, and Compose validation. Passing local checks does not establish that the NAS deployment, real source availability, Telegram authorization, or TV device playback have been verified.

## License

[MIT](LICENSE)

The bundled hls.js 1.7.4 is MIT licensed; its notice is in [web/vendor/hls.LICENSE](web/vendor/hls.LICENSE).
