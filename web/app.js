(() => {
  "use strict";
  const $ = (id) => document.getElementById(id);
  const main = $("content");
  let renderSequence = 0;
  const state = {
    user: null,
    page: "online",
    query: "",
    epg: null,
    filter: "all",
    library: [],
    channels: [],
    series: [],
    downloads: [],
    player: null,
    poll: null,
    onlinePage: 1,
    onlineSource: "all",
    onlineKind: "",
    onlineCategory: "",
    onlineYear: "",
    onlineLang: "",
    categories: null,
    onlineObserver: null,
    liveGroup: "all",
    epgOnlyCurrent: false,
  };
  const iconPaths = {
    play: { d: "M8 5.5v13l10.5-6.5Z", fill: true },
    pause: { d: "M9 6v12M15 6v12" },
    volume: {
      d: "M4 9v6h4l5 4V5L8 9H4ZM16 9.5a4 4 0 0 1 0 5M18.5 7a7 7 0 0 1 0 10",
    },
    mute: {
      d: "M4 9v6h4l5 4V5L8 9H4ZM17 9l5 6M22 9l-5 6",
    },
    fullscreen: {
      d: "M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5",
    },
    exitFullscreen: {
      d: "M9 4v5H4M20 9h-5V4M4 15h5v5M15 20v-5h5",
    },
  };
  const setIcon = (element, name) => {
    const icon = iconPaths[name];
    if (!element || !icon) return;
    if (typeof document.createElementNS !== "function") {
      element.textContent =
        { play: "▶", pause: "❚❚", volume: "🔊", mute: "🔇" }[name] || "";
      return;
    }
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", icon.d);
    if (icon.fill) {
      path.setAttribute("fill", "currentColor");
      path.setAttribute("stroke", "none");
    }
    svg.append(path);
    element.replaceChildren(svg);
  };
  const esc = (v) => String(v ?? "");
  const mediaKinds = [
    ["", "全部"],
    ["movie", "电影"],
    ["series", "电视剧"],
    ["variety", "综艺"],
    ["anime", "动漫"],
    ["child", "儿童"],
    ["other", "其他"],
  ];
  const kindLabel = (kind) =>
    mediaKinds.find(([value]) => value === kind)?.[1] || "其他";
  const sourceQuality = (source) => {
    const remark = String(source.remark || "").trim();
    if (/4k|2160/i.test(remark)) return "4K";
    if (/2k|1440/i.test(remark)) return "2K";
    if (/1080/i.test(remark)) return "1080P";
    if (/720/i.test(remark)) return "720P";
    if (/480/i.test(remark)) return "480P";
    if (/hd|高清|超清/i.test(remark)) return "HD";
    return remark;
  };
  const matchesKind = (item) =>
    !state.onlineKind || (item.kind || "other") === state.onlineKind;
  const languageOptions = [
    ["", "全部语言"],
    ["国语", "国语"],
    ["粤语", "粤语"],
    ["英语", "英语"],
    ["日语", "日语"],
    ["韩语", "韩语"],
    ["其他", "其他语言"],
  ];
  const normalizedLanguages = (raw) => {
    const value = String(raw || "");
    const result = new Set();
    if (/国语|普通话|汉语普通话|华语/.test(value)) result.add("国语");
    if (/粤语|广东话|港语/.test(value)) result.add("粤语");
    if (/英语|英文/.test(value)) result.add("英语");
    if (/日语|日文/.test(value)) result.add("日语");
    if (/韩语|韩文/.test(value)) result.add("韩语");
    if (!result.size && value.trim()) result.add("其他");
    return result;
  };
  const languageLabel = (value) =>
    languageOptions.find(([key]) => key === value)?.[1] || "全部语言";
  const matchesLanguage = (item) => {
    const selected = state.onlineLang;
    if (!selected) return true;
    const languages = normalizedLanguages(item.lang);
    if (selected === "其他")
      return languages.size > 0 && [...languages].every((entry) => entry === "其他");
    return languages.has(selected);
  };
  const n = (v, fallback = 0) =>
    Number.isFinite(Number(v)) ? Number(v) : fallback;
  const admin = () => state.user?.role === "admin";
  const make = (tag, attrs = {}, children = []) => {
    const el = document.createElement(tag);
    Object.entries(attrs).forEach(([key, value]) => {
      if (value == null || value === false) return;
      if (key === "class") el.className = value;
      else if (key === "text") el.textContent = esc(value);
      else if (key === "on")
        Object.entries(value).forEach(([event, fn]) => {
          if (typeof fn === "function") el.addEventListener(event, fn);
        });
      else if (key === "style") el.style.cssText = value;
      else if (key === "checked" || key === "disabled" || key === "hidden")
        el[key] = Boolean(value);
      else el.setAttribute(key, value === true ? "" : value);
    });
    (Array.isArray(children) ? children : [children]).forEach((child) => {
      if (child == null) return;
      el.append(child.nodeType ? child : document.createTextNode(esc(child)));
    });
    return el;
  };
  const button = (label, className, fn, attrs = {}) =>
    make(
      "button",
      {
        type: "button",
        class: `button ${className || ""}`,
        on: { click: fn },
        ...attrs,
      },
      label,
    );
  const icon = (label, aria, fn, className = "") =>
    make(
      "button",
      {
        type: "button",
        class: `icon-button ${className}`,
        "aria-label": aria,
        title: aria,
        on: { click: fn },
      },
      label,
    );
  async function response(res) {
    const raw = await res.text();
    let data = null;
    try {
      data = raw ? JSON.parse(raw) : null;
    } catch (_) {}
    if (!res.ok) throw new Error(data?.error || `请求失败（${res.status}）`);
    return data;
  }
  async function api(path, options = {}) {
    const opts = {
      credentials: "same-origin",
      ...options,
      headers: { Accept: "application/json", ...(options.headers || {}) },
    };
    const method = (opts.method || "GET").toUpperCase();
    if (!["GET", "HEAD"].includes(method))
      opts.headers["X-PolyView-Request"] = "1";
    if (opts.body && typeof opts.body !== "string") {
      opts.headers["Content-Type"] = "application/json";
      opts.body = JSON.stringify(opts.body);
    }
    return response(await fetch(path, opts));
  }
  function toast(message, error = false) {
    const el = $("toast");
    el.textContent = esc(message);
    el.className = `toast show${error ? " error" : ""}`;
    el.hidden = false;
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => {
      el.hidden = true;
      el.className = "toast";
    }, 4000);
  }
  function empty(iconText, title, description, action) {
    const children = [
      make("div", { class: "empty-icon", text: iconText }),
      make("h2", { text: title }),
      make("p", { text: description }),
    ];
    if (action) children.push(action);
    return make("div", { class: "empty-state" }, children);
  }
  function head(title, description, action) {
    const h = make(
      "div",
      { class: "page-head" },
      make("div", {}, [
        make("h1", { text: title }),
        make("p", { text: description }),
      ]),
    );
    if (action) h.append(action);
    return h;
  }
  function sectionTitle(title, count, hint) {
    const children = [make("h2", { text: title })];
    if (count != null)
      children.push(make("span", { class: "count-pill", text: count }));
    if (hint)
      children.push(make("span", { class: "section-hint", text: hint }));
    return make("div", { class: "section-title" }, children);
  }
  function updateNavBadges() {
    const setBadge = (id, count) => {
      const badge = $(id);
      if (!badge) return;
      badge.textContent = count > 99 ? "99+" : String(count);
      badge.hidden = !count;
    };
    setBadge("series-badge", state.series.length);
    setBadge(
      "downloads-badge",
      state.downloads.filter((job) => !/done|complete|failed|cancel/i.test(job.status || "")).length,
    );
  }
  function setNav(page) {
    state.page = page;
    document.querySelectorAll("[data-view]").forEach((el) => {
      const on = el.dataset.view === page;
      el.classList.toggle("active", on);
      if (on) el.setAttribute("aria-current", "page");
      else el.removeAttribute("aria-current");
    });
    $("breadcrumb").replaceChildren(
      document.createTextNode("影音空间 "),
      make("span", { text: "/" }),
      document.createTextNode(
        ` ${{ online: "影视发现", library: "媒体库", live: "电视直播", series: "追剧清单", downloads: "下载任务", settings: "账户与设置" }[page]}`,
      ),
    );
    render();
  }
  function card(item, index) {
    const pct =
      n(item.duration) > 0
        ? Math.min(100, (n(item.position) / n(item.duration)) * 100)
        : 0;
    const label =
      item.kind === "episode"
        ? `S${String(item.season || 1).padStart(2, "0")} E${String(item.episode || 1).padStart(2, "0")}`
        : "电影";
    const poster = make(
      "button",
      {
        type: "button",
        class: "poster-button",
        "aria-label": `播放 ${item.title}`,
        on: { click: () => play(item) },
      },
      make("div", { class: `poster-art tone-${index % 6}` }, [
        make("span", { class: "poster-label", text: label }),
        make("span", { class: "poster-name", text: item.title }),
        make("span", {
          class: "poster-play",
          "aria-hidden": "true",
          text: "▶",
        }),
        pct
          ? make("progress", { class: "media-progress", max: 100, value: pct })
          : null,
      ]),
    );
    const fav = icon(
      item.favorite ? "★" : "☆",
      item.favorite ? "取消收藏" : "收藏",
      async () => {
        try {
          const result = await api(
            `/api/library/${encodeURIComponent(item.id)}/favorite`,
            { method: "PUT", body: { favorite: !item.favorite } },
          );
          item.favorite = result?.favorite ?? !item.favorite;
          render();
        } catch (e) {
          toast(e.message, true);
        }
      },
      `favorite-button${item.favorite ? " is-favorite" : ""}`,
    );
    const meta = item.series ? `${item.series} · ${label}` : label;
    return make("article", { class: "media-card" }, [
      poster,
      fav,
      make("div", { class: "media-meta" }, [
        make("span", { text: meta }),
        item.watched
          ? make("span", { class: "watched", text: "✓ 已看" })
          : null,
      ]),
    ]);
  }
  async function library() {
    const result = await api(
      `/api/library${state.query ? `?q=${encodeURIComponent(state.query)}` : ""}`,
    );
    state.library = Array.isArray(result) ? result : result?.items || [];
    const list = state.library.filter(
      (item) =>
        state.filter === "all" ||
        (state.filter === "movie" && item.kind === "movie") ||
        (state.filter === "episode" && item.kind === "episode") ||
        (state.filter === "favorite" && item.favorite),
    );
    const root = make("div");
    root.append(
      head(
        "你的媒体库",
        state.library.length
          ? "今天晚上，想看点什么？"
          : "把属于你的故事，放进属于你的空间。",
        admin() ? button("扫描媒体目录", "primary", scan) : null,
      ),
    );
    if (!state.library.length) {
      root.append(
        make(
          "div",
          { class: "hero" },
          make("div", { class: "hero-copy" }, [
            make("p", {
              class: "eyebrow",
              text: "A QUIET PLACE FOR GOOD STORIES",
            }),
            make("h2", {
              text: admin() ? "从你的第一部电影开始。" : "媒体库还在等待内容。",
            }),
            make("p", {
              text: admin()
                ? "扫描媒体目录，PolyView 会自动整理电影和剧集。"
                : "请让管理员扫描媒体目录，之后你就可以在这里观看。",
            }),
            admin() ? button("扫描媒体目录", "primary", scan) : null,
          ]),
        ),
      );
      return root;
    }
    const filters = make("div", { class: "filters" });
    [
      ["all", "全部"],
      ["movie", "电影"],
      ["episode", "剧集"],
      ["favorite", "收藏"],
    ].forEach(([key, label]) =>
      filters.append(
        make(
          "button",
          {
            type: "button",
            class: `filter-chip${state.filter === key ? " active" : ""}`,
            "aria-pressed": String(state.filter === key),
            on: {
              click: () => {
                state.filter = key;
                render();
              },
            },
          },
          label,
        ),
      ),
    );
    filters.append(
      make("span", { class: "sort-note", text: `${list.length} 项内容` }),
    );
    root.append(filters);
    const continuing = state.library
      .filter((item) => n(item.duration) > 0 && n(item.position) > 0 && n(item.position) < n(item.duration) * 0.95)
      .sort((one, other) => n(other.position) - n(one.position))
      .slice(0, 8);
    if (continuing.length) {
      const strip = make("div", { class: "continue-grid" });
      continuing.forEach((item, index) => strip.append(card(item, index)));
      root.append(
        make("section", { class: "continue-section" }, [
          sectionTitle("继续观看", continuing.length, "接着上次的位置看"),
          strip,
        ]),
      );
    }
    if (state.filter === "episode") {
      const groups = new Map();
      list.forEach((item) => {
        const name = item.series || "未分组剧集";
        if (!groups.has(name)) groups.set(name, []);
        groups.get(name).push(item);
      });
      groups.forEach((items, name) => {
        const grid = make("div", { class: "media-grid" });
        items.forEach((item, index) => grid.append(card(item, index)));
        root.append(
          make("section", { class: "library-series" }, [
            sectionTitle(name, items.length),
            grid,
          ]),
        );
      });
      if (!groups.size)
        root.append(
          empty("⌕", "没有找到相关内容", "换一个关键词，或切换媒体筛选。"),
        );
    } else {
      const grid = make("div", { class: "media-grid" });
      if (!list.length)
        grid.append(
          empty("⌕", "没有找到相关内容", "换一个关键词，或切换媒体筛选。"),
        );
      else list.forEach((item, index) => grid.append(card(item, index)));
      root.append(grid);
    }
    return root;
  }
  async function scan() {
    try {
      const result = await api("/api/library/scan", {
        method: "POST",
        body: {},
      });
      toast(`扫描完成：新增 ${result?.added || 0} 项`);
      render();
    } catch (e) {
      toast(e.message, true);
    }
  }
  function destroyHls() {
    if (state.hls) {
      state.hls.destroy();
      state.hls = null;
    }
  }
  function liveSources(item) {
    const sources = (item.sources || [])
      .map((source) => (typeof source === "string" ? source : source.url))
      .filter(Boolean);
    return sources.length ? sources : item.url ? [item.url] : [];
  }
  function updateSourceButton() {
    const player = state.player;
    const button = $("player-source");
    if (!player?.live || player.sources.length < 2) {
      button.hidden = true;
      return;
    }
    button.hidden = false;
    button.textContent = `线路 ${player.sourceIndex + 1}/${player.sources.length} · 切换`;
  }
  function updatePlayerControls() {
    const player = state.player;
    const video = $("video-player");
    const live = Boolean(player?.live);
    for (const id of ["player-toggle", "player-mute", "player-fullscreen"])
      $(id).hidden = !live;
    const playlistSize = player?.playlist?.length || 0;
    $("player-prev").hidden =
      !playlistSize || player.playlistIndex <= 0;
    $("player-next").hidden =
      !playlistSize || player.playlistIndex >= playlistSize - 1;
    video.controls = !live;
    document.body.dataset.playing = video.paused ? "0" : "1";
    if (!live) return;
    setIcon($("player-toggle"), video.paused ? "play" : "pause");
    $("player-toggle").setAttribute("aria-label", video.paused ? "播放" : "暂停");
    setIcon(
      $("player-mute"),
      video.muted || video.volume === 0 ? "mute" : "volume",
    );
    $("player-mute").setAttribute(
      "aria-label",
      video.muted || video.volume === 0 ? "取消静音" : "静音",
    );
  }
  function failLiveSource() {
    const player = state.player;
    if (!player?.live) return;
    player.failed.add(player.sourceIndex);
    const next = Array.from(
      { length: player.sources.length },
      (_, offset) =>
        (player.sourceIndex + offset + 1) % player.sources.length,
    ).find((index) => !player.failed.has(index));
    if (next === undefined) {
      $("player-message").textContent =
        "所有线路都无法播放，请检查频道地址或稍后重试。";
      destroyHls();
      updateSourceButton();
      return;
    }
    player.sourceIndex = next;
    $("player-message").textContent = `正在切换线路 ${next + 1}/${player.sources.length}…`;
    attachPlayerSource();
  }
  function switchLiveSource(delta) {
    const player = state.player;
    if (!player?.live || player.sources.length < 2) return;
    player.failed = new Set();
    player.sourceIndex =
      (player.sourceIndex + delta + player.sources.length) %
      player.sources.length;
    attachPlayerSource();
  }
  function attachPlayerSource() {
    const player = state.player;
    if (!player) return;
    const video = $("video-player");
    const attempt = ++player.attempt;
    const current = () => state.player === player && player.attempt === attempt;
    destroyHls();
    video.pause();
    video.onloadedmetadata = null;
    video.onplaying = null;
    video.onerror = null;
    video.removeAttribute("src");
    video.load();
    updateSourceButton();
    $("player-message").textContent = player.live
      ? player.sources.length > 1
        ? `正在连接线路 ${player.sourceIndex + 1}/${player.sources.length}…`
        : "直播由服务器转发，请确认服务器能访问频道地址。"
      : "使用播放器控制开始播放。按 Esc 关闭。";
    const upstream = player.live
      ? player.sources[player.sourceIndex]
      : player.item.remote_url;
    const source = upstream
      ? `/api/playback?url=${encodeURIComponent(upstream)}`
      : `/api/library/${encodeURIComponent(player.item.id)}/stream`;
    const hlsSource = /\.m3u8(?:[?#]|$)/i.test(upstream || source);
    const nativeHls =
      video.canPlayType("application/vnd.apple.mpegurl") === "probably";
    const startLive = () => {
      if (!player.live || !current()) return;
      const result = video.play();
      if (result?.catch)
        result.catch((error) => {
          if (!current()) return;
          if (error?.name === "NotAllowedError")
            $("player-message").textContent =
              "浏览器阻止了自动播放，请点击播放器开始直播。";
        });
    };
    video.onerror = () => {
      if (!current()) return;
      if (player.live) failLiveSource();
      else
        $("player-message").textContent =
          "视频无法播放，请检查文件格式或媒体目录权限。";
    };
    if (player.live)
      video.onplaying = () => {
        if (current()) $("player-message").textContent = "正在直播。";
      };
    if (hlsSource && !nativeHls && window.Hls?.isSupported()) {
      const hls = new window.Hls({
        enableWorker: false,
        liveSyncDurationCount: 4,
        liveMaxLatencyDurationCount: 12,
        maxBufferLength: 30,
        maxMaxBufferLength: 90,
        backBufferLength: 15,
        manifestLoadingMaxRetry: 3,
        levelLoadingMaxRetry: 5,
        fragLoadingMaxRetry: 6,
      });
      state.hls = hls;
      hls.on(window.Hls.Events.ERROR, (_, data) => {
        if (!data.fatal || !current()) return;
        if (player.live) failLiveSource();
        else {
          $("player-message").textContent =
            "HLS 连接失败，请确认服务器能访问媒体地址或视频编码受支持。";
          destroyHls();
        }
      });
      hls.on(window.Hls.Events.MANIFEST_PARSED, startLive);
      hls.loadSource(source);
      hls.attachMedia(video);
    } else {
      video.src = source;
      video.load();
      if (player.live) {
        video.onloadedmetadata = startLive;
        startLive();
      }
    }
    if (!player.live)
      video.onloadedmetadata = () => {
        if (!current()) return;
        if (
          n(player.item.position) > 0 &&
          n(player.item.position) < video.duration - 1
        )
          video.currentTime = player.item.position;
      };
  }
  function playerGuide(item) {
    const box = $("player-guide");
    box.replaceChildren();
    if (!state.player?.live) {
      box.hidden = true;
      return;
    }
    const now = Math.floor(Date.now() / 1000);
    const programs = (state.epg?.programs || [])
      .filter(
        (program) =>
          Number(program.channel_id) === Number(item.id) &&
          program.stop > now,
      )
      .sort((one, other) => one.start - other.start);
    if (!programs.length) {
      box.hidden = true;
      return;
    }
    box.hidden = false;
    box.append(
      make(
        "div",
        { class: "player-guide-head" },
        [
          make("strong", { text: "接下来" }),
          make("small", { text: `${programs.length} 条节目` }),
        ],
      ),
    );
    programs.slice(0, 14).forEach((program) => {
      const current = now >= program.start && now < program.stop;
      const children = [
        make("span", { class: "player-guide-time", text: clockTime(program.start) }),
        make("strong", { text: program.title }),
      ];
      if (current)
        children.push(
          make("progress", {
            class: "guide-progress",
            max: 100,
            value: Math.round(
              ((now - program.start) / Math.max(1, program.stop - program.start)) *
                100,
            ),
          }),
        );
      box.append(
        make(
          "div",
          { class: `player-guide-item${current ? " current" : ""}` },
          children,
        ),
      );
    });
  }
  function play(item, live = false, playlist = [], playlistIndex = 0) {
    if (state.player) closePlayer();
    state.player = {
      item,
      live,
      sources: live ? liveSources(item) : [],
      playlist: live ? [] : playlist,
      playlistIndex,
      sourceIndex: 0,
      failed: new Set(),
      attempt: 0,
      lastSaved: 0,
      lastPayload: "",
      errorShown: false,
    };
    $("player-title").textContent = item.title || item.name || "正在播放";
    $("player-kind").textContent = live
      ? "LIVE NOW"
      : item.series || "NOW PLAYING";
    $("player-dialog").classList.toggle("live-mode", live);
    $("player-stage").classList.toggle("live-mode", live);
    $("player-dialog").showModal();
    playerGuide(item);
    updatePlayerControls();
    $("player-hud-title").textContent = $("player-title").textContent;
    $("player-hud-kind").textContent = $("player-kind").textContent;
     attachPlayerSource();
  }

  function playPlaylist(offset) {
    const current = state.player;
    if (!current || current.live || !current.playlist?.length) return;
    const index = current.playlistIndex + offset;
    if (index < 0 || index >= current.playlist.length) return;
    const item = current.playlist[index];
    play(item, false, current.playlist, index);
  }
  async function saveProgress(force = false, keepalive = false) {
    const current = state.player,
      video = $("video-player");
    if (
      !current ||
      current.live ||
      !Number.isFinite(video.currentTime) ||
      !Number.isFinite(video.duration) ||
      video.duration <= 0
    )
      return;
    if (!force && Date.now() - current.lastSaved < 10000) return;
    if (current.item.remote_url) {
      if (
        current.item.tracking_id &&
        (video.ended || video.currentTime / video.duration > 0.9) &&
        !current.item.marked_watched
      ) {
        current.item.marked_watched = true;
        try {
          await api(`/api/tracking/${current.item.tracking_id}`, {
            method: "PUT",
            body: {
              watched_episodes: Math.max(
                current.item.watched_episodes || 0,
                current.item.episode_number,
              ),
            },
            keepalive,
          });
        } catch (error) {
          current.item.marked_watched = false;
          toast(`追剧进度未保存：${error.message}`, true);
        }
      }
      current.lastSaved = Date.now();
      return;
    }
    const body = {
      position: Math.min(video.duration, Math.max(0, n(video.currentTime))),
      duration: video.duration,
      watched: Boolean(
        current.item.watched ||
          video.ended ||
          video.currentTime / video.duration > 0.9,
      ),
    };
    const payload = JSON.stringify(body);
    if (payload === current.lastPayload) return;
    current.lastSaved = Date.now();
    current.lastPayload = payload;
    Object.assign(current.item, body);
    try {
      await api(
        `/api/library/${encodeURIComponent(current.item.id)}/progress`,
        { method: "PUT", body, keepalive },
      );
      current.errorShown = false;
    } catch (e) {
      current.lastPayload = "";
      if (!current.errorShown) {
        toast(`观看进度未保存：${e.message}`, true);
        current.errorShown = true;
      }
    }
  }
  function closePlayer() {
    if (!state.player) return;
    saveProgress(true, true).then(() => {
      if (
        state.user &&
        !state.player &&
        ["library", "series"].includes(state.page)
      )
        render();
    });
    state.player = null;
    updatePlayerControls();
    $("player-source").hidden = true;
    $("player-guide").hidden = true;
    $("player-dialog").classList.remove("live-mode");
    $("player-stage").classList.remove("live-mode");
    const video = $("video-player");
    video.pause();
    destroyHls();
    video.onloadedmetadata = null;
    video.onplaying = null;
    video.onerror = null;
    video.removeAttribute("src");
    video.load();
    if ($("player-dialog").open) $("player-dialog").close();
  }
  const EPG_MINUTE_PX = 2;
  function guideStart(now = Date.now() / 1000) {
    return Math.floor(now / 300) * 300;
  }
  function clockTime(timestamp) {
    const date = new Date(timestamp * 1000);
    return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
  }
  async function refreshEpg() {
    try {
      const result = await api("/api/epg/refresh", { method: "POST" });
      toast(`节目单已更新：${result.matched_channels} 个频道`);
      await render();
    } catch (error) {
      toast(error.message, true);
    }
  }
  function channelsWithPrograms(channels = state.channels, programs = state.epg?.programs || []) {
    const covered = new Set(programs.map((program) => program.channel_id));
    return channels.filter((channel) => covered.has(channel.id));
  }
  function guideView(start, end, now = Math.floor(Date.now() / 1000)) {
    const guide = state.epg || {};
    const programs = Array.isArray(guide.programs) ? guide.programs : [];
    const byChannel = new Map();
    programs.forEach((program) => {
      if (!byChannel.has(program.channel_id)) byChannel.set(program.channel_id, []);
      byChannel.get(program.channel_id).push(program);
    });
    const groups = [];
    state.channels.forEach((channel) => {
      const group = channel.group_name || "其他";
      if (!groups.includes(group)) groups.push(group);
    });
    if (state.liveGroup !== "all" && !groups.includes(state.liveGroup))
      state.liveGroup = "all";
    const query = state.query.toLowerCase();
    const channels = state.channels.filter(
      (channel) =>
        (state.liveGroup === "all" || (channel.group_name || "其他") === state.liveGroup) &&
        (!query || `${channel.name} ${channel.group_name}`.toLowerCase().includes(query)),
    );
    const tabs = make("div", { class: "live-groups" }, [
      button(
        `全部 ${state.channels.length}`,
        `subtle small-button${state.liveGroup === "all" ? " active" : ""}`,
        () => {
          state.liveGroup = "all";
          $("live-guide").replaceChildren(guideView(start, end, now));
        },
      ),
      ...groups.map((group) =>
        button(
          group,
          `subtle small-button${state.liveGroup === group ? " active" : ""}`,
          () => {
            state.liveGroup = group;
            $("live-guide").replaceChildren(guideView(start, end, now));
          },
        ),
      ),
    ]);
    let scroller;
    const currentOnly = button(
      state.epgOnlyCurrent ? "显示全部节目" : "只看当前",
      "subtle small-button",
      () => {
        state.epgOnlyCurrent = !state.epgOnlyCurrent;
        $("live-guide").replaceChildren(guideView(start, end, now));
      },
    );
    const jumpNow = button("回到现在", "subtle small-button", () => {
      if (scroller)
        scroller.scrollLeft = ((now - start) / 60) * EPG_MINUTE_PX;
    });
    const updated = Number(guide.updated_at) || 0;
    const toolbar = make("div", { class: "epg-toolbar" }, [
      make("div", { class: "epg-meta" }, [
        make("span", {
          text: `从当前节目开始 · ${
            updated
              ? `节目单更新于 ${clockTime(updated)} · ${guide.matched_channels || 0} 个频道`
              : "尚未更新节目单"
          }`,
        }),
      ]),
      make("div", { class: "epg-actions" }, [
        currentOnly,
        jumpNow,
        admin() ? button("刷新节目单", "subtle small-button", refreshEpg) : null,
      ]),
    ]);
    scroller = make("div", { class: "epg-scroll" });
    const inner = make("div", {
      class: "epg-inner",
      style: `width:${170 + 1440 * EPG_MINUTE_PX}px`,
    });
    const rulerRow = make("div", { class: "epg-row epg-ruler-row" });
    rulerRow.append(make("div", { class: "epg-channel epg-corner", text: "频道" }));
    const ruler = make("div", {
      class: "epg-track epg-ruler",
      style: `width:${1440 * EPG_MINUTE_PX}px`,
    });
    for (
      let hour = Math.ceil(start / 3600) * 3600;
      hour < end;
      hour += 3600
    ) {
      ruler.append(
        make("span", {
          class: "epg-hour",
          style: `left:${((hour - start) / 60) * EPG_MINUTE_PX}px`,
          text: clockTime(hour),
        }),
      );
    }
    rulerRow.append(ruler);
    inner.append(rulerRow);
    channels.forEach((channel) => {
      const row = make("div", { class: "epg-row" });
      row.append(
        make(
          "button",
          {
            type: "button",
            class: "epg-channel",
            "aria-label": `播放 ${channel.name}`,
            on: { click: () => play(channel, true) },
          },
          [
            channel.logo
              ? make("img", { class: "epg-logo", src: channel.logo, alt: "" })
              : make("span", { class: "epg-logo", text: "◉" }),
            make("span", { class: "epg-channel-copy" }, [
              make("strong", { text: channel.name }),
              make("small", {
                text: `${channel.group_name || "直播"} · ${(channel.sources || []).length || 1} 线路`,
              }),
            ]),
          ],
        ),
      );
      const track = make("div", {
        class: "epg-track",
        style: `width:${1440 * EPG_MINUTE_PX}px`,
      });
      const items = (byChannel.get(channel.id) || []).filter(
        (program) =>
          program.stop > now &&
          (!state.epgOnlyCurrent ||
            (now >= program.start && now < program.stop)),
      );
      if (!items.length) {
        track.append(make("span", { class: "epg-nodata", text: "暂无节目单" }));
      }
      items.forEach((program) => {
        const left = Math.max(0, ((program.start - start) / 60) * EPG_MINUTE_PX);
        const right = Math.min(
          1440 * EPG_MINUTE_PX,
          ((program.stop - start) / 60) * EPG_MINUTE_PX,
        );
        if (right - left < 2) return;
        const classes = [
          "epg-program",
          now >= program.start && now < program.stop ? "current" : "",
        ]
          .filter(Boolean)
          .join(" ");
        track.append(
          make(
            "button",
            {
              type: "button",
              class: classes,
              style: `left:${left}px;width:${right - left}px`,
              title: `${program.title} ${clockTime(program.start)}-${clockTime(program.stop)}${
                program.description ? `\n${program.description}` : ""
              }`,
              on: { click: () => play(channel, true) },
            },
              [
                make("strong", { text: program.title }),
                make("small", {
                  text: `${clockTime(program.start)} - ${clockTime(program.stop)}`,
                }),
                now >= program.start && now < program.stop
                  ? make("progress", {
                      class: "epg-program-progress",
                      max: 100,
                      value: Math.round(
                        ((now - program.start) /
                          Math.max(1, program.stop - program.start)) *
                          100,
                      ),
                    })
                  : null,
              ].filter(Boolean),
          ),
        );
      });
      row.append(track);
      inner.append(row);
    });
    if (now >= start && now < end) {
      inner.append(
        make("span", {
          class: "epg-now",
          style: `left:${170 + ((now - start) / 60) * EPG_MINUTE_PX}px`,
        }),
      );
    }
    scroller.append(inner);
    return make("div", { class: "live-guide" }, [
      toolbar,
      tabs,
      channels.length
        ? scroller
        : empty("◉", "没有匹配的频道", "换一个分组或搜索词试试。"),
    ]);
  }
  async function live() {
    const result = await api("/api/channels");
    const allChannels = Array.isArray(result) ? result : result?.channels || [];
    state.channels = allChannels;
    const now = Math.floor(Date.now() / 1000);
    const start = guideStart(now);
    const end = start + 86400;
    try {
      state.epg = await api(`/api/epg?start=${now - 6 * 3600}&end=${end}`);
    } catch (error) {
      state.epg = { programs: [], updated_at: 0, error: error.message };
    }
    state.channels = channelsWithPrograms();
    const root = make(
      "div",
      {},
      head(
        "电视直播",
        "像翻节目单一样，挑一个正在播的节目。",
        make("div", { class: "head-actions" }, [
          admin() ? button("导入 M3U", "primary", importM3u) : null,
        ]),
      ),
    );
    if (!state.channels.length) {
      const hasChannels = allChannels.length > 0;
      root.append(
        empty(
          "◉",
          hasChannels ? "节目单暂不可用" : "还没有直播频道",
          hasChannels
            ? "当前频道还没有可显示的节目，请刷新节目单或稍后再试。"
            : admin()
              ? "导入一个 M3U 播放列表，把电视带回家。"
              : "请让管理员导入 M3U 播放列表。",
          admin() && !hasChannels ? button("导入 M3U", "primary", importM3u) : null,
        ),
      );
      return root;
    }
    const guide = make("div", { id: "live-guide" });
    guide.append(guideView(start, end, now));
    root.append(guide);
    return root;
  }
  function importM3u() {
    let dialog = $("import-dialog");
    if (!dialog) {
      const area = make("textarea", {
        id: "import-playlist",
        required: true,
        maxlength: 1900000,
        "aria-label": "M3U 频道列表",
        placeholder:
          "#EXTM3U\n#EXTINF:-1,频道名称\nhttps://example.com/live.m3u8",
      });
      const file = make("input", {
        type: "file",
        accept: ".m3u,.m3u8,text/plain",
        "aria-label": "上传 M3U 文件",
        on: {
          change: async (event) => {
            const selected = event.target.files[0];
            if (!selected) return;
            if (selected.size > 1900000) {
              toast("播放列表不能超过 1.9 MB", true);
              return;
            }
            try {
              area.value = await selected.text();
            } catch (_) {
              toast("无法读取所选文件", true);
            }
          },
        },
      });
      dialog = make(
        "dialog",
        {
          id: "import-dialog",
          class: "info-dialog",
          "aria-labelledby": "import-title",
        },
        [
          make("div", { class: "dialog-top" }, [
            make("h2", { id: "import-title", text: "导入 M3U 频道" }),
            icon("✕", "关闭频道导入", () => dialog.close()),
          ]),
          make("p", {
            class: "hint",
            text: "粘贴多行播放列表，或选择本地 M3U / M3U8 文件。",
          }),
          make(
            "form",
            {
              class: "stack",
              on: {
                submit: async (event) => {
                  event.preventDefault();
                  const submit =
                    event.currentTarget.querySelector("[type=submit]");
                  submit.disabled = true;
                  try {
                    const result = await api("/api/channels/import", {
                      method: "POST",
                      body: { playlist: area.value },
                    });
                    toast(`已导入 ${result?.imported || 0} 个频道`);
                    dialog.close();
                    area.value = "";
                    render();
                  } catch (e) {
                    toast(e.message, true);
                  } finally {
                    submit.disabled = false;
                  }
                },
              },
            },
            [
              file,
              area,
              button("导入频道", "primary", null, { type: "submit" }),
            ],
          ),
        ],
      );
      document.body.append(dialog);
    }
    dialog.showModal();
  }
  const stripTrailingYear = (value, year) => {
    const match = value.match(
      /([\s.·\-]*[\(（]?)((?:19|20)\d{2})([\)）]?)$/u,
    );
    if (!match) return value;
    const prefix = value.slice(0, match.index).trim();
    if (!prefix) return value;
    const [, lead, foundYear, trail] = match;
    const expectedYear = String(year ?? "").trim();
    const delimited =
      /[.·\-（(]/u.test(lead) || /[)）]/u.test(trail);
    if (expectedYear && foundYear === expectedYear) return prefix;
    if (!expectedYear && delimited) return prefix;
    return value;
  };
  const normalizedCatalogTitle = (title, year) => {
    const base = String(title ?? "")
      .normalize("NFKC")
      .trim()
      .toLocaleLowerCase();
    const value = stripTrailingYear(base, year);
    const cleaned = value.replace(
      /[\s:：,，.。!！?？、·・\-—_()（）\[\]【】《》"“”'’]/gu,
      "",
    );
    return cleaned || base;
  };
  const displayCatalogTitle = (title, year) => {
    const value = String(title ?? "").normalize("NFKC").trim();
    return stripTrailingYear(value, year).trim() || value;
  };
  const sourceRank = (source) => {
    const quality = sourceQuality(source).toUpperCase();
    if (quality.includes("4K") || quality.includes("2160")) return 5;
    if (quality.includes("2K") || quality.includes("1440")) return 4;
    if (quality.includes("1080")) return 3;
    if (quality.includes("720")) return 2;
    if (quality.includes("HD") || quality.includes("高清")) return 1;
    return 0;
  };
  const catalogGroupKey = (item) => {
    const year = String(item.year ?? "").trim();
    const title = normalizedCatalogTitle(item.title, year);
    return title
      ? `${item.kind || "other"}|${title}|${year}`
      : `id:${item.source_id}:${item.id ?? item.media_id}`;
  };
  const mergeCatalogGroups = (items, groups) => {
    const changed = new Set();
    items.filter((item) => matchesKind(item) && matchesLanguage(item)).forEach((item) => {
      const key = catalogGroupKey(item);
      let group = groups.get(key);
      if (!group) {
        group = {
          ...item,
          title: displayCatalogTitle(item.title, item.year),
          sources: [],
        };
        groups.set(key, group);
      }
      if (!group.cover && item.cover) {
        group.cover = item.cover;
        changed.add(key);
      }
      if (!group.year && item.year) group.year = item.year;
      if (!group.remark && item.remark) group.remark = item.remark;
      const source = {
        source_id: item.source_id,
        id: item.id ?? item.media_id,
        source_name: item.source_name,
        remark: item.remark,
        lang: item.lang,
      };
      const existingIndex = group.sources.findIndex(
        (entry) => String(entry.source_id) === String(source.source_id),
      );
      if (existingIndex === -1) {
        group.sources.push(source);
        changed.add(key);
      } else {
        const existing = group.sources[existingIndex];
        if (
          sourceRank(source) > sourceRank(existing) ||
          (!existing.remark && source.remark)
        ) {
          group.sources[existingIndex] = source;
          changed.add(key);
        }
      }
      group.sources.sort(
        (left, right) =>
          sourceRank(right) - sourceRank(left) ||
          String(left.source_name).localeCompare(String(right.source_name), "zh-CN"),
      );
      const best = group.sources[0];
      if (best) {
        group.source_id = best.source_id;
        group.id = best.id;
        group.source_name = best.source_name;
        group.remark = best.remark || group.remark;
      }
    });
    return changed;
  };
  const updateCatalogCard = (card, item) => {
    if (item.cover && !card._cover && card._art) {
      const image = make("img", {
        src: item.cover,
        alt: "",
        loading: "lazy",
        referrerpolicy: "no-referrer",
      });
      image.addEventListener("error", () => image.remove());
      card._art.prepend(image);
      card._cover = item.cover;
    }
    const quality = sourceQuality(item.sources?.[0] || item);
    const qualityLabel = /4K|2K|1080P|720P|480P|HD/i.test(quality)
      ? quality
      : "";
    card._quality.hidden = !qualityLabel;
    card._quality.textContent = qualityLabel;
    const count = item.sources?.length || 1;
    card._sourceBadge.hidden = count < 2;
    card._sourceBadge.textContent = `聚合 ${count} 源`;
    card._kindBadge.textContent = kindLabel(item.kind);
    card._meta.textContent = [
      item.year,
      count > 1 ? `${count} 个影视源` : item.remark,
      count > 1 ? "点击切换" : item.source_name,
    ]
      .filter(Boolean)
      .join(" · ");
  };
  function catalogCard(item, index) {
    const art = make(
      "div",
      { class: `poster-art catalog-art tone-${index % 6}` },
      [make("span", { class: "poster-name", text: item.title })],
    );
    const badge = make("span", {
      class: "source-badge",
      hidden: true,
      "aria-hidden": "true",
    });
    const kindBadge = make("span", {
      class: "kind-badge",
      text: kindLabel(item.kind),
    });
    const quality = make("span", {
      class: "quality-badge",
      hidden: true,
      "aria-hidden": "true",
    });
    art.append(badge, quality, kindBadge);
    const meta = make("div", { class: "media-meta" });
    const card = make("article", { class: "media-card" }, [
      make(
        "button",
        {
          type: "button",
          class: "poster-button",
          "aria-label": `查看 ${item.title}`,
          on: { click: () => openCatalogDetail(item) },
        },
        art,
      ),
      meta,
    ]);
    card._art = art;
    card._cover = "";
    card._sourceBadge = badge;
    card._quality = quality;
    card._kindBadge = kindBadge;
    card._meta = meta;
    updateCatalogCard(card, item);
    return card;
  }

  function spotlightCard(group) {
    const art = make("div", { class: "spotlight-art tone-0" });
    if (group.cover)
      art.append(
        make("img", {
          src: group.cover,
          alt: "",
          loading: "lazy",
          referrerpolicy: "no-referrer",
        }),
      );
    const quality = sourceQuality(group.sources?.[0] || group);
    const meta = [
      group.year,
      `${group.sources?.length || 1} 个影视源`,
      /4K|2K|1080P|720P|HD/i.test(quality) ? quality : "",
      kindLabel(group.kind),
    ]
      .filter(Boolean)
      .join(" · ");
    return make("section", { class: "spotlight", "aria-label": "焦点影片" }, [
      art,
      make("div", { class: "spotlight-copy" }, [
        make("p", { class: "eyebrow", text: "本周焦点" }),
        make("h2", { text: displayCatalogTitle(group.title, group.year) }),
        make("p", { class: "spotlight-meta", text: meta }),
        make("div", { class: "spotlight-actions" }, [
          button("立即播放", "primary", () => openCatalogDetail(group)),
          button("查看片源", "subtle", () =>
            $("content").querySelector(".media-grid .poster-button")?.click(),
          ),
        ]),
      ]),
    ]);
  }
  async function online() {
    const sequence = renderSequence;
    state.onlineObserver?.disconnect();
    state.onlineObserver = null;
    const configured = await api("/api/sources");
    const enabled = configured.filter((source) => source.enabled);
    const root = make(
      "div",
      {},
      head("影视发现", "搜索影片、在线观看，把想追的故事加入清单。"),
    );
    if (!enabled.length) {
      root.append(
        empty(
          "◈",
          "先连接一个影视源",
          admin()
            ? "在账户与设置中添加 MacCMS JSON 接口，即可浏览、搜索与播放。"
            : "请管理员配置影视源。",
          admin()
            ? button("配置影视源", "primary", () => setNav("settings"))
            : null,
        ),
      );
      return root;
    }
    let result;
    const groups = new Map();
    const groupOrder = [];
    const normalizedQuery = state.query
      ? normalizedCatalogTitle(state.query)
      : "";
    const queryRelevance = (group) => {
      if (!normalizedQuery) return 0;
      const title = normalizedCatalogTitle(group.title, group.year);
      if (title === normalizedQuery) return 3;
      if (title.startsWith(normalizedQuery)) return 2;
      if (title.includes(normalizedQuery)) return 1;
      return 0;
    };
    const compareGroupKeys = (leftKey, rightKey) => {
      const left = groups.get(leftKey);
      const right = groups.get(rightKey);
      if (normalizedQuery) {
        const diff = queryRelevance(right) - queryRelevance(left);
        if (diff !== 0) return diff;
      }
      const leftSources = left.sources?.length || 1;
      const rightSources = right.sources?.length || 1;
      if (rightSources !== leftSources) return rightSources - leftSources;
      const leftYear = Number(left.year) || 0;
      const rightYear = Number(right.year) || 0;
      if (rightYear !== leftYear) return rightYear - leftYear;
      return String(left.title).localeCompare(String(right.title), "zh-CN");
    };
    const syncGroupOrder = () => {
      const renderedKeys = new Set(groupOrder.slice(0, rendered));
      const tail = [...groups.keys()]
        .filter((key) => !renderedKeys.has(key))
        .sort(compareGroupKeys);
      groupOrder.splice(rendered, groupOrder.length - rendered, ...tail);
    };
    const paramsFor = (page) => {
      const params = new URLSearchParams({ page: String(page) });
      if (state.onlineSource !== "all")
        params.set("source_id", state.onlineSource);
      if (state.onlineKind) params.set("kind", state.onlineKind);
      if (state.onlineCategory) params.set("category", state.onlineCategory);
      if (state.onlineYear) params.set("year", state.onlineYear);
      return params;
    };
    const chipRow = (label, values, current, apply) =>
      make("div", { class: "discover-filter-row" }, [
        make("span", { class: "discover-filter-label", text: label }),
        make(
          "div",
          { class: "discover-filter-chips" },
          values.map(([value, text]) =>
            make("button", {
              type: "button",
              class: `filter-chip${value === current ? " active" : ""}`,
              "aria-pressed": String(value === current),
              text,
              on: {
                click: () => {
                  apply(value);
                  state.onlinePage = 1;
                  render();
                },
              },
            }),
          ),
        ),
      ]);
    const chooseKind = (value) => {
      state.onlineKind = value;
      state.onlineCategory = "";
      state.categories = null;
    };
    if (state.query) {
      result = await api(
        `/api/catalog/search?q=${encodeURIComponent(state.query)}`,
      );
      mergeCatalogGroups(result.results, groups);
      root.append(
        make("div", { class: "discover-filter-bar" }, [
          chipRow("类型", mediaKinds, state.onlineKind, chooseKind),
          chipRow("语言", languageOptions, state.onlineLang, (value) => (state.onlineLang = value)),
        ]),
      );
      root.append(
        sectionTitle(
          `“${state.query}” 的搜索结果`,
          groups.size,
          "同名影片已聚合，点击可切换影视源",
        ),
      );
    } else {
      if (
        state.onlineSource !== "all" &&
        !enabled.some((source) => String(source.id) === state.onlineSource)
      )
        state.onlineSource = "all";
      if (!state.categories) {
        try {
          const categoryUrl = state.onlineKind
            ? `/api/catalog/categories?kind=${encodeURIComponent(state.onlineKind)}`
            : "/api/catalog/categories";
          state.categories = (await api(categoryUrl)).categories || [];
        } catch {
          state.categories = [];
        }
      }
      if (
        state.onlineCategory &&
        !state.categories.includes(state.onlineCategory)
      )
        state.onlineCategory = "";
      const year = new Date().getFullYear();
      root.append(
        make("div", { class: "discover-filter-bar" }, [
          chipRow("类型", mediaKinds, state.onlineKind, chooseKind),
          chipRow("语言", languageOptions, state.onlineLang, (value) => (state.onlineLang = value)),
          chipRow(
            "影视源",
            [
              ["all", "全部影视源"],
              ...enabled.map((source) => [String(source.id), source.name]),
            ],
            state.onlineSource,
            (value) => (state.onlineSource = value),
          ),
          chipRow(
            "分类",
            [
              ["", "全部分类"],
              ...state.categories.map((category) => [category, category]),
            ],
            state.onlineCategory,
            (value) => (state.onlineCategory = value),
          ),
          chipRow(
            "年份",
            [
              ["", "全部年份"],
              ...Array.from({ length: 7 }, (_, index) => {
                const item = String(year - index);
                return [item, item];
              }),
            ],
            state.onlineYear,
            (value) => (state.onlineYear = value),
          ),
        ]),
      );
      result = await api(
        `/api/catalog/discover?${paramsFor(state.onlinePage)}`,
      );
      state.onlinePage = result.page || state.onlinePage;
      mergeCatalogGroups(result.results, groups);
    }
    if (result.errors?.length)
      root.append(
        make(
          "div",
          { class: "source-notice", role: "status" },
          result.errors.map((error) =>
            make("p", {
              text: `${configured.find((source) => source.id === error.source_id)?.name || "影视源"}：${error.message}`,
            }),
          ),
        ),
      );
    const grid = make("div", { class: "media-grid" });
    const cards = new Map();
    const renderGroup = (key, group) => {
      let card = cards.get(key);
      if (!card) {
        card = catalogCard(group, cards.size);
        cards.set(key, card);
        grid.append(card);
      } else {
        updateCatalogCard(card, group);
      }
    };
    let summary = null;
    if (!state.query) {
      summary = make("div", {
        class: "discover-summary",
        text: "已显示 0 组影片",
      });
      root.append(summary);
    }
    let rendered = 0;
    const batchSize = 20;
    const renderNextBatch = () => {
      const end = Math.min(rendered + batchSize, groupOrder.length);
      for (; rendered < end; rendered += 1) {
        const key = groupOrder[rendered];
        renderGroup(key, groups.get(key));
      }
      if (summary) {
        summary.textContent = `已显示 ${cards.size} 组影片`;
      }
    };
    syncGroupOrder();
    if (
      !state.query &&
      !state.onlineKind &&
      state.onlineSource === "all" &&
      !state.onlineCategory &&
      !state.onlineYear &&
      groupOrder.length
    )
      root.insertBefore(spotlightCard(groups.get(groupOrder[0])), summary);
    renderNextBatch();
    if (!cards.size)
      grid.append(
        empty(
          "⌕",
          state.query ? "没有找到相关影片" : "当前影视源没有返回内容",
          "可以更换影视源或搜索其他片名。",
        ),
      );
    root.append(grid);
    let hasMorePages = !state.query && result.page_count > state.onlinePage;
    let more = null;
    let moreText = null;
    let loading = false;
    const showEnd = () => {
      state.onlineObserver?.disconnect();
      state.onlineObserver = null;
      if (!more) return;
      more.className = "discover-end";
      more.replaceChildren(make("span", { text: "已经到底了" }));
    };
    if (rendered < groupOrder.length || hasMorePages) {
      more = make(
        "div",
        { class: "discover-more", role: "status" },
        [
          make("span", { class: "spinner", "aria-hidden": "true" }),
          make("span", { text: "加载更多…" }),
        ],
      );
      root.append(more);
      moreText = more.children[more.children.length - 1];
      const loadMore = async () => {
        if (loading || sequence !== renderSequence) return;
        if (rendered < groupOrder.length) {
          renderNextBatch();
          if (rendered >= groupOrder.length && !hasMorePages) showEnd();
          return;
        }
        if (!hasMorePages) return;
        loading = true;
        more.classList.add("loading");
        moreText.textContent = "正在加载…";
        try {
          const page = await api(
            `/api/catalog/discover?${paramsFor(state.onlinePage + 1)}`,
          );
          if (sequence !== renderSequence) return;
          state.onlinePage += 1;
          const changed = mergeCatalogGroups(page.results, groups);
          changed.forEach((key) => {
            if (cards.has(key)) renderGroup(key, groups.get(key));
          });
          syncGroupOrder();
          renderNextBatch();
          hasMorePages = state.onlinePage < page.page_count;
          if (rendered >= groupOrder.length && !hasMorePages) {
            showEnd();
          } else {
            more.classList.remove("loading");
            moreText.textContent = "加载更多…";
          }
        } catch (e) {
          if (sequence === renderSequence) {
            more.classList.remove("loading");
            moreText.textContent = "加载失败，点击重试";
            toast(e.message, true);
          }
        } finally {
          loading = false;
        }
      };
      more.addEventListener("click", loadMore);
      if (typeof IntersectionObserver === "function") {
        state.onlineObserver = new IntersectionObserver(
          (entries) => {
            if (entries.some((entry) => entry.isIntersecting)) loadMore();
          },
          { rootMargin: "0px" },
        );
        state.onlineObserver.observe(more);
      }
    } else {
      root.append(make("p", { class: "discover-end", text: "已经到底了" }));
    }
    return root;
  }
  async function openCatalogDetail(item) {
    let dialog = $("catalog-dialog");
    if (!dialog) {
      dialog = make("dialog", {
        id: "catalog-dialog",
        class: "info-dialog catalog-dialog",
        "aria-labelledby": "catalog-detail-title",
      });
      document.body.append(dialog);
    }
    const sequence = n(dialog.dataset.sequence) + 1;
    dialog.dataset.sequence = String(sequence);
    const sources = item.sources?.length
      ? item.sources
      : [{ source_id: item.source_id, id: item.id ?? item.media_id, source_name: item.source_name }];
    const sourceBar =
      sources.length > 1
        ? make("div", { class: "catalog-source-switch" }, [
            make("span", { class: "discover-filter-label", text: "影视源" }),
            make(
              "div",
              { class: "discover-filter-chips" },
              sources.map((source) => {
                const sourceId = source.id ?? source.media_id;
                const active =
                  String(source.source_id) === String(item.source_id) &&
                  String(sourceId) === String(item.id ?? item.media_id);
                const quality = sourceQuality(source);
                return make(
                  "button",
                  {
                    type: "button",
                    class: `filter-chip${active ? " active" : ""}`,
                    "aria-pressed": String(active),
                    "aria-label": [
                      source.source_name || `影视源 ${source.source_id}`,
                      quality,
                    ]
                      .filter(Boolean)
                      .join(" "),
                    on: {
                      click: () =>
                        openCatalogDetail({
                          ...item,
                          source_id: source.source_id,
                          id: sourceId,
                          source_name: source.source_name,
                          sources,
                        }),
                    },
                  },
                  [
                    make("span", {
                      class: "filter-chip-label",
                      text: source.source_name || `影视源 ${source.source_id}`,
                    }),
                    quality
                      ? make("span", {
                          class: "filter-chip-quality",
                          text: ` · ${quality}`,
                        })
                      : null,
                  ],
                );
              }),
            ),
          ])
        : null;
    dialog.replaceChildren(
      ...[
        (() => {
          const art = make("div", { class: "catalog-hero-art tone-2" });
          if (item.cover)
            art.append(
              make("img", {
                src: item.cover,
                alt: "",
                referrerpolicy: "no-referrer",
              }),
            );
          return make("section", { class: "catalog-hero" }, [
            art,
            make("div", { class: "catalog-hero-shade" }),
            make("div", { class: "catalog-hero-copy" }, [
              make("p", { class: "eyebrow", text: "影片详情" }),
              make("h2", { id: "catalog-detail-title", text: item.title }),
              make("p", {
                class: "catalog-hero-meta",
                text: [item.year, kindLabel(item.kind), item.source_name]
                  .filter(Boolean)
                  .join(" · "),
              }),
            ]),
            icon("✕", "关闭影片详情", () => dialog.close()),
          ]);
        })(),
        sourceBar,
        make("p", { class: "hint", text: "正在读取影片信息…" }),
      ].filter(Boolean),
    );
    if (!dialog.open) dialog.showModal();
    try {
      const [detail, subscriptions] = await Promise.all([
        api(
          `/api/catalog/${item.source_id}/detail?id=${encodeURIComponent(item.id || item.media_id)}`,
        ),
        api("/api/tracking"),
      ]);
      if (dialog.dataset.sequence !== String(sequence) || !dialog.open) return;
      let tracked = subscriptions.find(
        (subscription) =>
          subscription.source_id === detail.source_id &&
          subscription.media_id === detail.id,
      );
      const content = make("div", { class: "catalog-detail-content" });
      content.append(
        make("p", {
          class: "media-meta",
          text: [detail.year, detail.remark, detail.source_name]
            .filter(Boolean)
            .join(" · "),
        }),
      );
      const description = new DOMParser().parseFromString(
        detail.description || "",
        "text/html",
      ).body.textContent;
      if (description)
        content.append(
          make("p", { class: "catalog-description", text: description }),
        );
      const auto = make("input", { type: "checkbox" });
      const trackButton = button(
        tracked ? "已加入追剧" : "加入追剧",
        "primary",
        async () => {
          if (tracked) {
            dialog.close();
            setNav("series");
            return;
          }
          if (
            auto.checked &&
            !window.confirm(
              "自动下载会将当前所有未看集数加入队列，并持续下载以后更新的剧集。继续吗？",
            )
          )
            return;
          trackButton.disabled = true;
          try {
            tracked = await api("/api/tracking", {
              method: "POST",
              body: {
                source_id: detail.source_id,
                media_id: detail.id,
                auto_download: auto.checked,
              },
            });
            trackButton.textContent = "已加入追剧";
            auto.disabled = true;
            toast("已加入追剧清单");
          } catch (error) {
            toast(error.message, true);
          } finally {
            trackButton.disabled = false;
          }
        },
      );
      content.append(
        make("div", { class: "catalog-actions" }, [
          trackButton,
          !tracked
            ? make("label", { class: "checkbox-label" }, [auto, "自动下载更新"])
            : make("span", {
                class: "muted",
                text: tracked.auto_download ? "自动下载已开启" : "已订阅更新",
              }),
        ]),
      );
      if (!detail.groups.length) {
        content.append(
          empty("▷", "暂无可用播放线路", "该来源没有返回 HTTP(S) 播放地址。"),
        );
        dialog.append(content);
        return;
      }
      const groups = make(
        "select",
        { "aria-label": "播放线路" },
        detail.groups.map((group, index) =>
          make("option", {
            value: index,
            text: group.direct ? group.name : `${group.name}（非直链）`,
          }),
        ),
      );
      const preferred = detail.groups.findIndex((group) => group.direct);
      if (preferred > 0) groups.value = String(preferred);
      const episodes = make("select", { "aria-label": "选择集数" });
      const episodeGrid = make("div", { class: "episode-grid" });
      const selected = () =>
        detail.groups[n(groups.value)].episodes[n(episodes.value)];
      let episodeAscending = true;
      const orderedEpisodes = () =>
        detail.groups[n(groups.value)].episodes
          .map((episode, index) => ({ ...episode, sourceIndex: index }))
          .sort((one, other) =>
            episodeAscending
              ? one.sourceIndex - other.sourceIndex
              : other.sourceIndex - one.sourceIndex,
          );
      const watchedCount = tracked?.watched_episodes || 0;
      const watch = (index) => {
        const group = detail.groups[n(groups.value)];
        if (group.direct === false) {
          toast("该线路是非直链解析页，无法直接播放，请选择直链线路。", true);
          return;
        }
        const playlist = orderedEpisodes().map((episode) => ({
          title: `${detail.title} · ${episode.name}`,
          remote_url: episode.url,
          tracking_id: tracked?.id,
          watched_episodes: watchedCount,
          episode_number: episode.sourceIndex + 1,
        }));
        dialog.close();
        play(playlist[index], false, playlist, index);
      };
      const sortButton = button("倒序", "subtle small-button", () => {
        episodeAscending = !episodeAscending;
        sortButton.textContent = episodeAscending ? "倒序" : "正序";
        fillEpisodes();
      });
      const fillEpisodes = () => {
        episodes.replaceChildren(
          ...orderedEpisodes().map((episode, index) =>
            make("option", { value: index, text: episode.name }),
          ),
        );
        episodeGrid.replaceChildren(
          sortButton,
          ...orderedEpisodes().map((episode, index) =>
            button(
              episode.sourceIndex < watchedCount
                ? `✓ ${episode.name}`
                : episode.name,
              `subtle small-button${episode.sourceIndex < watchedCount ? " watched" : ""}`,
              () => watch(index),
            ),
          ),
        );
      };
      groups.addEventListener("change", fillEpisodes);
      fillEpisodes();
      const download = button("下载本集", "subtle", async () => {
        if (detail.groups[n(groups.value)].direct === false) {
          toast("该线路是非直链解析页，无法直接下载，请选择直链线路。", true);
          return;
        }
        download.disabled = true;
        try {
          await api("/api/downloads", {
            method: "POST",
            body: {
              url: selected().url,
              title: `${detail.title} S01E${String(n(episodes.value) + 1).padStart(2, "0")}`,
            },
          });
          toast("已加入下载队列");
        } catch (error) {
          toast(error.message, true);
        } finally {
          download.disabled = false;
        }
      });
      content.append(
        make("div", { class: "catalog-play-controls" }, [
          make("label", {}, ["线路", groups]),
          make("label", {}, ["集数", episodes]),
          button("在线播放", "primary", () => watch(n(episodes.value))),
          download,
        ]),
        episodeGrid,
        make("p", {
          class: "hint",
          text: "播放与下载使用来源提供的地址；标为“非直链”的线路需要解析器，可能无法播放。下载完成后自动进入媒体库。",
        }),
      );
      dialog.append(content);
    } catch (error) {
      if (dialog.dataset.sequence === String(sequence))
        dialog.append(
          make("p", {
            class: "form-error",
            role: "alert",
            text: error.message,
          }),
        );
    }
  }
  async function checkTracking() {
    try {
      const result = await api("/api/tracking/check", {
        method: "POST",
        body: {},
      });
      toast(
        `检查完成：${result.checked} 部，新增下载 ${result.queued} 集${result.errors?.length ? `，${result.errors.length} 项来源异常` : ""}`,
      );
      render();
    } catch (error) {
      toast(error.message, true);
    }
  }
  function trackingRows(subscriptions) {
    const list = make("div", { class: "series-list" });
    if (!subscriptions.length)
      list.append(
        empty(
          "▤",
          "还没有在线追剧",
          "在影视发现中打开影片详情，加入追剧清单。",
          button("发现影片", "primary", () => setNav("online")),
        ),
      );
    subscriptions
      .filter(
        (subscription) =>
          !state.query ||
          subscription.title.toLowerCase().includes(state.query.toLowerCase()),
      )
      .forEach((subscription) => {
        const watched = make("input", {
          type: "number",
          name: "watched",
          value: subscription.watched_episodes,
          min: 0,
          max: subscription.total_episodes,
          "aria-label": `${subscription.title} 已看集数`,
          required: true,
        });
        const auto = make("input", {
          type: "checkbox",
          checked: subscription.auto_download,
          on: {
            change: async (event) => {
              if (
                event.target.checked &&
                !window.confirm(
                  "将自动下载此剧所有未看集数，以及后续更新。继续吗？",
                )
              ) {
                event.target.checked = false;
                return;
              }
              try {
                await api(`/api/tracking/${subscription.id}`, {
                  method: "PUT",
                  body: { auto_download: event.target.checked },
                });
                toast("自动下载设置已保存");
              } catch (error) {
                event.target.checked = !event.target.checked;
                toast(error.message, true);
              }
            },
          },
        });
        const form = make(
          "form",
          {
            class: "tracking-progress",
            on: {
              submit: async (event) => {
                event.preventDefault();
                try {
                  await api(`/api/tracking/${subscription.id}`, {
                    method: "PUT",
                    body: { watched_episodes: n(watched.value) },
                  });
                  toast("已看集数已保存");
                  render();
                } catch (error) {
                  toast(error.message, true);
                }
              },
            },
          },
          [
            make("label", {}, ["已看", watched]),
            button("保存", "subtle small-button", null, { type: "submit" }),
          ],
        );
        const watchedCount = n(subscription.watched_episodes);
        const totalCount = n(subscription.total_episodes);
        const pct = totalCount
          ? Math.min(100, Math.round((watchedCount / totalCount) * 100))
          : 0;
        const statusLabel = subscription.error
          ? "来源异常"
          : totalCount && watchedCount >= totalCount
            ? "已追平"
            : "连载中";
        list.append(
          make("article", {
            class: `tracking-row${statusLabel === "已追平" ? " finished" : ""}`,
          }, [
            make("div", { class: "series-info" }, [
              make("h3", { text: subscription.title }),
              make("span", {
                class: `tracking-status${statusLabel === "来源异常" ? " error" : ""}`,
                text: statusLabel,
              }),
              make("progress", {
                class: "tracking-progress-bar",
                max: 100,
                value: pct,
              }),
              make("p", {
                text: `${subscription.watched_episodes} / ${subscription.total_episodes} 集已看 · 已入队 ${subscription.queued_episodes} 集 · 已下载 ${subscription.completed_downloads || 0} 集`,
              }),
              make("p", {
                class: subscription.error ? "form-error" : "hint",
                text:
                  subscription.error ||
                  (subscription.last_checked
                    ? `上次检查：${new Date(subscription.last_checked * 1000).toLocaleString()}`
                    : "等待检查更新"),
              }),
            ]),
            make("div", { class: "catalog-actions" }, [
              button("查看剧集", "subtle small-button", () =>
                openCatalogDetail({
                  source_id: subscription.source_id,
                  media_id: subscription.media_id,
                  title: subscription.title,
                }),
              ),
              make("label", { class: "checkbox-label" }, [auto, "自动下载"]),
              button("取消追剧", "danger small-button", async () => {
                if (
                  !window.confirm(
                    `取消追剧“${subscription.title}”？已下载文件会保留。`,
                  )
                )
                  return;
                try {
                  await api(`/api/tracking/${subscription.id}`, {
                    method: "DELETE",
                  });
                  render();
                } catch (error) {
                  toast(error.message, true);
                }
              }),
            ]),
            form,
          ]),
        );
      });
    return list;
  }
  async function series() {
    const [result, subscriptions] = await Promise.all([
      api("/api/series"),
      api("/api/tracking"),
    ]);
    state.series = Array.isArray(result) ? result : result?.series || [];
    const root = make(
      "div",
      {},
      head(
        "追剧清单",
        "订阅你正在等待的故事，随时回来继续。",
        button("检查更新", "primary", checkTracking),
      ),
    );
    root.append(
      sectionTitle("在线追剧", subscriptions.length),
      trackingRows(subscriptions),
      sectionTitle("本地剧集", state.series.length),
    );
    const list = make("div", { class: "series-list" });
    if (!state.series.length)
      list.append(
        empty("▤", "还没有剧集", "扫描媒体库后，剧集会自动出现在这里。"),
      );
    state.series
      .filter(
        (s) =>
          !state.query ||
          s.title.toLowerCase().includes(state.query.toLowerCase()),
      )
      .forEach((s) => {
        const total = n(s.total_episodes),
          watched = n(s.watched_episodes);
        list.append(
          make("article", { class: "series-row" }, [
            make("span", { class: "series-cover", text: "▤" }),
            make("div", { class: "series-info" }, [
              make("h3", { text: s.title }),
              make("p", { text: `${watched} / ${total || "?"} 集已观看` }),
            ]),
            button("查看剧集", "subtle small-button", () => {
              state.query = s.title;
              state.filter = "episode";
              $("search-input").value = s.title;
              setNav("library");
            }),
            total ? make("progress", { max: total, value: watched }) : null,
            make(
              "button",
              {
                type: "button",
                class: `button small-button ${s.subscribed ? "primary" : "subtle"}`,
                "aria-pressed": String(Boolean(s.subscribed)),
                on: {
                  click: async () => {
                    try {
                      const result = await api(
                        `/api/series/${encodeURIComponent(s.id)}/subscription`,
                        { method: "PUT", body: { subscribed: !s.subscribed } },
                      );
                      s.subscribed = result?.subscribed ?? !s.subscribed;
                      render();
                    } catch (e) {
                      toast(e.message, true);
                    }
                  },
                },
              },
              s.subscribed ? "已订阅" : "订阅",
            ),
          ]),
        );
      });
    root.append(list);
    return root;
  }
  function status(job) {
    if (job.status === "completed" || job.status === "done")
      return ["已完成", "done"];
    if (job.status === "failed" || job.status === "error")
      return ["失败", "error"];
    if (/cancel/i.test(job.status)) return ["已取消", ""];
    return [
      job.status === "running" || job.status === "downloading"
        ? "下载中"
        : "排队中",
      "",
    ];
  }
  function downloadList() {
    const list = make("div", { class: "download-list" });
    if (!state.downloads.length)
      list.append(
        empty(
          "↓",
          "下载队列是空的",
          "添加一个你有权保存的视频直链或 HLS 地址。",
        ),
      );
    state.downloads.forEach((job) => {
      const [label, cls] = status(job);
      const total = n(job.total_bytes),
        written = n(job.bytes_written);
      const row = make("article", { class: "download-row" }, [
        make("div", { class: "download-title" }, [
          make("strong", { text: job.title }),
          make("small", { text: job.error || job.relative_path || job.url }),
        ]),
        total
          ? make("progress", {
              class: "download-progress",
              max: total,
              value: written,
            })
          : make("span", {
              class: "small muted",
              text: written
                ? `${Math.round(written / 1048576)} MB`
                : "等待开始",
            }),
        make("span", { class: `download-status ${cls}`, text: label }),
      ]);
      if (/failed|cancel/i.test(job.status))
        row.append(
          button("重试", "subtle small-button", async () => {
            try {
              await api(`/api/downloads/${encodeURIComponent(job.id)}/retry`, {
                method: "POST",
                body: {},
              });
              await refreshDownloads();
            } catch (e) {
              toast(e.message, true);
            }
          }),
        );
      else if (!/completed|done/i.test(job.status))
        row.append(
          icon("×", `取消 ${job.title}`, async () => {
            try {
              await api(`/api/downloads/${encodeURIComponent(job.id)}`, {
                method: "DELETE",
              });
              await refreshDownloads();
            } catch (e) {
              toast(e.message, true);
            }
          }),
        );
      list.append(row);
    });
    return list;
  }
  async function refreshDownloads() {
    const result = await api("/api/downloads");
    state.downloads = Array.isArray(result) ? result : result?.jobs || [];
    const existing = main.querySelector(".download-list");
    if (state.page === "downloads" && existing) {
      existing.replaceWith(downloadList());
      updateNavBadges();
    }
  }
  async function downloads() {
    await refreshDownloads();
    const root = make(
      "div",
      {},
      head("下载任务", "把想看的内容带回你的媒体库。"),
    );
    const add = make("section", { class: "panel" }, [
      make("h2", { text: "添加下载" }),
      make("p", {
        class: "panel-description",
        text: "支持 HTTP / HTTPS 视频文件直链和 HLS（.m3u8）。HLS 由服务器上的 FFmpeg 下载合并；请只保存你拥有或获授权的内容。",
      }),
    ]);
    add.append(
      make(
        "form",
        {
          class: "form-row",
          on: {
            submit: async (event) => {
              event.preventDefault();
              const f = event.currentTarget;
              try {
                await api("/api/downloads", {
                  method: "POST",
                  body: {
                    title: f.elements.title.value.trim(),
                    url: f.elements.url.value.trim(),
                  },
                });
                f.reset();
                toast("已加入下载队列");
                await refreshDownloads();
              } catch (e) {
                toast(e.message, true);
              }
            },
          },
        },
        [
          make("label", {}, [
            "名称",
            make("input", { name: "title", required: true, maxlength: 200 }),
          ]),
          make("label", {}, [
            "视频 / HLS 地址",
            make("input", {
              name: "url",
              type: "url",
              required: true,
              maxlength: 2000,
              placeholder: "https://example.com/video.mp4",
            }),
          ]),
          button("开始下载", "primary", null, { type: "submit" }),
        ],
      ),
    );
    root.append(
      add,
      sectionTitle("全部任务", state.downloads.length),
      downloadList(),
    );
    return root;
  }
  async function epgSettings() {
    const panel = make("section", { class: "panel source-settings" }, [
      make("h2", { text: "电视节目单" }),
      make("p", {
        class: "panel-description",
        text: "每行一个 XMLTV 地址（最多 4 个），后面的地址只补充前面缺失的节目；电视直播页按频道名称自动匹配。",
      }),
    ]);
    let config = {
      url: "http://epg.51zmt.top:8000/e.xml\nhttps://epg.112114.xyz/pp.xml\nhttps://live.fanmingming.com/e.xml\nhttps://raw.githubusercontent.com/litiande03/epg/master/pl.xml.gz",
      programs: 0,
      updated_at: 0,
    };
    try {
      config = await api("/api/epg/config");
    } catch (error) {
      panel.append(make("p", { class: "form-error", text: error.message }));
      return panel;
    }
    panel.append(
      make("p", {
        class: "panel-description",
        text: config.updated_at
          ? `已收录 ${config.programs} 条节目 · 更新于 ${clockTime(config.updated_at)}`
          : "尚未拉取节目单。",
      }),
      make(
        "form",
        {
          class: "form-row",
          on: {
            submit: async (event) => {
              event.preventDefault();
              try {
                await api("/api/epg/config", {
                  method: "PUT",
                  body: { url: event.currentTarget.elements.url.value.trim() },
                });
                toast("节目单地址已保存，点击“立即刷新”拉取节目。");
              } catch (error) {
                toast(error.message, true);
              }
            },
          },
        },
        [
          make("label", {}, [
            "XMLTV 地址（每行一个）",
            make("textarea", {
              name: "url",
              rows: "4",
              required: true,
              maxlength: 4096,
              placeholder: "http://epg.51zmt.top:8000/e.xml",
              text: config.url,
            }),
          ]),
          button("保存地址", "primary", null, { type: "submit" }),
          button("立即刷新", "subtle", async () => {
            try {
              const result = await api("/api/epg/refresh", { method: "POST" });
              toast(`节目单已更新：${result.matched_channels} 个频道`);
              await render();
            } catch (error) {
              toast(error.message, true);
            }
          }),
        ],
      ),
    );
    return panel;
  }
  async function sourceSettings() {
    const panel = make("section", { class: "panel source-settings" }, [
      make("h2", { text: "在线影视源" }),
      make("p", {
        class: "panel-description",
        text: "配置 MacCMS JSON 接口，供网页搜索、追剧和影视仓使用。配置变更对所有成员生效。",
      }),
    ]);
    try {
      const [sources, catalogSettings] = await Promise.all([
        api("/api/sources"),
        api("/api/catalog/settings"),
      ]);
      const adultEnabled = make("input", {
        type: "checkbox",
        checked: catalogSettings.adult_sources_enabled,
      });
      panel.append(
        make(
          "form",
          {
            class: "form-row",
            on: {
              submit: async (event) => {
                event.preventDefault();
                try {
                  await api("/api/catalog/settings", {
                    method: "PUT",
                    body: { adult_sources_enabled: adultEnabled.checked },
                  });
                  toast(
                    adultEnabled.checked ? "成人源已开启" : "成人源已关闭",
                  );
                  render();
                } catch (error) {
                  toast(error.message, true);
                }
              },
            },
          },
          [
            make("label", { class: "checkbox-label" }, [
              adultEnabled,
              "允许成人源",
            ]),
            button("保存成人源设置", "subtle", null, { type: "submit" }),
          ],
        ),
        make("p", {
          class: "hint",
          text: "关闭后成人源不会出现在浏览、搜索和电视端。只有管理员能修改。",
        }),
      );
      const newForm = make(
        "form",
        {
          class: "form-row",
          on: {
            submit: async (event) => {
              event.preventDefault();
              const form = event.currentTarget,
                submit = form.querySelector('[type="submit"]');
              submit.disabled = true;
              try {
                await api("/api/sources", {
                  method: "POST",
                  body: {
                    name: form.elements.name.value.trim(),
                    url: form.elements.url.value.trim(),
                    adult: form.elements.adult.checked,
                  },
                });
                toast("影视源已添加");
                render();
              } catch (error) {
                toast(error.message, true);
              } finally {
                submit.disabled = false;
              }
            },
          },
        },
        [
          make("label", {}, [
            "名称",
            make("input", { name: "name", required: true, maxlength: 100 }),
          ]),
          make("label", {}, [
            "JSON 接口地址",
            make("input", {
              name: "url",
              type: "url",
              required: true,
              maxlength: 4096,
              placeholder: "https://example.com/api.php/provide/vod/",
            }),
          ]),
          make("label", { class: "checkbox-label" }, [
            make("input", { type: "checkbox", name: "adult" }),
            "成人源",
          ]),
          button("添加影视源", "primary", null, { type: "submit" }),
        ],
      );
      panel.append(newForm);
      sources.forEach((source) => {
        const name = make("input", {
          name: "name",
          value: source.name,
          required: true,
          maxlength: 100,
          "aria-label": "影视源名称",
        });
        const url = make("input", {
          name: "url",
          type: "url",
          value: source.url,
          required: true,
          maxlength: 4096,
          "aria-label": `${source.name} 接口地址`,
        });
        const enabled = make("input", {
          type: "checkbox",
          checked: source.enabled,
        });
        const adult = make("input", {
          type: "checkbox",
          checked: source.adult,
        });
        const form = make(
          "form",
          {
            class: "source-row",
            on: {
              submit: async (event) => {
                event.preventDefault();
                try {
                  await api(`/api/sources/${source.id}`, {
                    method: "PUT",
                    body: {
                      name: name.value.trim(),
                      url: url.value.trim(),
                      enabled: enabled.checked,
                      adult: adult.checked,
                    },
                  });
                  toast("影视源已保存");
                } catch (error) {
                  toast(error.message, true);
                }
              },
            },
          },
          [
            name,
            url,
            make("label", { class: "checkbox-label" }, [enabled, "启用"]),
            make("label", { class: "checkbox-label" }, [adult, "成人源"]),
            button("保存", "subtle small-button", null, { type: "submit" }),
            button("移除", "danger small-button", async () => {
              if (
                !window.confirm(
                  `移除影视源“${source.name}”？正在使用此来源的追剧订阅需要先取消。`,
                )
              )
                return;
              try {
                await api(`/api/sources/${source.id}`, { method: "DELETE" });
                toast("影视源已移除");
                render();
              } catch (error) {
                toast(error.message, true);
              }
            }),
          ],
        );
        panel.append(form);
      });
      if (!sources.length)
        panel.append(
          make("p", {
            class: "hint",
            text: "尚未配置影视源。请填写自己有权使用且当前可访问的接口。",
          }),
        );
    } catch (error) {
      panel.append(make("p", { class: "form-error", text: error.message }));
    }
    return panel;
  }
  async function tvboxSettings() {
    const panel = make("section", { class: "panel" }, [
      make("h2", { text: "影视仓 / TVBox 订阅" }),
      make("p", {
        class: "panel-description",
        text: "把配置地址填入影视仓的配置接口。地址包含本账户的访问令牌，请只用于自己的设备。",
      }),
    ]);
    try {
      const feed = await api("/api/tvbox");
      [
        ["影视仓配置", feed.config_path],
        ["媒体库接口", feed.catalog_path],
        ["直播 M3U", feed.live_path],
      ].forEach(([label, path]) => {
        const url = new URL(path, location.origin).href;
        const input = make("input", {
          value: url,
          readonly: true,
          "aria-label": label,
          spellcheck: "false",
        });
        panel.append(
          make("div", { class: "feed-row" }, [
            make("label", {}, [label, input]),
            button("复制", "subtle small-button", async () => {
              try {
                await navigator.clipboard.writeText(url);
                toast(`${label}已复制`);
              } catch (_) {
                input.focus();
                input.select();
                toast("地址已选中，请按 Ctrl+C（Mac 为 ⌘C）复制。");
              }
            }),
          ]),
        );
      });
      panel.append(
        make("p", {
          class: "hint",
          text: "电视需要能访问当前网页地址。原有订阅在重置令牌后会立即失效，需要在影视仓重新填写新地址。",
        }),
        button("重置电视访问令牌", "danger small-button", async () => {
          if (
            !window.confirm(
              "这会立即撤销旧电视订阅地址。继续后，需要在所有客户端填写新地址。确定重置？",
            )
          )
            return;
          try {
            await api("/api/tvbox/token", { method: "POST", body: {} });
            toast("电视访问令牌已重置");
            render();
          } catch (error) {
            toast(error.message, true);
          }
        }),
      );
    } catch (error) {
      panel.append(make("p", { class: "form-error", text: error.message }));
    }
    return panel;
  }
  async function settings() {
    const root = make(
      "div",
      {},
      head("账户与设置", "管理你的账户，以及这个影音空间的成员。"),
    );
    const grid = make("div", { class: "settings-grid" });
    const panel = make("section", { class: "panel" }, [
      make("h2", { text: "修改密码" }),
      make("p", { class: "panel-description", text: "密码至少 10 个字符。" }),
    ]);
    panel.append(
      make(
        "form",
        {
          class: "stack",
          on: {
            submit: async (event) => {
              event.preventDefault();
              const f = event.currentTarget;
              if (f.elements.password.value.length < 10)
                return toast("新密码需要至少 10 个字符", true);
              try {
                await api("/api/auth/password", {
                  method: "POST",
                  body: {
                    current_password: f.elements.current.value,
                    password: f.elements.password.value,
                  },
                });
                f.reset();
                toast("密码已更新");
              } catch (e) {
                toast(e.message, true);
              }
            },
          },
        },
        [
          make("label", {}, [
            "当前密码",
            make("input", {
              name: "current",
              type: "password",
              required: true,
            }),
          ]),
          make("label", {}, [
            "新密码",
            make("input", {
              name: "password",
              type: "password",
              minlength: 10,
              maxlength: 128,
              required: true,
            }),
          ]),
          button("保存新密码", "primary", null, { type: "submit" }),
        ],
      ),
    );
    grid.append(panel);
    if (admin()) {
      const users = make("section", { class: "panel" }, [
        make("h2", { text: "家庭成员" }),
        make("p", {
          class: "panel-description",
          text: "管理员可以邀请成员进入这个影音空间。",
        }),
      ]);
      users.append(
        make(
          "form",
          {
            class: "form-row",
            on: {
              submit: async (event) => {
                event.preventDefault();
                const f = event.currentTarget;
                try {
                  await api("/api/users", {
                    method: "POST",
                    body: {
                      username: f.elements.username.value.trim(),
                      password: f.elements.password.value,
                      role: f.elements.role.value,
                    },
                  });
                  f.reset();
                  toast("成员已添加");
                  await render();
                } catch (e) {
                  toast(e.message, true);
                }
              },
            },
          },
          [
            make("label", {}, [
              "用户名",
              make("input", {
                name: "username",
                required: true,
                pattern: "[A-Za-z0-9_-]{3,32}",
              }),
            ]),
            make("label", {}, [
              "初始密码",
              make("input", {
                name: "password",
                type: "password",
                minlength: 10,
                required: true,
              }),
            ]),
            make("label", {}, [
              "角色",
              make("select", { name: "role" }, [
                make("option", { value: "user", text: "家庭成员" }),
                make("option", { value: "admin", text: "管理员" }),
              ]),
            ]),
            button("添加成员", "primary", null, { type: "submit" }),
          ],
        ),
      );
      try {
        const result = await api("/api/users");
        const members = Array.isArray(result) ? result : result?.users || [];
        const list = make("div", { class: "user-list" });
        members.forEach((member) =>
          list.append(
            make("div", { class: "user-row" }, [
              make("span", {
                class: "avatar",
                text: member.username.slice(0, 1).toUpperCase(),
              }),
              make("div", {}, [
                make("strong", { text: member.username }),
                make("small", {
                  text: member.role === "admin" ? "管理员" : "家庭成员",
                }),
              ]),
              member.id === state.user.id
                ? make("span", { class: "small muted", text: "当前账户" })
                : button("移除", "danger small-button", async () => {
                    if (!window.confirm(`确定移除 ${member.username}？`))
                      return;
                    try {
                      await api(`/api/users/${encodeURIComponent(member.id)}`, {
                        method: "DELETE",
                      });
                      toast("成员已移除");
                      render();
                    } catch (e) {
                      toast(e.message, true);
                    }
                  }),
            ]),
          ),
        );
        users.append(list);
      } catch (e) {
        users.append(make("p", { class: "form-error", text: e.message }));
      }
      grid.append(users);
    }
    root.append(grid, await tvboxSettings());
    if (admin()) root.append(await epgSettings());
    if (admin()) root.append(await sourceSettings());
    return root;
  }
  async function render() {
    const sequence = ++renderSequence,
      pageName = state.page;
    clearInterval(state.poll);
    state.onlineObserver?.disconnect();
    state.onlineObserver = null;
    main.setAttribute("aria-busy", "true");
    const loadingText =
      pageName === "online" && state.onlineKind
        ? `正在从各影视源加载${kindLabel(state.onlineKind)}…`
        : "正在加载…";
    main.replaceChildren(
      ["online", "library"].includes(pageName)
        ? make("div", { class: "page-loading" }, [
            make("p", { class: "loading-caption", text: loadingText }),
            make(
              "div",
              { class: "skeleton-grid" },
              Array.from({ length: 8 }, () =>
                make("div", { class: "skeleton-card" }, [
                  make("div", { class: "skeleton skeleton-poster" }),
                  make("div", { class: "skeleton skeleton-line" }),
                ]),
              ),
            ),
          ])
        : make("div", { class: "loading-state" }, [
            make("span", { class: "spinner" }),
            make("p", { text: loadingText }),
          ]),
    );
    try {
      const page =
        pageName === "online"
          ? await online()
          : pageName === "library"
            ? await library()
            : pageName === "live"
              ? await live()
              : pageName === "series"
                ? await series()
                : pageName === "downloads"
                  ? await downloads()
                  : await settings();
      if (sequence !== renderSequence) return;
      main.replaceChildren(page);
      updateNavBadges();
      if (pageName === "downloads")
        state.poll = setInterval(
          () => refreshDownloads().catch((e) => toast(e.message, true)),
          3500,
        );
    } catch (e) {
      if (sequence === renderSequence)
        main.replaceChildren(
          empty(
            "◌",
            "这次没有打开成功",
            e.message,
            button("再试一次", "primary", render),
          ),
        );
    } finally {
      if (sequence === renderSequence) main.setAttribute("aria-busy", "false");
    }
  }
  function authMode(setup) {
    const username = $("auth-username"),
      password = $("auth-password");
    username.setAttribute("minlength", setup ? "3" : "1");
    username.setAttribute("maxlength", setup ? "32" : "128");
    username.setAttribute(
      "placeholder",
      setup ? "3–32 位字母、数字、_ 或 -" : "输入已有用户名",
    );
    if (setup) username.setAttribute("pattern", "[A-Za-z0-9_-]{3,32}");
    else username.removeAttribute("pattern");
    password.setAttribute("minlength", setup ? "10" : "1");
    password.setAttribute(
      "autocomplete",
      setup ? "new-password" : "current-password",
    );
    $("auth-title").textContent = setup ? "先为自己留一把钥匙。" : "欢迎回家。";
    $("auth-description").textContent = setup
      ? "创建管理员账户，开始你的私人影音空间。"
      : "登录，继续你的下一段精彩。";
    $("auth-submit").firstChild.textContent = setup
      ? "创建空间 "
      : "进入影音空间 ";
    $("auth-form").dataset.setup = setup ? "1" : "0";
    $("auth-error").hidden = true;
  }
  function showApp(user) {
    state.user = user;
    state.query = "";
    state.filter = "all";
    $("search-input").value = "";
    $("auth-password").value = "";
    $("auth-screen").hidden = true;
    $("app-shell").hidden = false;
    $("profile-name").textContent = user.username;
    $("profile-role").textContent = admin() ? "管理员" : "家庭成员";
    $("profile-avatar").textContent = user.username.slice(0, 1).toUpperCase();
    setNav("online");
  }
  async function logout() {
    try {
      await api("/api/auth/logout", { method: "POST", body: {} });
    } catch (e) {
      toast(`退出失败：${e.message}`, true);
      return;
    }
    state.user = null;
    ++renderSequence;
    clearInterval(state.poll);
    $("app-shell").hidden = true;
    $("auth-screen").hidden = false;
    authMode(false);
  }
  function directionalFocus(event) {
    if (
      !["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(
        event.key,
      ) ||
      ["INPUT", "TEXTAREA", "SELECT", "VIDEO"].includes(
        document.activeElement?.tagName,
      )
    )
      return;
    const from = document.activeElement?.getBoundingClientRect();
    if (!from || !from.width) return;
    const scope = document.querySelector("dialog[open]") || $("app-shell");
    const vertical = event.key === "ArrowUp" || event.key === "ArrowDown",
      positive = event.key === "ArrowRight" || event.key === "ArrowDown";
    let chosen = null,
      best = Infinity;
    scope
      .querySelectorAll(
        "button:not(:disabled), a[href], input, select, textarea",
      )
      .forEach((candidate) => {
        if (candidate === document.activeElement) return;
        const rect = candidate.getBoundingClientRect();
        if (!rect.width || !rect.height) return;
        const dx = rect.left + rect.width / 2 - (from.left + from.width / 2),
          dy = rect.top + rect.height / 2 - (from.top + from.height / 2);
        const forward = vertical ? dy : dx,
          side = vertical ? dx : dy;
        if (positive ? forward <= 1 : forward >= -1) return;
        const score = Math.abs(forward) + Math.abs(side) * 3;
        if (score < best) {
          best = score;
          chosen = candidate;
        }
      });
    if (chosen) {
      event.preventDefault();
      chosen.focus();
    }
  }
  document.addEventListener("keydown", directionalFocus);
  async function boot() {
    try {
      const status = await api("/api/auth/status");
      if (status?.user) showApp(status.user);
      else {
        $("auth-screen").hidden = false;
        authMode(Boolean(status?.needs_setup));
      }
    } catch (e) {
      $("auth-screen").hidden = false;
      authMode(false);
      $("auth-error").textContent = e.message;
      $("auth-error").hidden = false;
    }
  }
  $("auth-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const f = event.currentTarget,
      setup = f.dataset.setup === "1",
      username = setup
        ? f.elements.username.value.trim()
        : f.elements.username.value,
      password = f.elements.password.value;
    if (
      setup
        ? !/^[A-Za-z0-9_-]{3,32}$/.test(username)
        : !username.length ||
          Array.from(username).length > 128 ||
          /\p{Cc}/u.test(username)
    )
      return (
        ($("auth-error").textContent = setup
          ? "用户名需要 3–32 位字母、数字、下划线或短横线。"
          : "请输入不超过 128 个字符的已有用户名。"),
        ($("auth-error").hidden = false)
      );
    if (
      Array.from(password).length < (setup ? 10 : 1) ||
      Array.from(password).length > 128
    )
      return (
        ($("auth-error").textContent = setup
          ? "密码需要 10–128 个字符。"
          : "请输入不超过 128 个字符的已有密码。"),
        ($("auth-error").hidden = false)
      );
    try {
      const result = await api(setup ? "/api/auth/setup" : "/api/auth/login", {
        method: "POST",
        body: { username, password },
      });
      showApp(result?.user || result);
    } catch (e) {
      $("auth-error").textContent = e.message;
      $("auth-error").hidden = false;
    }
  });
  document
    .querySelectorAll("[data-view]")
    .forEach((el) =>
      el.addEventListener("click", () => setNav(el.dataset.view)),
    );
  function applyTheme(theme) {
    const dark = theme === "dark";
    document.documentElement.dataset.theme = dark ? "dark" : "light";
    document
      .querySelector('meta[name="theme-color"]')
      ?.setAttribute("content", dark ? "#080c14" : "#f4f5f9");
    const toggle = $("theme-toggle");
    toggle.setAttribute("aria-label", dark ? "切换浅色模式" : "切换深色模式");
    toggle.title = toggle.getAttribute("aria-label");
    try {
      localStorage.setItem("polyview-theme", dark ? "dark" : "light");
    } catch (_) {}
  }
  let savedTheme = "light";
  try {
    savedTheme = localStorage.getItem("polyview-theme") || "light";
  } catch (_) {}
  applyTheme(savedTheme);
  $("theme-toggle").addEventListener("click", () =>
    applyTheme(
      document.documentElement.dataset.theme === "dark" ? "light" : "dark",
    ),
  );
  const setNavigation = (open) => {
    document.body.classList.toggle("nav-open", open);
    $("nav-toggle").setAttribute("aria-expanded", String(open));
  };
  $("nav-toggle").addEventListener("click", () =>
    setNavigation(!document.body.classList.contains("nav-open")),
  );
  document
    .querySelectorAll(".sidebar [data-view]")
    .forEach((el) => el.addEventListener("click", () => setNavigation(false)));
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && document.body.classList.contains("nav-open"))
      setNavigation(false);
  });
  document.addEventListener("click", (event) => {
    if (
      event.target === document.body &&
      document.body.classList.contains("nav-open")
    )
      setNavigation(false);
  });
  $("logout-button").addEventListener("click", logout);
  $("mobile-logout").addEventListener("click", logout);
  $("help-button").addEventListener("click", () =>
    $("help-dialog").showModal(),
  );
  $("help-close").addEventListener("click", () => $("help-dialog").close());
  $("player-close").addEventListener("click", closePlayer);
  $("player-prev").addEventListener("click", () => playPlaylist(-1));
  $("player-next").addEventListener("click", () => playPlaylist(1));
  $("player-hud-close").addEventListener("click", () => {
    if (document.fullscreenElement || document.webkitFullscreenElement)
      (document.exitFullscreen || document.webkitExitFullscreen)?.call(
        document,
      );
    closePlayer();
  });
  $("player-source").addEventListener("click", () => switchLiveSource(1));
  let fullscreenExitedAt = 0;
  $("player-dialog").addEventListener("cancel", (event) => {
    event.preventDefault();
    if (document.fullscreenElement || document.webkitFullscreenElement) return;
    if (Date.now() - fullscreenExitedAt < 300) return;
    closePlayer();
  });
  $("player-toggle").addEventListener("click", () => {
    const video = $("video-player");
    if (video.paused)
      video
        .play()
        .catch(() =>
          toast("浏览器阻止了自动播放，请点击播放器开始直播。", true),
        );
    else video.pause();
  });
  $("player-mute").addEventListener("click", () => {
    const video = $("video-player");
    video.muted = !video.muted;
    updatePlayerControls();
  });
  let fullscreenPending = false;
  function toggleFullscreen() {
    if (fullscreenPending) return;
    const stage = $("player-stage");
    if (document.fullscreenElement || document.webkitFullscreenElement) {
      (document.exitFullscreen || document.webkitExitFullscreen)?.call(
        document,
      );
      return;
    }
    fullscreenPending = true;
    const request = (stage.requestFullscreen || stage.webkitRequestFullscreen)
      ?.call(stage);
    if (request?.finally)
      request
        .catch(() => toast("当前浏览器无法进入全屏。", true))
        .finally(() => {
          fullscreenPending = false;
        });
    else fullscreenPending = false;
  }
  $("player-fullscreen").addEventListener("click", toggleFullscreen);
  $("video-player").addEventListener("dblclick", (event) => {
    event.preventDefault();
    toggleFullscreen();
  });
  document.addEventListener("fullscreenchange", () => {
    const full = !!(
      document.fullscreenElement || document.webkitFullscreenElement
    );
    if (!full) fullscreenExitedAt = Date.now();
    setIcon($("player-fullscreen"), full ? "exitFullscreen" : "fullscreen");
    $("player-fullscreen").setAttribute(
      "aria-label",
      full ? "退出全屏" : "全屏",
    );
  });
  for (const event of ["play", "pause", "volumechange"])
    $("video-player").addEventListener(event, updatePlayerControls);

  $("video-player").addEventListener("timeupdate", () => saveProgress());
  $("video-player").addEventListener("pause", () => saveProgress(true));
  $("video-player").addEventListener("ended", () => saveProgress(true));
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) saveProgress(true, true);
  });
  $("search-input").addEventListener("input", (event) => {
    state.query = event.target.value.trim();
    state.onlinePage = 1;
    $("search-clear").hidden = !event.target.value;
    clearTimeout(state.searchTimer);
    state.searchTimer = setTimeout(() => {
      if (["online", "library", "live", "series"].includes(state.page))
        render();
    }, 250);
  });
  $("search-clear").addEventListener("click", () => {
    const input = $("search-input");
    input.value = "";
    state.query = "";
    state.onlinePage = 1;
    $("search-clear").hidden = true;
    input.focus?.();
    render();
  });
  document.addEventListener("keydown", (event) => {
    if (
      event.key === "/" &&
      !["INPUT", "TEXTAREA", "SELECT"].includes(document.activeElement?.tagName)
    ) {
      event.preventDefault();
      $("search-input").focus();
    }
  });
  window.addEventListener("beforeunload", () => saveProgress(true, true));
  boot();
})();
