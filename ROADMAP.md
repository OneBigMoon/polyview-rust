# PolyView 后续开发路线图（M7–M12）

> 目标：在现有 Rust 单二进制 + SQLite + 内嵌 Web UI（已完成 M1–M6）的基础上，融合 **LunaTV / LunaTV-config / LibreTV**、**CloudSaver**、**MoviePilot** 以及 **IPTV / TVBox 生态（含 `K1077-电视TV/` 参考软件协议）** 各家所长，打造成一体化的 NAS 全能影音中枢。

---

## 一、各参考项目核心能力吸收矩阵

| 参考生态 / 项目 | 吸收的核心长处 | PolyView 当前状态（M1–M6） | 后续补齐里程碑 |
| :--- | :--- | :--- | :--- |
| **LunaTV / LunaTV-config / LibreTV** | 远程源配置订阅（`config.json` / Base58 / 多仓）自动拉取、定时 API 测活测速与失效降权、JSON 解析器（解决非直链）、弹幕/多源自动容错切换 | 仅支持手动逐个添加 MacCMS URL；非直链直接跳过；测速需手动触发 | **M7、M8** |
| **IPTV 聚合生态 & `K1077-电视TV/` 直播软件** | M3U + TXT (`#genre#`) 全格式解析、多线路 `#` 拆分、自定义请求头（`User-Agent`/`Referer`/`Cookie`）透传、批量测活测速排序、完整 EPG 回看模板 | 仅支持严格 `#EXTM3U`；无请求头透传；无直播测活测速；仅支持简单 `start` 时移 | **M9** |
| **CloudSaver** | Telegram 资源频道聚合搜索、网盘链接自动提取与有效检测、多网盘（115/夸克/天翼/阿里等）一键转存、WebDAV / STRM 联动、TG Bot 推送 | 仅支持管理员 SSH 跑 `tdl login` + 手动粘贴单条 `t.me` 链接下载单视频 | **M10、M11** |
| **MoviePilot (v2/v3)** | TMDB/豆瓣元数据刮削（海报/背景图/演员/分集/NFO）、媒体文件智能识别重命名与硬链接整理、豆瓣“想看”自动转订阅、外部下载器（qBittorrent/Transmission）与规则优选 | 下载后直接落盘 `media/downloads/`；仅靠文件名正则识别 `S01E02`；无 NFO/TMDB 刮削、无整理重命名与 PT/BT 下载器对接 | **M12** |

---

## 二、开发约束（给 OMP / DeepSeek 执行时必须遵守）

1. **保持技术栈一致**：Rust 2024 + Axum + SQLite (`rusqlite`) + 原生前端 (`web/index.html`, `web/app.js`, `web/style.css`)，不引入 Node.js 运行时依赖。
2. **禁止破坏现有功能**：M1–M6 已完成的认证、权限、CMS 聚合、EPG、TVBox 输出、下载队列与遗留迁移必须保持兼容。
3. **每完成一个 Milestone 必须全部通过校验**：
   ```sh
   cargo fmt --all -- --check
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked --all-targets
   node --check web/app.js
   node --test tests/frontend.test.mjs
   ```
4. **安全底线**：所有写接口须校验登录态与 `X-PolyView-Request: 1`；管理员配置（源、解析器、网盘 Cookie、TG 凭据、下载器凭据）严格做权限隔离，且不向非管理员泄露敏感字段。

---

## 三、分阶段详细任务清单

### M7：源订阅中心与协议兼容（吸收 `LunaTV-config` + TVBox 生态）
**目标**：彻底告别手工逐个填源，支持远程配置订阅、自动巡检与 TVBox/LunaTV 配置一键导入。

- **后端任务**：
  1. **协议调研归档**（可选前置）：梳理 `K1077-电视TV/` 中代表性点播与直播软件的源配置格式、请求头特征与鉴权模式，整理至 `docs/source-protocol-analysis.md`。
  2. **远程源配置订阅表 `source_subscriptions`**：支持订阅 `LunaTV-config` 风格 JSON、TVBox 单仓/多仓 JSON 及 Base58 配置 URL，支持定时刷新（默认 12h）与手动预览导入。
  3. **API 接口**：
     - `GET/POST /api/source-subscriptions`、`PUT/DELETE /api/source-subscriptions/{id}`
     - `POST /api/source-subscriptions/preview`（解析远程配置，列出可导入的 CMS 源、解析器、直播源供管理员勾选）
     - `POST /api/source-subscriptions/sync`（同步已启用的订阅，自动去重、增量更新、标记失效源）
  4. **后台自动测活与健康度评分**：在 `src/catalog.rs` 增加定时健康巡检任务，记录延迟、成功率、连续失败次数；聚合搜索与详情页自动按健康分排序并降权/跳过故障源。
- **前端任务**：
  - 在源管理页增加「订阅导入 / 远程配置」标签页，支持一键粘贴 LunaTV-config / TVBox 订阅链接、勾选导入、查看健康度与延迟徽章。
- **涉及文件**：`src/catalog.rs`、`src/tvbox.rs`、`src/settings/backup.rs`、`web/app.js`、`web/index.html`、`tests/cases/source_ops.rs`。
- **验收标准**：能直接解析并导入 LunaTV-config / TVBox 格式 JSON，定时巡检能自动降权超时/5xx 源，单测与集成测试通过。

---

### M8：非直链解析器与播放体验强化（吸收 `LunaTV` / `LibreTV`）
**目标**：打通“非直链”播放与下载，提升跨源切源与观影体验。

- **后端任务**：
  1. **JSON 解析器管理表 `parsers`**：字段含 `name`、`url_template`、`headers_json`、`flags`（匹配线路标识，如 `qq,youku,iqiyi` 或 `*`）、`priority`、`timeout_ms`、`enabled`。
  2. **解析器接口与解析服务**：
     - `GET/POST /api/parsers`、`PUT/DELETE /api/parsers/{id}`、`POST /api/parsers/{id}/test`
     - 新增 `POST /api/playback/resolve`：当集数 URL 为非直链时，按 `flags` 与 `priority` 顺序并发/依次请求已启用的 JSON 解析器，提取真实 `.m3u8` / `.mp4` 播放地址及附带 Header，结果短期缓存（如 10 分钟）。
  3. **追更与下载联动**：修改 `src/tracking.rs` 和 `src/network.rs`，当自动下载遇到非直链线路但存在匹配解析器时，先尝试解析出直链再入队下载。
  4. **播放端多源自动容错**：在播放失败时返回同影片其他可用源的对应集数候选，支持前端无缝切源并继承当前播放进度。
- **前端任务**：
  - 管理后台新增「解析接口」配置面板（含单条测试按钮）。
  - 播放器支持“解析中”状态提示、播放失败自动切换同片其他可用源、保持当前时间戳续播。
- **涉及文件**：新增 `src/parsers.rs`、修改 `src/playback.rs`、`src/tracking.rs`、`src/network.rs`、`web/app.js`。
- **验收标准**：配置模拟 JSON 解析器后，非直链剧集可在 Web 端正常播放并成功走完下载入库测试。

---

### M9：直播系统全面升级（吸收 IPTV 生态 + 直播软件特性）
**目标**：支持国内全格式直播源、请求头透传、自动测活优选与完整 EPG 回看。

- **后端任务**：
  1. **全格式播放列表解析**（升级 `src/network.rs` 的 `parse_playlist`）：
     - 兼容 `#EXTM3U` 与 **TXT 分组格式**（`央视频道,#genre#` + `CCTV1,http://...#http://...`）。
     - 解析 `#EXTVLCOPT:http-user-agent=`、`#EXTVLCOPT:http-referrer=`、`#EXTINF` 中的 `catchup` / `catchup-source` / `tvg-id` / `tvg-logo`，以及 URL 末尾 `|User-Agent=...&Referer=...` 参数。
  2. **频道线路元数据扩展**：在频道线路表中持久化 `headers_json`、`catchup_mode`、`catchup_source`、`latency_ms`、`last_ok_at`、`fail_count`。
  3. **直播代理增强**（`src/playback.rs`）：代理请求自动注入线路专属 Header；支持 HLS 与 HTTP-TS/FLV 流式转发；提供回看 URL 模板渲染（替换 `${(b)yyyyMMddHHmmss}` / `${(e)yyyyMMddHHmmss}` 等常见 EPG 回看占位符）。
  4. **直播源批量测活与测速**：
     - 新增 `POST /api/channels/probe`（支持按分组或全量并发探测连通性与 TTFB 延迟）。
     - 支持“自动按延迟排序线路”与“一键清理连续失效线路”。
- **前端任务**：
  - 直播管理页新增「一键测活/测速」「清理失效线路」按钮及各线路延迟/状态显示。
  - EPG 节目单放开过去 7 天可点击项（当频道配置了回看模板时显示「回看」标识并直接播放回看流）。
- **涉及文件**：`src/network.rs`、`src/live_sources.rs`、`src/epg.rs`、`src/playback.rs`、`src/tvbox.rs`、`web/app.js`、`tests/cases/live_sources.rs`。
- **验收标准**：可正确导入含自定义 Header 的 TXT 与 M3U 混合源，测活后按延迟自动排序，代理播放透传 Header，支持回看 URL 渲染。

---

### M10：Telegram 原生深度集成（免 SSH 授权 + 频道浏览 + 在线流播）
**目标**：替代当前简陋的“宿主机手工 `tdl login` + 单链接下载”，让 TG 成为站内一等资源库。

- **后端任务**：
  1. **接入原生 MTProto 或完整交互桥接**（推荐优先评估 `grammers-client` 纯 Rust 方案，备选 `tdl` PTY 交互桥接）：
     - 支持 Web 端发起登录：输入 API ID/Hash（管理员配置）、手机号 → 验证码 → 二步密码（2FA）或扫码登录，Session 持久化在 `POLYVIEW_DATA_DIR/telegram/`。
  2. **TG 频道/群组浏览与搜索**：
     - `GET /api/telegram/chats`（列出已加入的频道/群组及自定义收藏资源频道）。
     - `GET /api/telegram/chats/{chat_id}/messages?q=...&offset=...`（拉取视频消息与资源帖子，返回文件名、大小、时长、缩略图、正文中的网盘链接）。
  3. **TG 视频在线流式播放**：
     - `GET /api/telegram/stream/{chat_id}/{msg_id}`：支持 HTTP `Range` 请求，按分块从 TG 拉取视频流边下边播，并同步暴露到 TVBox 订阅中。
  4. **下载引擎升级**：
     - 支持 MediaGroup（相册多视频）批量入队。
     - 实时上报下载百分比与速率（不再丢弃输出）。
     - 支持指定 TG 频道关键词订阅，命中新消息自动加入下载队列。
- **前端任务**：
  - 重构 Telegram 面板：Web 端登录向导、左侧频道列表 + 右侧视频卡片/搜索框、点击直接在线播放或一键加入下载队列。
- **涉及文件**：`Cargo.toml`、`src/telegram.rs`、`src/telegram/media.rs`、`src/network.rs`、`src/tvbox.rs`、`web/app.js`。
- **验收标准**：全程在浏览器内完成 TG 登录、浏览频道视频、拖动进度条在线播放，以及带实时进度的批量下载入库。

---

### M11：CloudSaver 能力融合（TG 资源聚合搜索 + 多网盘一键转存）
**目标**：吸收 `CloudSaver` 核心能力，实现“全网盘资源聚合搜 → 一键转存自己网盘 → 可选直连/下载”。

- **后端任务**：
  1. **网盘资源搜索源管理**：
     - 新增 `cloud_search_channels` 表，支持批量导入类似 CloudSaver 的公开 TG 资源频道配置（JSON 导入/导出）或外部搜索 API。
     - 新增 `GET /api/cloud/search?q=...`：并发检索已配置的 TG 频道（支持无登录公开频道网页/接口抓取 + 已登录 TG 客户端搜索）并自动提取与分类网盘链接（夸克、115、天翼、阿里、百度、UC、迅雷），过滤重复链接。
  2. **多网盘账号与一键转存模块**（新增 `src/cloud/`）：
     - 新增 `cloud_accounts` 表（加密或管理员隔离存储 Cookie/Token 及默认保存目录）。
     - 优先实现国内最常用的 **夸克网盘**、**115 网盘**、**天翼云盘** 转存适配器（目录浏览、链接有效性校验、一键转存到指定目录）。
     - `GET/POST /api/cloud/accounts`、`POST /api/cloud/save`（一键转存并返回目标路径）。
  3. **（可选扩展）WebDAV / AList / OpenList 挂载或 STRM 导出**：支持把转存后的网盘目录映射为本地虚拟媒体库或生成 `.strm` 文件供本地/TVBox 播放。
  4. **TG Bot 联动**：支持配置 Telegram Bot Token，接收私聊/群内指令（搜片、发链接自动转存或下载）并回传进度。
- **前端任务**：
  - 新增「网盘搜片」页面（集成豆瓣热门榜“对榜寻片”、按网盘类型筛选、一键转存弹窗选择目录）。
  - 账户/管理页新增「网盘账号配置」与「资源频道订阅管理」。
- **涉及文件**：新增 `src/cloud/`（`mod.rs`、`search.rs`、`quark.rs`、`pan115.rs`、`tianyi.rs`、`bot.rs`）、`web/app.js`、`web/index.html`。
- **验收标准**：输入片名可聚合搜出多频道网盘链接，点击可校验有效性并一键转存到已配置的网盘目录。

---

### M12：MoviePilot 级媒体库自动化（刮削 + 整理重命名 + 下载器联动）
**目标**：把 PolyView 的本地媒体库从“简单文件列表”升级为全自动、带完整海报墙与多下载器联动的家庭影院中枢。

- **后端任务**：
  1. **元数据刮削引擎**（新增 `src/scraper.rs`）：
     - 对接 **TMDB API**（管理员配置 API Key 与代理）+ 豆瓣/Bangumi 兜底。
     - 自动拉取中文/原名、年份、评分、类型、剧情简介、海报、背景图、演职员表及剧集分季/分集标题，本地缓存图片并生成兼容 Emby/Jellyfin/Kodi 的 `.nfo` 文件。
  2. **智能识别与文件整理（硬链接 / 移动）**：
     - 升级文件名解析器（支持常见 PT/BT 命名规范、年份、季集 `S01E01` / `EP01` / `第01集`、分辨率、编码）。
     - 下载完成或手动触发整理时，按模板（如 `Movies/{title} ({year})/{title} ({year}).{ext}`、`TV/{title} ({year})/Season {season:02}/{title} S{season:02}E{episode:02}.{ext}`）执行**硬链接**（跨分区自动降级为复制/移动），保留整理历史以便回滚。
  3. **外部下载器对接（qBittorrent / Transmission）**：
     - 新增 `downloaders` 配置与客户端适配，支持在统一任务中心查看/管理 qBittorrent / Transmission 任务，并监听完成事件自动触发整理与刮削。
  4. **订阅规则与豆瓣同步**：
     - 支持按画质/关键词过滤订阅资源；支持定时同步指定豆瓣用户的“想看”列表自动创建追剧/追片订阅；完成后自动通知（复用现有 Bark/Webhook）并触发 Emby/Jellyfin 媒体库刷新（可选配置）。
- **前端任务**：
  - 本地媒体库升级为完整海报墙详情页（背景大图、评分、演员表、分季切换、手动重新识别/刮削按钮）。
  - 新增「媒体整理记录」与「外部下载器 / TMDB / Emby 联动设置」面板。
- **涉及文件**：新增 `src/scraper.rs`、`src/organizer.rs`、`src/downloaders.rs`，修改 `src/library.rs`、`src/tracking.rs`、`src/settings.rs`、`web/app.js`。
- **验收标准**：下载或放入未规范命名的影片文件后，能自动识别并硬链接整理至标准目录、生成海报与 `.nfo` 文件，并在 Web 端与 TVBox 端正确展示完整元数据。

---

## 四、给 OMP + DeepSeek 的推荐执行指令模板

在 OMP 窗口中建议**每次只推进一个 Milestone**，直接发送如下提示词即可：

> 请阅读项目根目录的 `ROADMAP.md`，先检查当前代码结构，然后严格按照「开发约束」完成 **M7（源订阅中心与协议兼容）** 的全部后端接口、前端界面与自动化测试。完成后运行 `ROADMAP.md` 中列出的全部检查命令确保全部通过，并提交一个清晰的 commit。
