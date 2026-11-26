# 电视应用源协议的静态样本

本次从 `K1077-电视TV/` 选取两款点播客户端和三款直播客户端。按 ZIP 目录读取 DEX、文本资源和 JSON，仅记录字段、标记及结构。分析期间没有安装或运行 APK、加载插件、访问内置源或提取凭据。下表的 SHA-256 前 16 位用于区分同名发行包。

| 样本 | SHA-256 前缀 | 静态观察 |
| --- | --- | --- |
| NewBox 电视版（内置 4K 接口） | `a5c89a53f974f401` | `assets/js/lib/live2cms.js` 同时包含 `#EXTM3U`、`#EXTINF`、`#genre#`、`vod_play_from`、`vod_play_url` 和 `User-Agent`、`Referer`、`Authorization`。`assets/epg_data.json` 的 `epgs` 包含 615 条记录，样本字段有 `tvid`、`epgid`、`name`、`status`、`note`、`logo`。 |
| OK影视 2.5.0（安卓 4.4） | `58aa9186822a4fb7` | `classes.dex` 包含 `sites`、`parses`、`lives`、两个 `vod_play_*` 字段及 `#EXTVLCOPT`、`catchup-source`。包内也有原生库和代理组件，同一个 APK 可包含多种来源的字符串。 |
| 星河 TV 直播 | `92ac7a6c5dc10038` | `assets/local_channels.json` 是 31 个键组成的字典，抽查的分组值为字符串数组。`assets/core_manifest.json` 有 `schema`、`jarList` 和 `assetHashes`。DEX 含 M3U、`#genre#`、HLS MIME、RTMP 和 RTSP 字符串。 |
| 星视 TV 直播 2.7.6 | `af8b0519bab14c0f` | `assets/js/deep.parse.js` 含 `#EXTM3U` 与 HLS MIME；`assets/index.html` 含 `User-Agent`。这些资源本身没有给出可作为 MacCMS 清单导入的配置。 |
| 空蒙直播 64 位共存版 | `40f6f2925b124608` | `assets/configs_default.json` 的 `channelSourceList` 样本项含 `name`、`uri`、`epgUriList`；另有 `channelAliasList` 和 `channelMergeSimilar`。`assets/channel_alias.json` 有 189 个键。DEX 含 `tvg-id`、`tvg-logo`、`#EXTVLCOPT`、`catchup-source` 和 `Cookie`。 |

## 对 PolyView 导入器的含义

- 点播目录、播放线路和节目单是不同层级。MacCMS 的 `vod_play_from` 与 `vod_play_url` 描述剧集的播放线路；TVBox 的 `sites`、`parses`、`lives` 分别声明目录、解析器与直播订阅。配置预览需要按用途区分。
- 直播的频道标识、台标、别名、EPG 源与媒体地址应分别保存。M3U 与分组 TXT 可以表达相近的频道结构，请求头和回看模板属于具体线路。
- `User-Agent` 和 `Referer` 可作为播放兼容参数；`Cookie` 与 `Authorization` 涉及身份信息。静态字面量无法证明具体接口必需哪一种鉴权、凭据的值、有效期或可用性。样本中出现的媒体协议也不能证明当前服务端支持它们。
- 本次兼容范围以可声明的数据格式为准。包内 JavaScript、原生代理、JAR 列表和插件元数据只作为识别格式的证据。PolyView 导入清单时无需运行它们。

## 复核方法

在项目根目录使用 `unzip -Z1 "$apk"` 检查成员路径，使用 `unzip -p "$apk" assets/configs_default.json | jq '.channelSourceList[] | keys'` 核对空蒙样本的字段名。`shasum -a 256 "$apk"` 用于比对版本。检查 DEX 时只匹配已知协议标记；通用网络库中也可能出现同一标记。

LunaTV 的公开配置文档给出了 `api_site.<key>.api`、`name` 和可选的 `detail`。其中 `api` 是目录接口，`detail` 是网页根地址；预览导入时应选取 `api`。这条映射依据上游文档，与 APK 的静态观察分开记录。[LunaTV 配置格式](https://github.com/vodtv/LunaTV/blob/main/docs/deployment/CONFIGURATION.md)
