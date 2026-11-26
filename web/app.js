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
    onlineArea: "",
    categories: null,
    onlineObserver: null,
    liveGroup: "all",
    epgOnlyCurrent: false,
    settingsTab: "account",
    showAllChannels: false,
    downloadFilter: "all",
    expandedSeries: new Set(),
    expandAllSeries: false,
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
  const areaPatterns = new Map([
    ["中国大陆", /大陆|大陸|内地|內地|^(中国|中國)$|\b(china|cn|mainland)\b/i],
    ["港台", /港台|香港|台湾|臺灣|台灣|澳门|澳門|\b(hong kong|taiwan|macao|macau|hk|tw)\b/i],
    ["美国", /美国|美國|\b(usa?|united states|america)\b/i],
    ["日本", /日本|\b(japan|jp)\b/i],
    ["韩国", /韩国|韓國|南韩|南韓|\b(korea|kr)\b/i],
    ["欧美", /欧美|歐美|欧洲|歐洲|英国|英國|法国|法國|德国|德國|意大利|義大利|西班牙|加拿大|俄罗斯|俄羅斯|瑞典|丹麦|荷兰|挪威|芬兰|比利时|瑞士|奥地利|爱尔兰|波兰|\b(europe|uk|britain|france|germany|italy|canada|spain|russia)\b/i],
  ]);
  const areaOptions = [["", "全部地区"], ...[...areaPatterns.keys(), "其他"].map((area) => [area, area])];
  function matchesArea(item) {
    const selected = state.onlineArea;
    if (!selected) return true;
    const area = String(item.area || "").trim();
    if (selected === "其他") return ![...areaPatterns.values()].some((pattern) => pattern.test(area));
    return areaPatterns.get(selected).test(area);
  }
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
  const playbackSpeeds = [1, 1.25, 1.5, 2];
  const speedLabel = (value) =>
    ({ 1: "1.0×", 1.25: "1.25×", 1.5: "1.5×", 2: "2.0×" })[value] ||
    `${value}×`;
  const storedNumber = (key, fallback) => {
    try {
      if (typeof localStorage === "undefined") return fallback;
      const raw = localStorage.getItem(key);
      if (raw === null) return fallback;
      const value = Number(raw);
      return Number.isFinite(value) ? value : fallback;
    } catch (_) {
      return fallback;
    }
  };
  const storeNumber = (key, value) => {
    try {
      if (typeof localStorage === "undefined") return;
      localStorage.setItem(key, String(value));
    } catch (_) {}
  };
  let playbackSpeed = storedNumber("polyview-speed", 1);
  if (!playbackSpeeds.includes(playbackSpeed)) playbackSpeed = 1;
  const episodeCode = (number) => `S01E${String(number).padStart(2, "0")}`;
  function syncConnection(
    online = typeof navigator === "undefined" ? true : navigator.onLine !== false,
  ) {
    const banner = $("connection-banner");
    if (banner) banner.hidden = online;
  }
  function paintSpeedButton() {
    const button = $("player-speed");
    if (!button) return;
    button.textContent = speedLabel(playbackSpeed);
    button.setAttribute("aria-label", `播放速度 ${playbackSpeed} 倍`);
    button.title = button.getAttribute("aria-label");
  }
  function cycleSpeed() {
    const next =
      playbackSpeeds[
        (playbackSpeeds.indexOf(playbackSpeed) + 1) % playbackSpeeds.length
      ];
    playbackSpeed = next;
    storeNumber("polyview-speed", next);
    const video = $("video-player");
    if (video) video.playbackRate = next;
    paintSpeedButton();
  }
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
        ` ${{ online: "影视发现", calendar: "放送日历", library: "媒体库", live: "电视直播", favorites: "我的收藏", series: "追剧清单", downloads: "下载任务", settings: "账户与设置" }[page]}`,
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
        on: { click: () => {
          const key = item.kind === "episode" && seriesKey(item);
          if (!key) return play(item);
          const episodes = state.library.filter((entry) => entry.kind === "episode" && seriesKey(entry) === key);
          play(item, false, episodes, episodes.indexOf(item));
        } },
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
  const seriesKey = (item) =>
    item.series_id != null
      ? `id:${item.series_id}`
      : item.series
        ? `name:${item.series}`
        : null;
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
    const seriesGroups = new Map();
    const singles = [];
    if (state.filter === "all") {
      for (const item of list) {
        const key = item.kind === "episode" && seriesKey(item);
        if (!key) singles.push(item);
        else {
          if (!seriesGroups.has(key)) seriesGroups.set(key, []);
          seriesGroups.get(key).push(item);
        }
      }
    }
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
    if (seriesGroups.size)
      filters.append(
        button(
          state.expandAllSeries ? "收起全部单集" : "展开全部单集",
          "subtle small-button",
          () => {
            state.expandAllSeries = !state.expandAllSeries;
            state.expandedSeries.clear();
            render();
          },
        ),
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
    } else if (state.filter === "all") {
      const grid = make("div", { class: "media-grid" });
      if (!singles.length && !seriesGroups.size)
        grid.append(
          empty("⌕", "没有找到相关内容", "换一个关键词，或切换媒体筛选。"),
        );
      else singles.forEach((item, index) => grid.append(card(item, index)));
      let index = 0;
      seriesGroups.forEach((items, key) => {
        const name = items[0].series || "剧集";
        const watched = items.filter((item) => item.watched).length;
        const expanded =
          state.expandAllSeries || state.expandedSeries.has(key);
        const summary = make("article", { class: "media-card stack-card" }, [
          make(
            "button",
            {
              type: "button",
              class: "poster-button",
              "aria-label": `${expanded ? "收起" : "展开"}剧集 ${name}`,
              "aria-expanded": String(expanded),
              on: {
                click: () => {
                  if (state.expandAllSeries) {
                    state.expandAllSeries = false;
                    state.expandedSeries = new Set(seriesGroups.keys());
                  }
                  if (state.expandedSeries.has(key))
                    state.expandedSeries.delete(key);
                  else state.expandedSeries.add(key);
                  render();
                },
              },
            },
            make("div", { class: `poster-art stack-art tone-${index++ % 6}` }, [
              make("span", { class: "poster-name", text: name }),
              make("span", {
                class: "stack-badge",
                text: `共 ${items.length} 集 · 已看 ${watched} 集`,
              }),
            ]),
          ),
          make("div", { class: "media-meta" }, [
            make("span", { text: "剧集" }),
            make("span", {
              class: "watched",
              text: expanded ? "已展开" : "点击展开",
            }),
          ]),
        ]);
        const section = make("section", {
          class: "library-series library-stack",
        }, [
          sectionTitle(
            name,
            items.length,
            `共 ${items.length} 集 · 已看 ${watched} 集`,
          ),
          summary,
        ]);
        if (expanded) {
          const episodeGrid = make("div", { class: "media-grid" });
          items.forEach((item, episodeIndex) =>
            episodeGrid.append(card(item, episodeIndex)),
          );
          section.append(episodeGrid);
        }
        root.append(section);
      });
      root.append(grid);
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
  function liveUrl(player, index) {
    const source = player.sources[index];
    if (!player.shiftSeconds || !source) return source;
    const url = new URL(source);
    url.searchParams.set("start", String(-player.shiftSeconds));
    return url.href;
  }
  function paintTimeshift(player) {
    const bar = $("player-timeshift");
    bar.replaceChildren(...[0, 5, 15, 30].map((minutes) => {
      const active = player.shiftSeconds === minutes * 60;
      return button(minutes ? `-${minutes}分` : "直播", `filter-chip${active ? " active" : ""}`, () => {
        if (active || state.player !== player) return;
        player.shiftSeconds = minutes * 60;
        player.failed.clear();
        paintTimeshift(player);
        attachPlayerSource();
      }, { "aria-pressed": String(active) });
    }), make("span", { class: "hint", text: "时移回看依赖上游支持" }));
  }
  async function probeLiveSources() {
    const player = state.player;
    if (!player?.live || player.sources.length < 2 || player.probing) return;
    const attempt = player.attempt;
    const controller = typeof AbortController === "function" ? new AbortController() : null;
    const timer = controller && setTimeout(() => controller.abort(), 10000);
    player.probing = true;
    const control = $("player-best-source");
    control.disabled = true;
    try {
      const index = await Promise.any(player.sources.map(async (_, i) => {
        const url = `/api/playback?url=${encodeURIComponent(liveUrl(player, i))}`;
        const response = await fetch(url, { method: "GET", cache: "no-store", signal: controller?.signal });
        response.body?.cancel().catch(() => {});
        if (!response.ok) throw new Error();
        return i;
      }));
      if (state.player !== player || player.attempt !== attempt) return;
      player.failed.clear();
      if (player.sourceIndex !== index) {
        player.sourceIndex = index;
        attachPlayerSource();
      }
      toast(`已选择线路 ${index + 1}`);
    } catch (_) {
      if (state.player === player && player.attempt === attempt) toast("没有可用线路", true);
    } finally {
      clearTimeout(timer);
      controller?.abort();
      player.probing = false;
      if (state.player === player) control.disabled = false;
    }
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
  async function changeChannelFavorite(channel, star, refresh) {
    if (star.disabled) return;
    star.disabled = true;
    const favorite = !channel.favorite;
    try {
      await api(`/api/channels/${encodeURIComponent(channel.id)}/favorite`, { method: "PUT", body: { favorite } });
      channel.favorite = favorite;
      if (state.player?.live && state.player.item.id === channel.id) {
        state.player.item.favorite = favorite;
        updatePlayerControls();
      }
      refresh();
    } catch (error) { toast(error.message, true); }
    finally { star.disabled = false; }
  }
  function updatePlayerControls() {
    const player = state.player;
    const video = $("video-player");
    const live = Boolean(player?.live);
    $("player-timeshift").hidden = !live;
    $("player-best-source").hidden = !live || player.sources.length < 2;
    $("player-best-source").disabled = Boolean(player?.probing);
    for (const id of ["player-toggle", "player-mute", "player-fullscreen"])
      $(id).hidden = !live;
    const playlistSize = player?.playlist?.length || 0;
    $("player-prev").hidden =
      !playlistSize || player.playlistIndex <= 0;
    $("player-next").hidden =
      !playlistSize || player.playlistIndex >= playlistSize - 1;
    $("player-prev").setAttribute("aria-label", live ? "上一台" : "上一集");
    $("player-next").setAttribute("aria-label", live ? "下一台" : "下一集");
    $("player-speed").hidden = !player || live;
    const star = $("player-favorite");
    if (star) {
      star.hidden = !live || player.item.id == null;
      if (!star.hidden) {
        star.textContent = player.item.favorite ? "★" : "☆";
        star.setAttribute("aria-pressed", String(Boolean(player.item.favorite)));
        star.setAttribute("aria-label", player.item.favorite ? "取消收藏频道" : "收藏频道");
        star.title = star.getAttribute("aria-label");
      }
    }
    const guideToggle = $("player-guide-toggle");
    guideToggle.hidden = !live;
    if (live) {
      const collapsed = Boolean(player.guideCollapsed);
      guideToggle.textContent = collapsed ? "显示节目单" : "收起节目单";
      guideToggle.setAttribute("aria-label", guideToggle.textContent);
      guideToggle.title = guideToggle.textContent;
      guideToggle.setAttribute("aria-pressed", String(!collapsed));
    }
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
  function skipKey(item) {
    if (item.source_id != null && item.media_id != null) return `online:${item.source_id}:${item.media_id}`;
    return !item.remote_url && item.id != null ? `local:${item.id}` : null;
  }
  const pendingSkipWrites = new Map();
  function skipPanel(key, player = null) {
    const enabled = make("input", { type: "checkbox" });
    const seconds = (name) => make("input", { type: "number", name, min: 0, max: 600, step: "any", required: true, value: 0 });
    const intro = seconds("intro_sec");
    const outro = seconds("outro_sec");
    const message = make("span", { class: "hint", role: "status" });
    const fields = make("fieldset", { class: "skip-fields", disabled: true, "aria-label": "跳过设置" }, [
      make("label", { class: "checkbox-label" }, [enabled, "启用自动跳过"]),
      make("label", {}, ["片头（秒）", intro]),
      make("label", {}, ["片尾（秒）", outro]),
    ]);
    function apply(config) {
      enabled.checked = Boolean(config?.enabled);
      intro.value = config?.intro_sec ?? 0;
      outro.value = config?.outro_sec ?? 0;
      if (player && state.player === player) {
        player.skip = { enabled: enabled.checked, intro_sec: Number(intro.value), outro_sec: Number(outro.value) };
        skipIntro(player);
      }
    }
    async function save() {
      if (fields.disabled) return;
      const config = { media_key: key, enabled: enabled.checked, intro_sec: Number(intro.value), outro_sec: Number(outro.value) };
      const validSeconds = (value) => Number.isFinite(value) && value >= 0 && value <= 600;
      if (!intro.value || !outro.value || !validSeconds(config.intro_sec) || !validSeconds(config.outro_sec)) {
        message.textContent = "片头和片尾须在 0 到 600 秒之间。";
        return;
      }
      fields.disabled = true;
      const write = api("/api/skip", { method: "PUT", body: config });
      pendingSkipWrites.set(key, write);
      try {
        await write;
        apply(config);
        message.textContent = "已保存跳过设置";
      } catch (error) { message.textContent = error.message; }
      finally {
        if (pendingSkipWrites.get(key) === write) pendingSkipWrites.delete(key);
        fields.disabled = false;
      }
    }
    if (player) {
      function mark(input) {
        const video = $("video-player");
        if (state.player !== player || !player.metadataReady || !Number.isFinite(video.duration)) {
          message.textContent = "请等待视频载入。";
          return;
        }
        const value = input === intro ? video.currentTime : video.duration - video.currentTime;
        input.value = Math.round(value * 1000) / 1000;
        return save();
      }
      fields.append(button("设当前时间为片头", "subtle small-button", () => mark(intro)),
        button("设当前剩余为片尾", "subtle small-button", () => mark(outro)));
    }
    fields.append(button("保存跳过设置", "primary small-button", save));
    const retry = button("重新读取", "subtle small-button", load, { hidden: true });
    async function load() {
      retry.hidden = true;
      try {
        // The detail panel may still be saving as playback starts.
        const pending = pendingSkipWrites.get(key);
        if (pending) await pending.catch(() => {});
        apply(await api(`/api/skip?key=${encodeURIComponent(key)}`));
        fields.disabled = false;
        message.textContent = "";
      } catch (error) {
        message.textContent = error.message;
        retry.hidden = false;
      }
    }
    load();
    return make("details", { class: "skip-panel" }, [make("summary", { text: "跳过片头/片尾" }), fields, message, retry]);
  }
  function skipIntro(player) {
    const video = $("video-player");
    if (!player.metadataReady || !player.skip || player.introChecked || !Number.isFinite(video.duration) || video.duration <= 0) return;
    player.introChecked = true;
    const { enabled, intro_sec } = player.skip;
    if (enabled && video.currentTime < intro_sec && intro_sec < video.duration) {
      video.currentTime = intro_sec;
      toast("已跳过片头");
    }
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
      ? liveUrl(player, player.sourceIndex)
      : player.item.remote_url;
    const source = upstream
      ? `/api/playback?url=${encodeURIComponent(upstream)}`
      : `/api/library/${encodeURIComponent(player.item.id)}/stream`;
    const hlsSource = /\.m3u8(?:[?#]|$)/i.test(upstream || source);
    const nativeHls =
      video.canPlayType("application/vnd.apple.mpegurl") === "probably";
    const startPlayback = () => {
      if ((!player.live && !player.autoplay) || !current()) return;
      const result = video.play();
      if (result?.catch)
        result.catch((error) => {
          if (!current()) return;
          if (error?.name === "NotAllowedError")
            $("player-message").textContent =
              player.live ? "浏览器阻止了自动播放，请点击播放器开始直播。" : "请点击播放器继续播放。";
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
      hls.on(window.Hls.Events.MANIFEST_PARSED, startPlayback);
      hls.loadSource(source);
      hls.attachMedia(video);
    } else {
      video.src = source;
      video.load();
      if (player.live) {
        video.onloadedmetadata = startPlayback;
        startPlayback();
      }
    }
    if (!player.live)
      video.onloadedmetadata = () => {
        if (!current()) return;
        if (
          n(player.item.position) > 0 &&
          (player.item.remote_url ? n(player.item.position) < video.duration : n(player.item.position) < video.duration - 1)
        )
          video.currentTime = player.item.position;
        player.metadataReady = true;
        skipIntro(player);
        if (!hlsSource || nativeHls || !state.hls) startPlayback();
      };
  }
  function playerGuide(item) {
    const box = $("player-guide");
    box.replaceChildren();
    if (!state.player?.live) {
      if (state.player) state.player.guideAvailable = false;
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
      state.player.guideAvailable = false;
      box.hidden = true;
      return;
    }
    state.player.guideAvailable = true;
    box.hidden = Boolean(state.player.guideCollapsed);
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
  function play(item, live = false, playlist = [], playlistIndex = 0, autoplay = false) {
    if (state.player) closePlayer();
    state.player = {
      item,
      live,
      autoplay,
      sources: live ? liveSources(item) : [],
      playlist,
      playlistIndex,
      sourceIndex: 0,
      shiftSeconds: 0,
      failed: new Set(),
      attempt: 0,
      lastSaved: 0,
      lastPayload: "",
      errorShown: false,
      guideCollapsed: false,
      guideAvailable: false,
      metadataReady: false,
      introChecked: false,
      finishing: false,
    };
    $("player-title").textContent = item.title || item.name || "正在播放";
    $("player-kind").textContent = live
      ? "LIVE NOW"
      : item.series || "NOW PLAYING";
    $("player-dialog").classList.toggle("live-mode", live);
    $("player-stage").classList.toggle("live-mode", live);
    $("player-dialog").showModal();
    const video = $("video-player");
    video.defaultPlaybackRate = live ? 1 : playbackSpeed;
    video.playbackRate = video.defaultPlaybackRate;
    video.volume = Math.min(1, Math.max(0, storedNumber("polyview-volume", 1)));
    video.muted = storedNumber("polyview-muted", 0) === 1;
    if (live) paintTimeshift(state.player);
    playerGuide(item);
    updatePlayerControls();
    $("player-hud-title").textContent = $("player-title").textContent;
    $("player-hud-kind").textContent = $("player-kind").textContent;
    const key = live ? null : skipKey(item);
    const skipHost = $("player-skip");
    skipHost.hidden = !key;
    skipHost.replaceChildren(...(key ? [skipPanel(key, state.player)] : []));
    attachPlayerSource();
  }

  function playPlaylist(offset, autoplay = false) {
    const current = state.player;
    if (!current || !current.playlist?.length) return;
    const index = current.playlistIndex + offset;
    if (index < 0 || index >= current.playlist.length) return;
    const item = current.playlist[index];
    play(item, current.live, current.playlist, index, autoplay);
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
    const item = current.item;
    const remote = Boolean(item.remote_url);
    const completed = current.finishing || video.ended || video.currentTime / video.duration > 0.9;
    let trackingSave;
    if (remote && item.tracking_id && completed && !item.marked_watched) {
      item.marked_watched = true;
      trackingSave = api(`/api/tracking/${item.tracking_id}`, {
        method: "PUT",
        body: { watched_episodes: Math.max(item.watched_episodes || 0, item.episode_number) },
        keepalive,
      }).catch((error) => {
        item.marked_watched = false;
        toast(`追剧进度未保存：${error.message}`, true);
      });
    }
    if (remote && (item.source_id == null || item.media_id == null)) return trackingSave;
    const body = {
      position: current.finishing ? video.duration : Math.min(video.duration, Math.max(0, n(video.currentTime))),
      duration: video.duration,
      ...(remote ? {
        source_id: item.source_id,
        media_id: item.media_id,
        title: item.catalog_title || item.title,
        cover: item.cover || "",
        episode_index: item.episode_index ?? 0,
        episode_name: item.episode_name || "",
        remote_url: item.remote_url,
      } : { watched: Boolean(item.watched || completed) }),
    };
    const payload = JSON.stringify(body);
    if (payload === current.lastPayload) return trackingSave;
    current.lastSaved = Date.now();
    current.lastPayload = payload;
    if (!remote) Object.assign(item, body);
    try {
      await api(
        remote ? "/api/progress" : `/api/library/${encodeURIComponent(item.id)}/progress`,
        { method: "PUT", body, keepalive },
      );
      current.errorShown = false;
    } catch (e) {
      if (current.lastPayload === payload) current.lastPayload = "";
      if (!current.errorShown) {
        toast(`观看进度未保存：${e.message}`, true);
        current.errorShown = true;
      }
    }
    await trackingSave;
  }
  function closePlayer() {
    if (!state.player) return;
    saveProgress(true, true).then(() => {
      if (
        state.user &&
        !state.player &&
        ["online", "favorites", "library", "series"].includes(state.page)
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
  const favoritesGroup = Symbol("favorite channels");
  function guideView(start, end, now = Math.floor(Date.now() / 1000), allChannels = state.channels) {
    const refreshGuide = () => $("live-guide")?.replaceChildren(guideView(start, end, now, allChannels));
    const guide = state.epg || {};
    const programs = Array.isArray(guide.programs) ? guide.programs : [];
    const byChannel = new Map();
    programs.forEach((program) => {
      if (!byChannel.has(program.channel_id)) byChannel.set(program.channel_id, []);
      byChannel.get(program.channel_id).push(program);
    });
    const groupCounts = new Map();
    state.channels.forEach((channel) => {
      const group = channel.group_name || "其他";
      groupCounts.set(group, (groupCounts.get(group) || 0) + 1);
    });
    const groups = [...groupCounts.keys()];
    const favoriteChannels = allChannels.filter((channel) => channel.favorite);
    if (state.liveGroup !== "all" && !groupCounts.has(state.liveGroup) && !(state.liveGroup === favoritesGroup && favoriteChannels.length))
      state.liveGroup = "all";
    const query = state.query.toLowerCase();
    const channels = (state.liveGroup === favoritesGroup ? favoriteChannels : state.channels).filter(
      (channel) =>
        (state.liveGroup === "all" || state.liveGroup === favoritesGroup || (channel.group_name || "其他") === state.liveGroup) &&
        (!query || `${channel.name} ${channel.group_name}`.toLowerCase().includes(query)),
    );
    const groupTab = (group, label) => button(label, `subtle small-button${state.liveGroup === group ? " active" : ""}`, () => {
      state.liveGroup = group;
      refreshGuide();
    }, { "aria-pressed": String(state.liveGroup === group) });
    const tabs = make("div", { class: "live-groups" }, [
      favoriteChannels.length ? groupTab(favoritesGroup, "★ 收藏") : null,
      groupTab("all", `全部 ${state.channels.length}`),
      ...groups.map((group) => groupTab(group, `${group} ${groupCounts.get(group)}`)),
    ]);
    let scroller;
    const panBack = button("← -2 小时", "subtle small-button", () => {
      if (scroller) scroller.scrollLeft = Math.max(0, n(scroller.scrollLeft) - 240);
    });
    const panForward = button("+2 小时 →", "subtle small-button", () => {
      if (scroller) scroller.scrollLeft = n(scroller.scrollLeft) + 240;
    });
    const currentOnly = button(
      state.epgOnlyCurrent ? "显示全部节目" : "只看当前",
      "subtle small-button",
      () => {
        state.epgOnlyCurrent = !state.epgOnlyCurrent;
        refreshGuide();
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
        panBack,
        panForward,
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
    channels.forEach((channel, channelIndex) => {
      const row = make("div", { class: "epg-row" });
      const star = icon(channel.favorite ? "★" : "☆", `${channel.favorite ? "取消收藏" : "收藏"} ${channel.name}`, () => changeChannelFavorite(channel, star, refreshGuide), "guide-star");
      star.setAttribute("aria-pressed", String(Boolean(channel.favorite)));
      row.append(make("div", { class: "epg-channel guide-channel-cell" }, [star,
        make(
          "button",
          {
            type: "button",
            class: "guide-channel-play",
            "aria-label": `播放 ${channel.name}`,
            on: { click: () => play(channel, true, channels, channelIndex) },
          },
          [
            channel.logo
              ? posterImage(channel.logo, { class: "epg-logo" })
              : make("span", { class: "epg-logo", text: "◉" }),
            make("span", { class: "epg-channel-copy" }, [
              make("strong", { text: channel.name }),
              make("small", {
                text: `${channel.group_name || "直播"} · ${(channel.sources || []).length || 1} 线路`,
              }),
            ]),
          ],
        ),
      ]));
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
              on: { click: () => play(channel, true, channels, channelIndex) },
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
    const visible = channelsWithPrograms(allChannels, state.epg?.programs || []);
    const withoutEpg = allChannels.length - visible.length;
    state.channels = state.showAllChannels ? allChannels : visible;
    const root = make(
      "div",
      {},
      head(
        "电视直播",
        "像翻节目单一样，挑一个正在播的节目。",
        make("div", { class: "head-actions" }, [
          withoutEpg || state.showAllChannels
            ? button(
                state.showAllChannels
                  ? "只看有节目单的频道"
                  : `显示全部频道（含 ${withoutEpg} 个无节目单）`,
                "subtle",
                () => {
                  state.showAllChannels = !state.showAllChannels;
                  render();
                },
              )
            : null,
          admin() ? button("导入 M3U", "primary", importM3u) : null,
        ]),
      ),
    );
    if (!state.channels.length && !allChannels.some((channel) => channel.favorite)) {
      const hasChannels = allChannels.length > 0;
      root.append(
        empty(
          "◉",
          hasChannels ? "节目单暂不可用" : "还没有直播频道",
          hasChannels
            ? "当前频道还没有可显示的节目，请刷新节目单或显示全部频道。"
            : admin()
              ? "导入一个 M3U 播放列表，把电视带回家。"
              : "请让管理员导入 M3U 播放列表。",
          admin() && !hasChannels
            ? button("导入 M3U", "primary", importM3u)
            : withoutEpg > 0
              ? button("显示全部频道", "subtle", () => {
                  state.showAllChannels = true;
                  render();
                })
              : null,
        ),
      );
      return root;
    }
    const guide = make("div", { id: "live-guide" });
    guide.append(guideView(start, end, now, allChannels));
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
    items.filter((item) => matchesKind(item) && matchesLanguage(item) && matchesArea(item)).forEach((item) => {
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
  function posterImage(src, attrs = {}) {
    const image = make("img", { alt: "", loading: "lazy", referrerpolicy: "no-referrer", ...attrs });
    let retried = false;
    image.addEventListener("error", () => {
      if (retried) {
        image.remove();
        return;
      }
      retried = true;
      image.setAttribute("src", `/api/image?url=${encodeURIComponent(src)}`);
    });
    image.setAttribute("src", src);
    return image;
  }
  const updateCatalogCard = (card, item) => {
    if (item.cover && !card._cover && card._art) {
      card._art.prepend(posterImage(item.cover));
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

  function spotlightCard(items) {
    const art = make("div", { class: "spotlight-art tone-0" });
    const title = make("h2");
    const meta = make("p", { class: "spotlight-meta" });
    const dots = make("div", { class: "spotlight-dots" });
    let active = 0;
    const paint = () => {
      const group = items[active];
      if (!group) return;
      art.replaceChildren(...(group.cover ? [posterImage(group.cover)] : []));
      title.textContent = displayCatalogTitle(group.title, group.year);
      const quality = sourceQuality(group.sources?.[0] || group);
      meta.textContent = [
        group.year,
        `${group.sources?.length || 1} 个影视源`,
        /4K|2K|1080P|720P|HD/i.test(quality) ? quality : "",
        kindLabel(group.kind),
      ]
        .filter(Boolean)
        .join(" · ");
      dots.replaceChildren(
        ...items.map((_, index) =>
          make("button", {
            type: "button",
            class: `spotlight-dot${index === active ? " active" : ""}`,
            "aria-pressed": String(index === active),
            "aria-label": `查看第 ${index + 1} 部焦点影片`,
            on: {
              click: () => {
                active = index;
                paint();
              },
            },
          }),
        ),
      );
    };
    paint();
    const copy = make("div", { class: "spotlight-copy" }, [
      make("p", { class: "eyebrow", text: "本周焦点" }),
      title,
      meta,
      make("div", { class: "spotlight-actions" }, [
        button("立即播放", "primary", () => openCatalogDetail(items[active])),
        button("查看片源", "subtle", () => openCatalogDetail(items[active])),
      ]),
    ]);
    if (items.length > 1) copy.append(dots);
    return make("section", {
      class: "spotlight",
      "aria-label": "焦点影片",
    }, [art, copy]);
  }
  function mediaKey(item) {
    return [item.source_id, item.media_id ?? item.id].map(encodeURIComponent).join("/");
  }
  function elapsedLabel(position) {
    const seconds = Math.floor(position);
    return [Math.floor(seconds / 60), seconds % 60].map((v) => String(v).padStart(2, "0")).join(":");
  }
  function savedCard(item, index, playLabel, onPlay, removeLabel, onRemove) {
    const title = item.title || item.name;
    const art = make("div", { class: `poster-art tone-${index % 6}` }, [
      item.cover || item.logo ? posterImage(item.cover || item.logo) : null,
      make("span", { class: "poster-name", text: title }),
      item.duration > 0 ? make("progress", { class: "media-progress", max: item.duration, value: item.position || 0 }) : null,
    ]);
    const remove = icon("×", removeLabel, async () => {
      remove.disabled = true;
      try { await onRemove(); }
      catch (error) { toast(error.message, true); }
      finally { remove.disabled = false; }
    }, "favorite-button");
    return make("article", { class: "media-card saved-card" }, [
      make("button", { class: "poster-button", type: "button", "aria-label": playLabel, on: { click: onPlay } }, art),
      remove,
      make("div", { class: "media-meta", text: [item.episode_name, item.position > 0 ? elapsedLabel(item.position) : "", item.year].filter(Boolean).join(" · ") }),
    ]);
  }
  function continueWatching(entries) {
    const section = make("section", { class: "continue-section" });
    const cards = make("div", { class: "continue-grid resume-grid" });
    function paint() {
      section.hidden = !entries.length;
      section.replaceChildren(sectionTitle("继续观看", entries.length), cards);
      cards.replaceChildren(...entries.map((item, index) => {
        const local = item.entry_kind === "local";
        const resume = () => {
          if (!local && !item.remote_url) return openCatalogDetail(item, true);
          play(local ? { ...item, id: item.local_id } : { ...item, catalog_title: item.title }, false, [], 0, true);
        };
        return savedCard(item, index, `续播 ${item.title}`, resume, `移除 ${item.title}`, async () => {
          const path = local ? `local/${encodeURIComponent(item.local_id)}` : `online/${mediaKey(item)}`;
          await api(`/api/progress/${path}`, { method: "DELETE" });
          const current = entries.indexOf(item);
          if (current >= 0) entries.splice(current, 1);
          paint();
        });
      }));
    }
    paint();
    return section;
  }
  async function favorites() {
    const data = await api("/api/favorites");
    const root = make("div", {}, head("我的收藏", "播放或管理已收藏的影片和频道。"));
    for (const [key, label] of [["catalog", "在线影视收藏"], ["local", "本地媒体收藏"], ["channels", "收藏直播频道"]]) {
      const items = data[key] || [];
      const section = make("section", { class: "favorites-section" });
      function paint() {
        const grid = make("div", { class: "media-grid" }, items.map((item, index) => {
          const title = item.title || item.name;
          const watch = () => {
            if (key === "catalog") return openCatalogDetail(item, true);
            if (key === "channels") return play(item, true, items.slice(), index, true);
            play(item, false, [], 0, true);
          };
          return savedCard(item, index, `播放 ${title}`, watch, `取消收藏 ${title}`, async () => {
            if (key === "catalog") await api(`/api/favorites/${mediaKey(item)}`, { method: "DELETE" });
            else {
              const path = key === "local" ? "library" : "channels";
              await api(`/api/${path}/${encodeURIComponent(item.id)}/favorite`, { method: "PUT", body: { favorite: false } });
              item.favorite = false;
            }
            const current = items.indexOf(item);
            if (current >= 0) items.splice(current, 1);
            paint();
          });
        }));
        section.replaceChildren(sectionTitle(label, items.length), items.length ? grid : make("p", { class: "hint", text: "暂无收藏" }));
      }
      paint();
      root.append(section);
    }
    return root;
  }
  function searchCatalogTitle(title) {
    Object.assign(state, {
      onlineSource: "all", onlineKind: "",
      onlineCategory: "", onlineYear: "", onlineLang: "", onlineArea: "", categories: null,
    });
    submitSearch(title, true);
  }

  function discoveryPoster(title, cover, metadata, index, action) {
    return make("button", {
      type: "button", class: "poster-button discovery-poster", "aria-label": title, on: { click: action },
    }, [
      make("div", { class: `poster-art catalog-art tone-${index % 6}` }, [
        cover ? posterImage(cover) : null,
        make("span", { class: "poster-name", text: title }),
      ]),
      metadata ? make("span", { class: "discovery-meta", text: metadata }) : null,
    ]);
  }

  async function loadRecommendations(box) {
    try {
      const data = await api("/api/catalog/recommend");
      for (const group of data.groups || []) {
        if (!group.items?.length) continue;
        box.append(make("section", { class: "recommend-section" }, [
          make("header", { class: "recommend-heading" }, [
            make("h2", { text: group.title }),
            make("span", { class: "recommend-origin", text: data.origin === "douban" ? "豆瓣" : "各源最新" }),
          ]),
          make("div", { class: "recommend-strip" }, group.items.map((item, index) =>
            discoveryPoster(item.title, item.cover, [item.year, Number(item.rating) > 0 ? `${item.rating} 分` : null].filter(Boolean).join(" · "), index,
              () => item.source_id != null && item.id != null
                ? openCatalogDetail({ ...item, kind: group.kind }) : searchCatalogTitle(item.title)),
          )),
        ]));
      }
    } catch (error) {
      box.append(make("p", { class: "hint", text: `热门榜单暂不可用：${error.message}` }));
    }
  }

  async function calendar() {
    const data = await api("/api/calendar");
    const root = make("div", {}, head("放送日历", "按放送日查看剧集，搜索片源后可加入追剧清单。"));
    if (data.enabled === false) {
      root.append(empty("▤", "放送日历已关闭", "管理员可以在设置中开启放送日历。"));
      return root;
    }
    const tabs = make("div", { class: "filters", "aria-label": "放送星期" });
    const list = make("div", { class: "media-grid" });
    const days = data.days || [];
    const selectDay = (weekday) => {
      Array.from(tabs.children).forEach((tab, index) => {
        const active = weekday === index + 1;
        tab.classList.toggle("active", active);
        tab.setAttribute("aria-pressed", String(active));
      });
      const items = days.find((day) => day.weekday === weekday)?.items || [];
      list.replaceChildren(...items.map((item, index) => {
        const title = item.title_cn || item.title_raw;
        return discoveryPoster(title, item.cover, Number(item.score) > 0 ? `${item.score} 分` : "暂无评分", index,
          () => searchCatalogTitle(title));
      }));
      if (!items.length) list.append(empty("▤", "当天暂无放送", "可以查看其他日期。"));
    };
    [..."一二三四五六日"].forEach((day, index) => {
      tabs.append(make("button", {
        type: "button", class: "filter-chip", text: `周${day}`, on: { click: () => selectDay(index + 1) },
      }));
    });
    selectDay(new Date().getDay() || 7);
    root.append(tabs, list);
    return root;
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
    const defaultView = !state.query && state.onlineSource === "all" && !state.onlineKind &&
      !state.onlineCategory && !state.onlineYear && !state.onlineLang && !state.onlineArea;
    if (defaultView) {
      const recommendations = make("div");
      root.append(recommendations);
      void loadRecommendations(recommendations);
    }
    if (!state.query) {
      try {
        const entries = await api("/api/progress");
        if (entries?.length) root.append(continueWatching(entries));
      } catch (error) {
        root.append(make("p", { class: "hint", text: `继续观看暂不可用：${error.message}` }));
      }
    }
    if (!enabled.length) {
      root.append(
        empty(
          "◈",
          "先连接一个影视源",
          admin()
            ? "在账户与设置中添加 MacCMS JSON 接口，即可浏览、搜索与播放。"
            : "请管理员配置影视源。",
          admin()
            ? button("配置影视源", "primary", () => {
                state.settingsTab = "sources";
                setNav("settings");
              })
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
          chipRow("地区", areaOptions, state.onlineArea, (value) => (state.onlineArea = value)),
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
          chipRow("地区", areaOptions, state.onlineArea, (value) => (state.onlineArea = value)),
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
    if (defaultView && groupOrder.length) {
      const featured = groupOrder.map((key) => groups.get(key)).filter((group) => group.cover).slice(0, 5);
      root.insertBefore(spotlightCard(featured.length ? featured : [groups.get(groupOrder[0])]), summary);
    }
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
  async function openCatalogDetail(item, playImmediately = false) {
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
          if (item.cover) art.append(posterImage(item.cover, { loading: "eager" }));
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
      const mediaId = item.id ?? item.media_id;
      const progressQuery = new URLSearchParams({ source_id: item.source_id, media_id: mediaId });
      const [detail, subscriptions, progress, saved] = await Promise.all([
        api(
          `/api/catalog/${item.source_id}/detail?id=${encodeURIComponent(mediaId)}`,
        ),
        api("/api/tracking"),
        api(`/api/progress?${progressQuery}`),
        api("/api/favorites"),
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
      const favoriteData = {
        source_id: detail.source_id, media_id: detail.id, title: detail.title,
        cover: detail.cover || item.cover || "", year: detail.year || item.year || "",
        kind: item.kind || "other", remark: detail.remark || "",
      };
      let isFavorite = saved?.catalog?.some((entry) => mediaKey(entry) === mediaKey(favoriteData)) || false;
      const favorite = button("", "subtle", async () => {
        favorite.disabled = true;
        try {
          if (isFavorite) await api(`/api/favorites/${mediaKey(favoriteData)}`, { method: "DELETE" });
          else await api("/api/favorites", { method: "POST", body: favoriteData });
          isFavorite = !isFavorite;
          paintFavorite();
        } catch (error) { toast(error.message, true); }
        finally { favorite.disabled = false; }
      });
      function paintFavorite() {
        favorite.textContent = isFavorite ? "★ 已收藏" : "☆ 收藏";
        favorite.setAttribute("aria-pressed", String(isFavorite));
      }
      paintFavorite();
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
          favorite,
          !tracked
            ? make("label", { class: "checkbox-label" }, [auto, "自动下载更新"])
            : make("span", {
                class: "muted",
                text: tracked.auto_download ? "自动下载已开启" : "已订阅更新",
              }),
        ]),
        skipPanel(skipKey(favoriteData)),
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
      let episodeAscending = true;
      const orderedEpisodes = () =>
        detail.groups[n(groups.value)].episodes
          .map((episode, index) => ({ ...episode, sourceIndex: index }))
          .sort((one, other) =>
            episodeAscending
              ? one.sourceIndex - other.sourceIndex
              : other.sourceIndex - one.sourceIndex,
          );
      const selected = () => orderedEpisodes()[n(episodes.value)];
      const watchedCount = tracked?.watched_episodes || 0;
      let batchMode = false;
      let submitting = false;
      const picked = new Set();
      let episodeButtons = [];
      const watch = (index, position = 0, autoplay = false) => {
        const group = detail.groups[n(groups.value)];
        if (group.direct === false) {
          toast("该线路是非直链解析页，无法直接播放，请选择直链线路。", true);
          return;
        }
        const playlist = orderedEpisodes().map((episode) => ({
          title: `${detail.title} · ${episode.name}`,
          remote_url: episode.url,
          catalog_title: detail.title,
          source_id: detail.source_id,
          media_id: detail.id,
          cover: favoriteData.cover,
          episode_index: episode.sourceIndex,
          episode_name: episode.name,
          tracking_id: tracked?.id,
          watched_episodes: watchedCount,
          episode_number: episode.sourceIndex + 1,
        }));
        dialog.close();
        playlist[index].position = position;
        play(playlist[index], false, playlist, index, autoplay);
      };
      const record = progress?.[0];
      function resume() {
        // Prefer the original stream when multiple providers label their episodes differently.
        let line = detail.groups.findIndex((group) => group.direct !== false && group.episodes.some((episode) => episode.url === record.remote_url));
        if (line < 0) line = detail.groups.findIndex((group) => group.direct !== false && group.episodes[record.episode_index]);
        if (line < 0) return toast("该集暂无可用直链线路。", true);
        groups.value = String(line);
        const ordered = orderedEpisodes();
        let index = ordered.findIndex((episode) => episode.url === record.remote_url);
        if (index < 0) index = ordered.findIndex((episode) => episode.sourceIndex === record.episode_index);
        watch(index, record.position, true);
      }
      const sortButton = button("倒序", "subtle small-button", () => {
        episodeAscending = !episodeAscending;
        sortButton.textContent = episodeAscending ? "倒序" : "正序";
        fillEpisodes();
      });
      const batchToggle = button("选择集数", "subtle small-button", () => {
        batchMode = !batchMode;
        if (!batchMode) picked.clear();
        paintBatch();
      });
      batchToggle.setAttribute("aria-pressed", "false");
      const selectAllUnwatched = button("全选未看", "subtle small-button", () => {
        picked.clear();
        orderedEpisodes()
          .filter((episode) => episode.sourceIndex >= watchedCount)
          .forEach((episode) => picked.add(episode.sourceIndex));
        paintBatch();
      });
      const selectAll = button("全选", "subtle small-button", () => {
        orderedEpisodes().forEach((episode) => picked.add(episode.sourceIndex));
        paintBatch();
      });
      async function submitBatch() {
        if (submitting || !picked.size) return;
        if (detail.groups[n(groups.value)].direct === false) {
          toast("该线路是非直链解析页，无法直接下载，请选择直链线路。", true);
          return;
        }
        submitting = true;
        paintBatch();
        let ok = 0;
        let failed = 0;
        for (const episode of orderedEpisodes()) {
          if (!picked.has(episode.sourceIndex)) continue;
          try {
            await api("/api/downloads", {
              method: "POST",
              body: {
                url: episode.url,
                title: `${detail.title} ${episodeCode(episode.sourceIndex + 1)}`,
              },
            });
            ok += 1;
          } catch (_) {
            failed += 1;
          }
        }
        if (failed) toast(`已加入 ${ok} 集，${failed} 集失败`, true);
        else toast(`已加入 ${ok} 集下载队列`);
        submitting = false;
        picked.clear();
        batchMode = false;
        fillEpisodes();
      }
      const batchButton = button(
        "批量加入下载队列（0 集）",
        "primary small-button",
        submitBatch,
      );
      batchButton.disabled = true;
      batchButton.hidden = true;
      function paintBatch() {
        batchButton.textContent = `批量加入下载队列（${picked.size} 集）`;
        batchButton.disabled = submitting || picked.size === 0;
        for (const control of [groups, sortButton, batchToggle, selectAll, selectAllUnwatched])
          control.disabled = submitting;
        batchToggle.textContent = batchMode ? "退出多选" : "选择集数";
        batchToggle.setAttribute("aria-pressed", String(batchMode));
        batchButton.hidden = !batchMode;
        selectAll.hidden = !batchMode;
        selectAllUnwatched.hidden = !batchMode;
        episodeButtons.forEach(({ node, index }) => {
          node.disabled = submitting;
          node.classList.toggle("picked", picked.has(index));
          if (batchMode) node.setAttribute("aria-pressed", String(picked.has(index)));
          else node.removeAttribute("aria-pressed");
        });
      }
      const fillEpisodes = () => {
        episodes.replaceChildren(
          ...orderedEpisodes().map((episode, index) =>
            make("option", { value: index, text: episode.name }),
          ),
        );
        episodeButtons = orderedEpisodes().map((episode, index) => ({
          index: episode.sourceIndex,
          node: button(
            episode.sourceIndex < watchedCount
              ? `✓ ${episode.name}`
              : episode.name,
            `subtle small-button${episode.sourceIndex < watchedCount ? " watched" : ""}`,
            () => {
              if (!batchMode) {
                watch(index);
                return;
              }
              if (picked.has(episode.sourceIndex))
                picked.delete(episode.sourceIndex);
              else picked.add(episode.sourceIndex);
              paintBatch();
            },
          ),
        }));
        episodeGrid.replaceChildren(
          sortButton,
          ...episodeButtons.map(({ node }) => node),
        );
        paintBatch();
      };
      groups.addEventListener("change", () => {
        picked.clear();
        batchMode = false;
        fillEpisodes();
      });
      fillEpisodes();
      const download = button("下载本集", "subtle", async () => {
        if (detail.groups[n(groups.value)].direct === false) {
          toast("该线路是非直链解析页，无法直接下载，请选择直链线路。", true);
          return;
        }
        download.disabled = true;
        const episode = selected();
        try {
          await api("/api/downloads", {
            method: "POST",
            body: {
              url: episode.url,
              title: `${detail.title} ${episodeCode(episode.sourceIndex + 1)}`,
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
          record?.position > 0 ? button(`续播 ${record.episode_name || "本集"}（${elapsedLabel(record.position)}）`, "subtle", resume) : null,
          download,
        ]),
        make("div", { class: "catalog-batch" }, [
          batchToggle,
          selectAllUnwatched,
          selectAll,
          batchButton,
        ]),
        episodeGrid,
        make("p", {
          class: "hint",
          text: "播放与下载使用来源提供的地址；标为“非直链”的线路需要解析器，可能无法播放。下载完成后自动进入媒体库。",
        }),
      );
      dialog.append(content);
      if (playImmediately) {
        if (record?.position > 0) resume();
        else if (selected()) watch(n(episodes.value), 0, true);
      }
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
        const watchedCount = n(subscription.watched_episodes);
        const totalCount = n(subscription.total_episodes);
        const step = (delta) => {
          const max = totalCount || Number.MAX_SAFE_INTEGER;
          watched.value = String(
            Math.min(max, Math.max(0, n(watched.value) + delta)),
          );
        };
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
        const pausedBadge = make("span", { class: "tracking-status paused", text: "已暂停", hidden: !subscription.paused });
        const pause = button(subscription.paused ? "恢复检查" : "暂停检查", "subtle small-button", async () => {
          const paused = !subscription.paused;
          pause.disabled = true;
          try {
            await api(`/api/tracking/${subscription.id}`, { method: "PUT", body: { paused } });
            subscription.paused = paused;
            pausedBadge.hidden = !paused;
            pause.textContent = paused ? "恢复检查" : "暂停检查";
            toast(paused ? "已暂停检查更新" : "已恢复检查更新");
          } catch (error) { toast(error.message, true); }
          finally { pause.disabled = false; }
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
            make("label", {}, [
              "已看",
              icon("−", `${subscription.title} 减少一集`, () => step(-1), "step-button"),
              watched,
              icon("+", `${subscription.title} 增加一集`, () => step(1), "step-button"),
            ]),
            button("保存", "subtle small-button", null, { type: "submit" }),
          ],
        );
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
            make("span", { class: "series-cover" },
              subscription.cover
                ? posterImage(subscription.cover)
                : "▤",
            ),
            make("div", { class: "series-info" }, [
              make("h3", { text: subscription.title }),
              make("span", {
                class: `tracking-status${statusLabel === "来源异常" ? " error" : ""}`,
                text: statusLabel,
              }),
              pausedBadge,
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
              pause,
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
  function statusGroup(job) {
    const value = job.status || "";
    if (/completed|done/i.test(value)) return "done";
    if (/failed|error|cancel/i.test(value)) return "problem";
    if (value === "running" || value === "downloading") return "running";
    return "queued";
  }
  function downloadList() {
    const jobs =
      state.downloadFilter === "all"
        ? state.downloads
        : state.downloads.filter(
            (job) => statusGroup(job) === state.downloadFilter,
          );
    const list = make("div", { class: "download-list" });
    if (!jobs.length)
      list.append(
        empty(
          "↓",
          state.downloads.length ? "没有符合条件的任务" : "下载队列是空的",
          state.downloads.length
            ? "换一个筛选条件试试。"
            : "添加一个你有权保存的视频直链或 HLS 地址。",
        ),
      );
    jobs.forEach((job) => {
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
    if (state.page !== "downloads") return;
    const existing = main.querySelector(".download-list");
    if (existing) {
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
    const counts = {
      all: state.downloads.length,
      running: 0,
      queued: 0,
      done: 0,
      problem: 0,
    };
    state.downloads.forEach((job) => {
      counts[statusGroup(job)] += 1;
    });
    root.append(
      add,
      sectionTitle("全部任务", state.downloads.length),
      make(
        "div",
        { class: "filters" },
        [
          ["all", "全部"],
          ["running", "下载中"],
          ["queued", "排队中"],
          ["done", "已完成"],
          ["problem", "异常"],
        ].map(([key, label]) =>
          make("button", {
            type: "button",
            class: `filter-chip${state.downloadFilter === key ? " active" : ""}`,
            "aria-pressed": String(state.downloadFilter === key),
            text: `${label} ${counts[key]}`,
            on: {
              click: () => {
                state.downloadFilter = key;
                render();
              },
            },
          }),
        ),
      ),
      downloadList(),
    );
    return root;
  }
  async function liveSubscriptionSettings() {
    const path = "/api/live-sources";
    const name = make("input", { name: "name", required: true });
    const url = make("input", { name: "url", type: "url", required: true });
    const rows = make("div", { class: "source-list" });
    const message = make("p", { class: "hint", role: "status" });
    const add = button("添加直播源", "primary", null, { type: "submit" });
    let busy = false;
    async function change(action) {
      if (busy) return;
      busy = true;
      try {
        await action();
        await load();
      } catch (error) { message.textContent = error.message; }
      finally { busy = false; }
    }
    async function load() {
      const sources = await api(path);
      rows.replaceChildren(...sources.map((source) => {
        const endpoint = `${path}/${source.id}`;
        return make("div", { class: "form-row" }, [
          make("strong", { text: source.name }), make("span", { text: source.url }),
          make("span", { class: "hint", text: `${source.enabled ? "已启用" : "已停用"} · ${source.channel_count} 个频道` }),
          source.error && make("span", { class: "form-error", text: source.error }),
          button(source.enabled ? "停用" : "启用", "subtle", () => change(() => api(endpoint, {
            method: "PUT", body: { name: source.name, url: source.url, enabled: !source.enabled },
          }))),
          button("删除", "subtle", () => {
            if (window.confirm(`删除直播源“${source.name}”？`)) return change(() => api(endpoint, { method: "DELETE" }));
          }),
        ]);
      }));
    }
    const form = make("form", { class: "form-row", on: { submit: (event) => {
      event.preventDefault();
      return change(async () => {
        await api(path, { method: "POST", body: { name: name.value.trim(), url: url.value.trim(), enabled: true } });
        name.value = "";
        url.value = "";
        message.textContent = "直播源已添加";
      });
    } } }, [make("label", {}, ["直播源名称", name]), make("label", {}, ["M3U 地址", url]), add]);
    const refresh = button("刷新已启用直播源", "subtle", () => change(async () => {
      const result = await api(path + "/refresh", { method: "POST" });
      message.textContent = `已刷新 ${result.refreshed} 个直播源，${result.failed} 个失败`;
    }));
    try { await load(); } catch (error) { message.textContent = error.message; }
    return make("section", { class: "panel source-settings" }, [make("h2", { text: "远程 M3U 直播源管理" }), form, refresh, rows, message]);
  }
  async function channelAliasSettings() {
    const path = "/api/epg/aliases";
    const panel = make("section", { class: "panel source-settings" }, make("h2", { text: "频道别名映射" }));
    try {
      const aliases = await api(path);
      const rows = make("div");
      const fields = new Map();
      function appendAlias(alias) {
        const from = make("input", { name: "from", required: true, value: alias.from });
        const to = make("input", { name: "to", required: true, value: alias.to });
        const row = make("div", { class: "form-row" }, [make("label", {}, ["直播频道名", from]), make("label", {}, ["节目单频道名", to]),
          button("移除映射", "subtle", () => { fields.delete(row); rows.replaceChildren(...fields.keys()); }),
        ]);
        fields.set(row, { from, to });
        rows.append(row);
      }
      aliases.forEach(appendAlias);
      const save = button("保存频道别名", "primary", null, { type: "submit" });
      panel.append(make("form", { on: { submit: async (event) => {
        event.preventDefault();
        if (save.disabled) return;
        save.disabled = true;
        try {
          const body = Array.from(fields.values(), ({ from, to }) => ({ from: from.value.trim(), to: to.value.trim() }));
          await api(path, { method: "PUT", body });
          toast("频道别名已保存");
        } catch (error) { toast(error.message, true); }
        finally { save.disabled = false; }
      } } }, [rows, button("添加映射", "subtle", () => appendAlias({ from: "", to: "" })), save]));
    } catch (error) { panel.append(make("p", { class: "form-error", text: error.message })); }
    return panel;
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
  const sourceAdvancedFields = [
    ["custom_ua", "User-Agent"], ["custom_referer", "Referer"],
    ["custom_cookie", "Cookie", "password"], ["proxy_url", "代理地址"],
    ["timeout_secs", "超时（秒）", "number"], ["searchable", "允许搜索", "checkbox"],
  ];
  function settingsFields(definitions, values) {
    const inputs = definitions.map(([name, label, type = "text"]) => {
      const attrs = { name, type };
      if (type === "checkbox") attrs.checked = Boolean(values[name]);
      else attrs.value = values[name] ?? "";
      if (type === "number") Object.assign(attrs, { min: 3, max: 60, required: true });
      const input = make("input", attrs);
      return { name, type, input, label: make("label", { class: type === "checkbox" ? "checkbox-label" : "" },
        type === "checkbox" ? [input, label] : [label, input]) };
    });
    return {
      labels: inputs.map((field) => field.label),
      values: () => Object.fromEntries(inputs.map(({ name, type, input }) => [name,
        type === "checkbox" ? Boolean(input.checked) : type === "number" ? Number(input.value) : input.value.trim(),
      ])),
    };
  }
  async function networkSettings() {
    const panel = make("section", { class: "network-settings" }, make("h3", { text: "播放与网络" }));
    try {
      const fields = settingsFields([
        ["ad_block", "HLS 保守去广告", "checkbox"], ["play_referer", "播放 Referer"],
        ["play_ua", "播放 User-Agent"], ["image_referer", "图片 Referer"],
      ], await api("/api/settings/play"));
      panel.append(make("form", { class: "form-row", on: { submit: async (event) => {
        event.preventDefault();
        try {
          await api("/api/settings/play", { method: "PUT", body: fields.values() });
          toast("播放与网络设置已保存");
        } catch (error) { toast(error.message, true); }
      } } }, [...fields.labels, button("保存播放设置", "primary", null, { type: "submit" })]));
    } catch (error) { panel.append(make("p", { class: "form-error", text: error.message })); }
    return panel;
  }
  function sourceHealth(source) {
    const ok = source.last_probe_ok, ms = source.last_latency_ms;
    const color = ok == null ? "unknown" : !ok ? "failed" : ms == null ? "unknown" : ms < 500 ? "fast" : ms < 1500 ? "medium" : "slow";
    const text = ok == null ? "未测速" : !ok ? "测速失败" : ms == null ? "可用" : `${ms} ms`;
    return make("span", { class: `source-health ${color}`, text:
      source.cooldown_until > Date.now() / 1000 ? `${text} · 冷却中` : text });
  }
  function sourceBatchDialog(sources) {
    const data = make("textarea", { rows: 10, "aria-label": "影视源 JSON", spellcheck: "false" });
    const title = make("h3");
    const dialog = make("dialog", { class: "info-dialog source-batch-dialog", "aria-labelledby": "source-batch-title" });
    title.setAttribute("id", "source-batch-title");
    const submit = button("确认导入", "primary", async () => {
      submit.disabled = true;
      try {
        const rows = JSON.parse(data.value);
        if (!Array.isArray(rows)) throw new Error("请粘贴影视源 JSON 数组");
        const result = await api("/api/sources/batch", { method: "POST", body: rows });
        dialog.close();
        toast(`已导入 ${result.imported} 个，跳过 ${result.skipped} 个`);
        render();
      } catch (error) { toast(error.message, true); }
      finally { submit.disabled = false; }
    });
    dialog.append(title, data, submit, button("关闭", "subtle", () => dialog.close()));
    function open(exporting) {
      const keys = ["name", "url", "enabled", "adult", "priority", ...sourceAdvancedFields.map(([key]) => key)];
      title.textContent = exporting ? "导出影视源 JSON" : "导入影视源 JSON";
      data.value = exporting ? JSON.stringify(sources.map((source) =>
        Object.fromEntries(keys.map((key) => [key, source[key]]))), null, 2) : "";
      data.readOnly = exporting;
      submit.hidden = exporting;
      dialog.showModal();
      if (exporting) data.select?.();
    }
    return [button("导入 JSON", "subtle", () => open(false)), button("导出 JSON", "subtle", () => open(true)), dialog];
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
      function advancedEditor(source = {}) {
        const fields = settingsFields(sourceAdvancedFields, { searchable: true, timeout_secs: 12, ...source });
        return {
          values: fields.values,
          panel: make("details", { class: "source-advanced" }, [
            make("summary", { text: "高级配置" }), make("div", { class: "form-row" }, fields.labels),
          ]),
        };
      }
      const newAdvanced = advancedEditor();
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
                    ...newAdvanced.values(),
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
          newAdvanced.panel,
          button("添加影视源", "primary", null, { type: "submit" }),
        ],
      );
      panel.append(newForm);
      const badges = new Map();
      const probe = async (source) => {
        try {
          const results = await api("/api/sources/probe", { method: "POST", body: source ? { source_id: source.id } : {} });
          const updated = await api("/api/sources");
          updated.forEach((item) => badges.get(item.id)?.replaceChildren(sourceHealth(item)));
          toast(`测速完成：${results.filter((result) => result.ok).length} / ${results.length} 个可用`);
        } catch (error) { toast(error.message, true); }
      };
      panel.append(make("div", { class: "form-row" }, [button("全部测速", "subtle", () => probe()), ...sourceBatchDialog(sources)]));
      const move = async (index, offset) => {
        const ids = sources.map((source) => source.id);
        [ids[index], ids[index + offset]] = [ids[index + offset], ids[index]];
        try {
          await api("/api/sources/reorder", { method: "POST", body: { ids } });
          await render();
        } catch (error) { toast(error.message, true); }
      };
      sources.forEach((source, index) => {
        const badge = make("span", {}, sourceHealth(source));
        badges.set(source.id, badge);
        const advanced = advancedEditor(source);
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
                  const body = {
                    name: name.value.trim(), url: url.value.trim(),
                    enabled: enabled.checked, adult: adult.checked,
                    priority: source.priority, ...advanced.values(),
                  };
                  await api(`/api/sources/${source.id}`, { method: "PUT", body });
                  Object.assign(source, body);
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
            badge,
            button("测速", "subtle small-button", () => probe(source)),
            button("上移", "subtle small-button", () => move(index, -1), { disabled: index === 0 }),
            button("下移", "subtle small-button", () => move(index, 1), { disabled: index === sources.length - 1 }),
            advanced.panel,
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
    panel.append(await networkSettings());
    return panel;
  }
  function copyField(label, field, caption = "复制") {
    const copy = button(caption, "subtle small-button", async () => {
      try {
        await navigator.clipboard.writeText(field.value);
        copy.textContent = "✓ 已复制";
        setTimeout(() => { copy.textContent = caption; }, 1500);
        toast(`${label}已复制`);
      } catch (_) {
        field.focus();
        field.select();
        toast("已选中，请按 Ctrl+C（Mac 为 ⌘C）复制。");
      }
    });
    return copy;
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
      const configs = make("div", { class: "tvbox-configs" });
      panel.append(configs);
      [
        ["标准配置地址", feed.config_path, configs],
        ["Base58 配置地址（旧版影视仓兼容）", feed.config_b58_path, configs],
        ["媒体库接口", feed.catalog_path, panel],
        ["直播 M3U", feed.live_path, panel],
      ].forEach(([label, path, container]) => {
        if (!path) return;
        const url = new URL(path, location.origin).href;
        const input = make("input", {
          value: url,
          readonly: true,
          "aria-label": label,
          spellcheck: "false",
        });
        container.append(make("div", { class: "feed-row" }, [
          make("label", {}, [label, input]), copyField(label, input),
        ]));
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
  function accountSettings() {
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
    return panel;
  }
  function extractResourceLinks(text) {
    const links = new Set();
    const matches = text.match(/https?:\/\/[^\s<>"'`，。；、【】（）“”‘’|]+/gi) || [];
    for (const match of matches) {
      const link = match.replace(/[.,;]+$/, "").replace(/\)+$/, (end, index) => {
        const prefix = match.slice(0, index);
        const open = (prefix.match(/\(/g) || []).length - (prefix.match(/\)/g) || []).length;
        return end.slice(0, Math.max(0, open));
      });
      try {
        if (new URL(link).hostname) links.add(link);
      } catch (_) { /* Ignore malformed links in pasted text. */ }
    }
    return [...links];
  }
  function resourceLinkTool() {
    const text = make("textarea", { rows: 4, "aria-label": "待提取文本" });
    const result = make("textarea", { rows: 4, spellcheck: "false", "aria-label": "提取结果" });
    const destination = make("select", { "aria-label": "导入位置" }, [
      make("option", { value: "channels", text: "播放链接 → 直播频道" }),
      make("option", { value: "sources", text: "CMS 接口 → 在线影视源" }),
    ]);
    const copy = copyField("资源链接", result, "复制链接");
    const importLinks = button("导入链接", "primary", async () => {
      if (importLinks.disabled) return;
      const links = extractResourceLinks(result.value);
      if (!links.length) return toast("请先提取资源链接", true);
      importLinks.disabled = true;
      try {
        const rows = links.map((url) => {
          const parsed = new URL(url);
          return { name: `${parsed.host}${parsed.pathname}`.slice(0, 100), url };
        });
        if (destination.value === "sources") {
          const applied = await api("/api/sources/batch", { method: "POST", body: rows });
          toast(`已导入 ${applied.imported} 个影视源，跳过 ${applied.skipped} 个`);
        } else {
          const playlist = ["#EXTM3U", ...rows.map(({ name, url }) => `#EXTINF:-1,${name}\n${url}`)].join("\n");
          const applied = await api("/api/channels/import", { method: "POST", body: { playlist } });
          toast(`已导入 ${applied.imported} 个频道`);
        }
      } catch (error) { toast(error.message, true); }
      finally { importLinks.disabled = false; }
    });
    return make("details", { class: "panel resource-links" }, [
      make("summary", { text: "从文本提取资源链接" }),
      make("div", { class: "stack" }, [
        make("label", {}, ["粘贴含有 HTTP(S) 资源链接的文本", text]),
        button("提取链接", "subtle", () => {
          const links = extractResourceLinks(text.value);
          result.value = links.join("\n");
          toast(`已提取 ${links.length} 个不同的链接`);
        }),
        make("label", {}, ["提取结果（每行一个链接，导入时请保留同一类型）", result]),
        make("div", { class: "form-row" }, [make("label", {}, ["导入位置", destination]), copy, importLinks]),
      ]),
    ]);
  }
  function backupSettings() {
    const file = make("input", { type: "file", accept: ".json,application/json", "aria-label": "选择配置备份文件" });
    const restore = button("导入备份", "primary", async () => {
      if (restore.disabled) return;
      const selected = file.files?.[0];
      if (!selected) return toast("请选择 JSON 备份文件", true);
      restore.disabled = true;
      try {
        const backup = JSON.parse(await selected.text());
        if (!backup || typeof backup !== "object" || Array.isArray(backup)) throw new Error("请选择整站配置备份 JSON 文件");
        if (!window.confirm(`导入 ${selected.name}？将按地址合并影视源和直播源，并替换站点设置和节目单配置。账户与登录会话将保留。`)) return;
        await api("/api/settings/import", { method: "POST", body: backup });
        file.value = "";
        toast("整站配置已导入");
      } catch (error) { toast(error.message, true); }
      finally { restore.disabled = false; }
    });
    return [make("section", { class: "panel" }, [
      make("h2", { text: "配置导出 / 导入" }),
      make("p", { class: "panel-description", text: "完整配置备份含私密配置，包括影视源凭据、代理和通知设置。请妥善保管 JSON 文件。" }),
      make("a", { class: "button subtle", href: "/api/settings/export", download: "polyview-config.json", text: "导出完整配置备份" }),
      make("div", { class: "form-row backup-import" }, [make("label", {}, ["选择配置备份文件", file]), restore]),
    ]), resourceLinkTool()];
  }
  function memberPasswordDialog() {
    const title = make("h3", { id: "member-password-title" });
    const dialog = make("dialog", { class: "info-dialog", "aria-labelledby": "member-password-title" });
    const field = make("input", { type: "password", autocomplete: "new-password", required: true });
    const submit = button("确认重置", "primary", null, { type: "submit" });
    let target;
    dialog.addEventListener("close", () => { field.value = ""; });
    dialog.append(title, make("form", { class: "stack", on: { submit: async (event) => {
      event.preventDefault();
      if (!target || target.id === state.user.id || submit.disabled) return;
      const length = Array.from(field.value).length;
      if (length < 10 || length > 128) return toast("密码需要 10–128 个字符", true);
      submit.disabled = true;
      try {
        await api(`/api/users/${encodeURIComponent(target.id)}/password`, {
          method: "POST", body: { password: field.value },
        });
        field.value = "";
        dialog.close();
        toast("密码已重置，该成员需要重新登录");
      } catch (error) { toast(error.message, true); }
      finally { submit.disabled = false; }
    } } }, [
      make("label", {}, ["新密码（10–128 个字符）", field]),
      make("p", { class: "hint", text: "重置后，该成员的登录会话将立即失效。" }),
      make("div", { class: "head-actions" }, [submit, button("取消", "subtle", () => dialog.close())]),
    ]));
    return { dialog, open(member) {
      if (submit.disabled || member.id === state.user.id) return;
      target = member;
      title.textContent = `重置 ${member.username} 的密码`;
      field.value = "";
      dialog.showModal();
    } };
  }
  function memberRow(member, resetPassword) {
    const self = member.id === state.user.id;
    const path = `/api/users/${encodeURIComponent(member.id)}`;
    const role = make("select", { "aria-label": `${member.username} 的角色`, disabled: self }, [
      make("option", { value: "user", text: "家庭成员" }),
      make("option", { value: "admin", text: "管理员" }),
    ]);
    role.value = member.role;
    const toggle = make("button", {
      type: "button", class: "switch", role: "switch", disabled: self,
      "aria-label": `启用 ${member.username}`, "aria-checked": String(!member.disabled),
    });
    const status = make("small", { text: member.disabled ? "已禁用" : "已启用" });
    async function update(nextRole, disabled) {
      if (self || role.disabled) return;
      role.disabled = toggle.disabled = true;
      try {
        await api(path, { method: "PUT", body: { role: nextRole, disabled } });
        member.role = nextRole;
        member.disabled = disabled;
        toggle.setAttribute("aria-checked", String(!disabled));
        status.textContent = disabled ? "已禁用" : "已启用";
        toast("成员设置已更新");
      } catch (error) { toast(error.message, true); }
      finally {
        role.value = member.role;
        role.disabled = toggle.disabled = false;
      }
    }
    role.addEventListener("change", () => update(role.value, Boolean(member.disabled)));
    toggle.addEventListener("click", () => update(member.role, !member.disabled));
    return make("div", { class: "user-row member-settings-row" }, [
      make("span", { class: "avatar", text: member.username.slice(0, 1).toUpperCase() }),
      make("div", {}, [make("strong", { text: member.username }), status]), role, toggle,
      self ? make("span", { class: "small muted", text: "当前账户" })
        : button("重置密码", "subtle small-button", () => resetPassword(member)),
      self ? null : button("移除", "danger small-button", async () => {
        if (!window.confirm(`确定移除 ${member.username}？`)) return;
        try {
          await api(path, { method: "DELETE" });
          toast("成员已移除");
          render();
        } catch (error) { toast(error.message, true); }
      }),
    ]);
  }
  async function familySettings() {
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
      const reset = memberPasswordDialog();
      users.append(make("div", { class: "user-list" }, members.map((member) => memberRow(member, reset.open))), reset.dialog);
    } catch (e) {
      users.append(make("p", { class: "form-error", text: e.message }));
    }
    return users;
  }
  async function notificationCalendarSettings() {
    async function settingsPanel(path, title, definitions, actions) {
      const panel = make("section", { class: "panel" }, make("h2", { text: title }));
      try {
        const fields = settingsFields(definitions, await api(path));
        const save = button(`保存${title}设置`, "primary", null, { type: "submit" });
        panel.append(make("form", { class: "stack", on: { submit: async (event) => {
          event.preventDefault();
          save.disabled = true;
          try {
            await api(path, { method: "PUT", body: fields.values() });
            toast(`${title}设置已保存`);
          } catch (error) { toast(error.message, true); }
          finally { save.disabled = false; }
        } } }, [...fields.labels, save]));
        if (actions) panel.append(...actions);
      } catch (error) { panel.append(make("p", { class: "form-error", text: error.message })); }
      return panel;
    }
    const results = make("div", { role: "status", "aria-live": "polite" });
    const test = button("测试发送", "subtle", async () => {
      test.disabled = true;
      results.textContent = "正在测试已保存的通知渠道…";
      try {
        const response = await api("/api/settings/notify/test", { method: "POST" });
        if (!response.results.length) results.textContent = "尚未配置通知渠道";
        else results.replaceChildren(...response.results.map(({ channel, ok, message }) =>
          make("p", { class: ok ? "hint" : "form-error", text: `${channel}：${ok ? "成功" : "失败"} · ${message}` })));
      } catch (error) { results.textContent = `测试发送失败：${error.message}`; }
      finally { test.disabled = false; }
    });
    return Promise.all([
      settingsPanel("/api/settings/notify", "通知", [
        ["bark_url", "Bark 地址", "url"],
        ["webhook_post_url", "POST Webhook 地址", "url"],
        ["webhook_get_url", "GET Webhook 地址", "url"],
      ], [make("p", { class: "hint", text: "留空可停用该渠道。测试发送使用已保存的地址。" }), test, results]),
      settingsPanel("/api/settings/calendar", "放送日历", [
        ["enabled", "启用放送日历", "checkbox"],
        ["api_url", "日历 API 或镜像地址", "url"],
      ]),
    ]);
  }
  async function settings() {
    const root = make(
      "div",
      {},
      head("账户与设置", "管理你的账户，以及这个影音空间的成员。"),
    );
    const tabs = [
      ["account", "账户安全"],
      ...(admin()
        ? [
            ["family", "家庭成员"],
            ["sources", "在线影视源"],
            ["epg", "直播与节目单"],
            ["notify", "通知与日历"],
            ["backup", "备份与工具"],
          ]
        : []),
      ["tvbox", "影视仓订阅"],
    ];
    const active = tabs.some(([key]) => key === state.settingsTab)
      ? state.settingsTab
      : "account";
    state.settingsTab = active;
    root.append(
      make(
        "div",
        { class: "settings-tabs" },
        tabs.map(([key, label]) =>
          make("button", {
            type: "button",
            class: `filter-chip${key === active ? " active" : ""}`,
            "aria-pressed": String(key === active),
            text: label,
            on: {
              click: () => {
                state.settingsTab = key;
                render();
              },
            },
          }),
        ),
      ),
    );
    const body = make("div", { class: "settings-panel" });
    if (active === "account") body.append(accountSettings());
    else if (active === "family") body.append(await familySettings());
    else if (active === "sources") body.append(await sourceSettings());
    else if (active === "epg") body.append(...await Promise.all([epgSettings(), liveSubscriptionSettings(), channelAliasSettings()]));
    else if (active === "notify") body.append(...await notificationCalendarSettings());
    else if (active === "backup") body.append(...backupSettings());
    else if (active === "tvbox") body.append(await tvboxSettings());
    root.append(body);
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
        pageName === "favorites" ? await favorites() :
        pageName === "calendar" ? await calendar() :
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
    password.type = "password";
    $("auth-reveal").setAttribute("aria-pressed", "false");
    $("auth-reveal").setAttribute("aria-label", "显示密码");
    $("auth-reveal").title = "显示密码";
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
  $("player-best-source").addEventListener("click", probeLiveSources);
  $("player-favorite")?.addEventListener("click", () => {
    if (state.player?.live) return changeChannelFavorite(state.player.item, $("player-favorite"), () => {
      if (["live", "favorites"].includes(state.page)) render();
    });
  });
  paintSpeedButton();
  $("player-speed").addEventListener("click", cycleSpeed);
  $("player-guide-toggle").addEventListener("click", () => {
    const player = state.player;
    if (!player?.live) return;
    player.guideCollapsed = !player.guideCollapsed;
    $("player-guide").hidden = player.guideCollapsed || !player.guideAvailable;
    updatePlayerControls();
  });
  $("video-player").addEventListener("volumechange", () => {
    const video = $("video-player");
    storeNumber("polyview-volume", video.volume);
    storeNumber("polyview-muted", video.muted ? 1 : 0);
  });
  $("auth-reveal").addEventListener("click", () => {
    const field = $("auth-password");
    const reveal = field.type === "password";
    field.type = reveal ? "text" : "password";
    const button = $("auth-reveal");
    button.setAttribute("aria-label", reveal ? "隐藏密码" : "显示密码");
    button.setAttribute("aria-pressed", String(reveal));
    button.title = button.getAttribute("aria-label");
  });
  syncConnection();
  window.addEventListener("offline", () => syncConnection(false));
  window.addEventListener("online", () => {
    syncConnection(true);
    toast("网络已恢复");
  });
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

  $("video-player").addEventListener("timeupdate", () => {
    const player = state.player;
    if (!player?.metadataReady || player.live || player.finishing) return;
    const video = $("video-player");
    skipIntro(player);
    const config = player.skip;
    if (config?.enabled && config.outro_sec > 0 && !video.paused && !video.ended &&
        Number.isFinite(video.duration) && video.duration > config.intro_sec + config.outro_sec + 30 &&
        video.currentTime >= video.duration - config.outro_sec && video.currentTime < video.duration) {
      return finishEpisode(true);
    }
    return saveProgress();
  });
  $("video-player").addEventListener("pause", () => {
    if (state.player?.metadataReady) return saveProgress(true);
  });
  async function finishEpisode(skippedOutro) {
    const player = state.player;
    if (!player || player.live || player.finishing) return;
    player.finishing = true;
    try {
      if (skippedOutro) {
        const video = $("video-player");
        video.currentTime = video.duration;
        toast("已跳过片尾");
      }
      await saveProgress(true);
      if (player === state.player && player.playlistIndex < player.playlist.length - 1) {
        toast(skippedOutro ? "已跳过片尾，正在播放下一集" : "即将播放下一集");
        playPlaylist(1, true);
      }
    } finally { player.finishing = false; }
  }
  $("video-player").addEventListener("ended", () => {
    if ($("video-player").ended) return finishEpisode(false);
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) saveProgress(true, true);
  });
  const searchInput = $("search-input");
  const searchPopup = $("search-popup");
  const searchablePage = () => ["online", "library", "live", "series"].includes(state.page);
  let popupVersion = 0;
  let suggestionTimer;
  let composingSearch = false;

  function closeSearchPopup() {
    clearTimeout(suggestionTimer);
    popupVersion++;
    if (searchPopup) searchPopup.hidden = true;
  }
  function submitSearch(value, catalog = false) {
    closeSearchPopup();
    clearTimeout(state.searchTimer);
    searchInput.value = value.trim();
    state.query = searchInput.value;
    state.onlinePage = 1;
    $("search-clear").hidden = !state.query;
    if (state.query) api("/api/search/history", { method: "POST", body: { keyword: state.query } }).catch(() => {});
    if ((catalog && state.page !== "online") || !searchablePage()) setNav("online");
    else render();
  }
  async function showSearchHistory() {
    closeSearchPopup();
    if (!searchPopup) return;
    const version = popupVersion;
    const history = await api("/api/search/history").catch(() => []);
    if (version !== popupVersion || !history.length) return;
    const remove = async (keyword) => {
      const suffix = keyword ? `?q=${encodeURIComponent(keyword)}` : "";
      try {
        await api(`/api/search/history${suffix}`, { method: "DELETE" });
        if (version === popupVersion) {
          searchInput.focus?.();
          if (version === popupVersion) await showSearchHistory();
        }
      } catch (error) { toast(error.message, true); }
    };
    searchPopup.replaceChildren(
      make("strong", { text: "最近搜索" }),
      button("清空历史", "", () => remove()),
      make("div", { class: "search-history-chips" }, history.map((keyword) =>
        make("span", { class: "search-history-chip" }, [
          button(keyword, "", () => submitSearch(keyword)),
          button("×", "", () => remove(keyword), { "aria-label": `删除搜索记录 ${keyword}` }),
        ]))),
    );
    searchPopup.hidden = false;
  }
  function updateSearch() {
    closeSearchPopup();
    clearTimeout(state.searchTimer);
    const query = searchInput.value.trim();
    state.query = query;
    state.onlinePage = 1;
    $("search-clear").hidden = !searchInput.value;
    state.searchTimer = setTimeout(() => { if (searchablePage()) render(); }, 250);
    if (!query) { showSearchHistory(); return; }
    if (!searchPopup) return;
    const version = popupVersion;
    suggestionTimer = setTimeout(async () => {
      if (version !== popupVersion) return;
      const suggestions = await api(`/api/catalog/suggest?q=${encodeURIComponent(query)}`).catch(() => []);
      if (version !== popupVersion || !suggestions.length) return;
      searchPopup.replaceChildren(...suggestions.map(({ title, year, kind }) => button(
        [title, year, kind && kindLabel(kind)].filter(Boolean).join(" · "),
        "search-suggestion", () => submitSearch(title, true),
      )));
      searchPopup.hidden = false;
    }, 300);
  }
  function leaveSearch(event) {
    if (event.relatedTarget !== searchInput && !searchPopup?.contains?.(event.relatedTarget)) closeSearchPopup();
  }
  searchInput.addEventListener("focus", showSearchHistory);
  searchInput.addEventListener("blur", leaveSearch);
  searchPopup?.addEventListener("focusout", leaveSearch);
  searchPopup?.addEventListener("mousedown", (event) => event.preventDefault());
  searchPopup?.addEventListener("keydown", (event) => {
    if (event.key === "Escape") { searchInput.focus(); closeSearchPopup(); }
  });
  searchInput.addEventListener("compositionstart", () => {
    composingSearch = true;
    clearTimeout(state.searchTimer);
    closeSearchPopup();
  });
  searchInput.addEventListener("compositionend", () => { composingSearch = false; updateSearch(); });
  searchInput.addEventListener("input", (event) => {
    if (!event.isComposing && !composingSearch) updateSearch();
  });
  searchInput.addEventListener("keydown", (event) => {
    if (event.isComposing || composingSearch || event.keyCode === 229) return;
    if (event.key === "Escape") { event.preventDefault(); closeSearchPopup(); }
    if (event.key === "Enter") { event.preventDefault(); submitSearch(searchInput.value); }
  });
  $("search-clear").addEventListener("click", () => {
    searchInput.value = "";
    searchInput.focus?.();
    updateSearch();
    clearTimeout(state.searchTimer);
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
