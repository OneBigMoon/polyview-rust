import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const application = await readFile(
  new URL("../web/app.js", import.meta.url),
  "utf8",
);

function environment(routes = {}, { canPlayType = () => "" } = {}) {
  const elements = new Map();
  const calls = [];
  const order = [];
  class Element {
    constructor(tag = "div") {
      this.tagName = tag.toUpperCase();
      this.nodeType = 1;
      this.children = [];
      this.attributes = {};
      this.listeners = {};
      this.dataset = {};
      this.classes = new Set();
      this.classList = {
        add: (name) => this.classes.add(name),
        remove: (name) => this.classes.delete(name),
        toggle: (name, on) => {
          if (on) this.classes.add(name);
          else this.classes.delete(name);
          return this.classes.has(name);
        },
      };
      this.style = { cssText: "" };
      this.open = false;
      this.currentTime = 0;
      this.duration = 100;
      this.paused = true;
      this.controls = true;
      this.muted = false;
      this.volume = 1;
      this.playCalls = 0;
    }
    append(...children) {
      this.children.push(...children);
    }
    prepend(child) {
      this.children.unshift(child);
    }
    get firstChild() {
      return this.children[0];
    }
    replaceChildren(...children) {
      this.children = children;
    }
    set textContent(value) {
      this.children = [String(value)];
    }
    get textContent() {
      return this.children
        .map((child) => (typeof child === "string" ? child : child.textContent))
        .join("");
    }
    set value(value) {
      this._value = String(value);
    }
    get value() {
      return (
        this._value ??
        (this.tagName === "SELECT" ? this.children[0]?.value : "") ??
        ""
      );
    }
    setAttribute(name, value) {
      this.attributes[name] = String(value);
      if (name === "id") elements.set(value, this);
      if (name === "value") this.value = value;
    }
    getAttribute(name) {
      return this.attributes[name] ?? null;
    }
    removeAttribute(name) {
      delete this.attributes[name];
    }
    addEventListener(event, callback) {
      (this.listeners[event] ??= []).push(callback);
    }
    showModal() {
      this.open = true;
      order.push(`open:${this.attributes.id}`);
    }
    close() {
      this.open = false;
      order.push(`close:${this.attributes.id}`);
    }
    canPlayType(type) {
      return canPlayType(type);
    }
    load() {}
    play() {
      this.playCalls += 1;
      this.paused = false;
      return Promise.resolve();
    }
    pause() {
      this.paused = true;
    }
    requestFullscreen() {
      document.fullscreenElement = this;
      document.dispatchEvent("fullscreenchange");
      return Promise.resolve();
    }
    dispatchEvent(type, event = {}) {
      (this.listeners?.[type] ?? []).forEach((cb) => cb(event));
    }
  }
  for (const id of [
    "content",
    "auth-form",
    "auth-username",
    "auth-password",
    "auth-title",
    "auth-description",
    "auth-submit",
    "auth-error",
    "auth-screen",
    "app-shell",
    "profile-name",
    "profile-role",
    "profile-avatar",
    "breadcrumb",
    "logout-button",
    "mobile-logout",
    "help-button",
    "help-close",
    "player-close",
    "player-toggle",
    "player-mute",
    "player-fullscreen",
    "player-source",
    "player-guide",
    "player-stage",
    "player-dialog",
    "player-title",
    "player-kind",
    "player-message",
    "video-player",
    "search-input",
    "theme-toggle",
    "nav-toggle",
    "toast",
  ]) {
    const element = new Element(
      id.includes("dialog")
        ? "dialog"
        : id === "video-player"
          ? "video"
          : "div",
    );
    element.setAttribute("id", id);
  }
  elements
    .get("auth-submit")
    .append({ nodeType: 3, textContent: "进入影音空间" });
  elements.get("auth-form").elements = {
    username: elements.get("auth-username"),
    password: elements.get("auth-password"),
  };
  const hlsInstances = [];
  class Hls {
    static Events = { ERROR: "error", MANIFEST_PARSED: "manifest_parsed" };
    static isSupported() {
      return true;
    }
    constructor() {
      hlsInstances.push(this);
    }
    on(event, callback) {
      (this.listeners ??= {})[event] = callback;
    }
    emit(event, data) {
      this.listeners?.[event]?.(event, data);
    }
    loadSource(url) {
      this.source = url;
    }
    attachMedia(video) {
      this.video = video;
    }
    destroy() {
      this.destroyed = true;
    }
  }
  const document = {
    fullscreenElement: null,
    activeElement: null,
    documentElement: new Element("html"),
    body: new Element("body"),
    addEventListener(event, callback) {
      (this.listeners ??= {})[event] ??= [];
      this.listeners[event].push(callback);
    },
    getElementById: (id) => elements.get(id) ?? null,
    createElement: (tag) => new Element(tag),
    createTextNode: (text) => ({ nodeType: 3, textContent: text }),
    querySelector: () => null,
    querySelectorAll: () => [],
    dispatchEvent(type) {
      (this.listeners?.[type] ?? []).forEach((cb) => cb({ type }));
    },
    exitFullscreen() {
      this.fullscreenElement = null;
      this.dispatchEvent("fullscreenchange");
      return Promise.resolve();
    },
  };
  const storage = {};
  const context = vm.createContext({
    document,
    localStorage: {
      getItem: (key) => storage[key] ?? null,
      setItem: (key, value) => {
        storage[key] = String(value);
      },
    },
    window: { Hls, addEventListener() {}, confirm: () => true },
    DOMParser: class {
      parseFromString(text) {
        return { body: { textContent: text.replace(/<[^>]*>/g, "") } };
      }
    },
    fetch: async (url, options = {}) => {
      calls.push({ url, options });
      const body = routes[url] ?? {};
      return { ok: true, status: 200, text: async () => JSON.stringify(body) };
    },
    console,
    URL,
    URLSearchParams,
    Date,
    setTimeout: () => 1,
    clearTimeout() {},
    setInterval: () => 1,
    clearInterval() {},
  });
  const marker = /\bboot\(\);\s*\}\)\(\);\s*$/;
  assert.match(
    application,
    marker,
    "test harness must replace only the startup call",
  );
  vm.runInContext(
    application.replace(
      marker,
      "globalThis.hooks = {state, play, closePlayer, saveProgress, openCatalogDetail, authMode, switchLiveSource, guideView, channelsWithPrograms, online};\n})();",
    ),
    context,
  );
  return {
    document,
    elements,
    calls,
    order,
    hlsInstances,
    documentElement: document.documentElement,
    ...context.hooks,
    elements,
    calls,
    order,
    hlsInstances,
    documentElement: document.documentElement,
  };
}

test("remote HLS chooses the HLS player using the original URL and routes through the proxy", () => {
  const app = environment();
  const url = "https://source.example/video.m3u8?token=A%2BB&other=1";
  app.play({ title: "Episode", remote_url: url });
  assert.equal(app.hlsInstances.length, 1);
  assert.equal(
    app.hlsInstances[0].source,
    `/api/playback?url=${encodeURIComponent(url)}`,
  );
  assert.equal(app.elements.get("player-dialog").open, true);
  app.closePlayer();
  assert.equal(app.hlsInstances[0].destroyed, true);
  assert.equal(app.elements.get("player-dialog").open, false);
});

test("Chrome's tentative HLS support uses hls.js instead of native playback", () => {
  const app = environment({}, { canPlayType: () => "maybe" });
  app.play({ title: "Live", url: "https://source.example/live.m3u8" }, true);
  assert.equal(app.hlsInstances.length, 1);
  assert.equal(
    app.hlsInstances[0].source,
    "/api/playback?url=https%3A%2F%2Fsource.example%2Flive.m3u8",
  );
  assert.equal(app.elements.get("video-player").src, undefined);
});

test("native HLS playback is used only when the browser reports it can play", () => {
  const app = environment({}, { canPlayType: () => "probably" });
  app.play({ title: "Live", url: "https://source.example/live.m3u8" }, true);
  assert.equal(app.hlsInstances.length, 0);
  assert.equal(
    app.elements.get("video-player").src,
    "/api/playback?url=https%3A%2F%2Fsource.example%2Flive.m3u8",
  );
});

test("live channels fail over to the next source when HLS dies", () => {
  const app = environment();
  app.play(
    {
      name: "CCTV-1",
      sources: [
        { url: "https://a.example/1.m3u8" },
        { url: "https://b.example/1.m3u8" },
      ],
    },
    true,
  );
  const button = app.elements.get("player-source");
  assert.equal(button.hidden, false);
  assert.equal(button.textContent, "线路 1/2 · 切换");
  app.hlsInstances[0].emit("error", { fatal: true });
  assert.equal(app.hlsInstances.length, 2);
  assert.equal(
    app.hlsInstances[1].source,
    "/api/playback?url=https%3A%2F%2Fb.example%2F1.m3u8",
  );
  assert.equal(button.textContent, "线路 2/2 · 切换");
});

test("the source button switches live channels manually", () => {
  const app = environment();
  app.play(
    {
      name: "CCTV-1",
      sources: [
        { url: "https://a.example/1.m3u8" },
        { url: "https://b.example/1.m3u8" },
      ],
    },
    true,
  );
  app.switchLiveSource(1);
  assert.equal(app.hlsInstances.length, 2);
  assert.equal(
    app.hlsInstances[1].source,
    "/api/playback?url=https%3A%2F%2Fb.example%2F1.m3u8",
  );
  app.switchLiveSource(1);
  assert.equal(
    app.hlsInstances[2].source,
    "/api/playback?url=https%3A%2F%2Fa.example%2F1.m3u8",
  );
});

test("remote viewing records tracked watched episodes without writing local-library progress", async () => {
  const app = environment();
  app.play({
    title: "Episode",
    remote_url: "https://source.example/episode.mp4",
    tracking_id: 12,
    watched_episodes: 4,
    episode_number: 2,
  });
  const video = app.elements.get("video-player");
  video.currentTime = 95;
  await app.saveProgress(true);
  await app.saveProgress(true);
  assert.equal(app.calls.length, 1);
  assert.equal(app.calls[0].url, "/api/tracking/12");
  assert.equal(JSON.parse(app.calls[0].options.body).watched_episodes, 4);
  assert.equal(app.calls[0].options.headers["X-PolyView-Request"], "1");
});

test("opening an episode closes details before showing the player", async () => {
  const detail = {
    id: "42",
    source_id: 3,
    title: "故事",
    description: "<p>简介</p>",
    groups: [
      {
        name: "HLS",
        direct: true,
        episodes: [{ name: "第1集", url: "https://source.example/1.m3u8" }],
      },
    ],
  };
  const app = environment({
    "/api/catalog/3/detail?id=42": detail,
    "/api/tracking": [],
  });
  await app.openCatalogDetail({ id: "42", source_id: 3, title: "故事" });
  const dialog = app.elements.get("catalog-dialog");
  const find = (node) =>
    node.tagName === "BUTTON" && node.textContent === "在线播放"
      ? node
      : (node.children || [])
          .map((child) => (typeof child === "object" ? find(child) : null))
          .find(Boolean);
  const watch = find(dialog);
  assert.ok(watch, "details expose a play action");
  watch.listeners.click[0]();
  assert.deepEqual(app.order.slice(-2), [
    "close:catalog-dialog",
    "open:player-dialog",
  ]);
  assert.equal(dialog.open, false);
});

test("non-direct parser pages are rejected before opening the player", async () => {
  const detail = {
    id: "43",
    source_id: 3,
    title: "解析页",
    description: "",
    groups: [
      {
        name: "share",
        direct: false,
        episodes: [
          { name: "第1集", url: "https://source.example/share/abc" },
        ],
      },
    ],
  };
  const app = environment({
    "/api/catalog/3/detail?id=43": detail,
    "/api/tracking": [],
  });
  await app.openCatalogDetail({ id: "43", source_id: 3, title: "解析页" });
  const dialog = app.elements.get("catalog-dialog");
  const find = (node) =>
    node.tagName === "BUTTON" && node.textContent === "在线播放"
      ? node
      : (node.children || [])
          .map((child) => (typeof child === "object" ? find(child) : null))
          .find(Boolean);
  const watch = find(dialog);
  assert.ok(watch, "details expose a play action");
  watch.listeners.click[0]();
  assert.equal(dialog.open, true);
  assert.equal(app.order.includes("open:player-dialog"), false);
  assert.match(app.elements.get("toast").textContent, /非直链/);
});

test("imported Unicode usernames and short passwords reach login without relaxing setup", async () => {
  const app = environment({
    "/api/auth/login": { id: 1, username: "旧用户", role: "user" },
    "/api/sources": [],
  });
  const form = app.elements.get("auth-form");
  app.authMode(false);
  assert.equal(form.elements.username.attributes.minlength, "1");
  assert.equal(form.elements.username.attributes.maxlength, "128");
  assert.equal(form.elements.username.attributes.pattern, undefined);
  assert.equal(form.elements.password.attributes.minlength, "1");
  form.elements.username.value = "旧用户";
  form.elements.password.value = "123";
  await form.listeners.submit[0]({ preventDefault() {}, currentTarget: form });
  const login = app.calls.find((call) => call.url === "/api/auth/login");
  assert.ok(login, "legacy login is sent to the server");
  assert.deepEqual(JSON.parse(login.options.body), {
    username: "旧用户",
    password: "123",
  });

  const setup = environment();
  setup.authMode(true);
  const setupForm = setup.elements.get("auth-form");
  assert.equal(
    setupForm.elements.username.attributes.pattern,
    "[A-Za-z0-9_-]{3,32}",
  );
  assert.equal(setupForm.elements.username.attributes.minlength, "3");
  assert.equal(setupForm.elements.password.attributes.minlength, "10");
  setupForm.elements.username.value = "旧用户";
  setupForm.elements.password.value = "123";
  await setupForm.listeners.submit[0]({
    preventDefault() {},
    currentTarget: setupForm,
  });
  assert.equal(setup.calls.length, 0);
  setupForm.elements.username.value = "newadmin";
  await setupForm.listeners.submit[0]({
    preventDefault() {},
    currentTarget: setupForm,
  });
  assert.equal(
    setup.calls.length,
    0,
    "new setup still refuses a short password",
  );
  assert.equal(setup.elements.get("auth-error").hidden, false);
});

test("live HLS channels are relayed through the playback proxy", () => {
  const app = environment();
  const url = "http://iptv.example/live/channel1.m3u8?token=A%2BB";
  app.play({ name: "测试频道", url }, true);
  assert.equal(app.hlsInstances.length, 1);
  assert.equal(
    app.hlsInstances[0].source,
    `/api/playback?url=${encodeURIComponent(url)}`,
  );
  app.closePlayer();
});

test("clicking a live channel starts playback once the manifest is ready", () => {
  const app = environment();
  const video = app.elements.get("video-player");
  app.play({ name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  assert.equal(video.playCalls, 0);
  app.hlsInstances[0].emit("manifest_parsed");
  assert.equal(video.playCalls, 1);
  assert.equal(video.paused, false);
  assert.equal(video.controls, false);
  assert.equal(app.elements.get("player-toggle").hidden, false);
  assert.equal(app.elements.get("player-mute").hidden, false);
  assert.equal(app.elements.get("player-fullscreen").hidden, false);
   app.closePlayer();
  assert.equal(video.controls, true);
  assert.equal(app.elements.get("player-toggle").hidden, true);

});
test("the live player shows current and upcoming programmes on the right", () => {
  const app = environment();
  const now = Math.floor(Date.now() / 1000);
  app.state.epg = {
    updated_at: 0,
    matched_channels: 1,
    programs: [
      { channel_id: 1, start: now - 7200, stop: now, title: "已过期节目" },
      { channel_id: 1, start: now, stop: now + 7200, title: "正在播出" },
      { channel_id: 1, start: now + 7200, stop: now + 14400, title: "下个节目" },
      { channel_id: 2, start: now, stop: now + 7200, title: "别的频道" },
    ],
  };
  app.play({ id: 1, name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  const guide = app.elements.get("player-guide");
  assert.equal(guide.hidden, false);
  assert.match(guide.textContent, /正在播出/);
  assert.match(guide.textContent, /下个节目/);
  assert.doesNotMatch(guide.textContent, /已过期节目/);
  assert.doesNotMatch(guide.textContent, /别的频道/);
  app.closePlayer();
  assert.equal(guide.hidden, true);
});

test("Esc that exits fullscreen does not close the live player", () => {
  const app = environment();
  app.play({ id: 1, name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  const dialog = app.elements.get("player-dialog");
  const cancel = dialog.listeners.cancel[0];
  const event = { preventDefault() { event.defaultPrevented = true; } };
  app.document.fullscreenElement = app.document.documentElement;
  cancel.call(dialog, event);
  assert.equal(event.defaultPrevented, true);
  assert.equal(dialog.open, true);
  app.document.fullscreenElement = null;
  app.document.dispatchEvent("fullscreenchange");
  cancel.call(dialog, event);
  assert.equal(dialog.open, true);
  assert.ok(app.state.player);
});
test("double-clicking the video uses our fullscreen instead of the browser's", () => {
  const app = environment();
  app.play({ id: 1, name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  const video = app.elements.get("video-player");
  const dblclick = video.listeners.dblclick[0];
  let prevented = false;
  dblclick.call(video, { preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
  assert.equal(app.document.fullscreenElement, app.document.documentElement);
  assert.equal(app.elements.get("player-fullscreen").getAttribute("aria-label"), "退出全屏");
});

test("rapid fullscreen clicks request once while a request is pending", async () => {
  const app = environment();
  app.play({ id: 1, name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  const button = app.elements.get("player-fullscreen");
  const click = button.listeners.click[0];
  const stage = app.document.documentElement;
  let requests = 0;
  stage.requestFullscreen = () => {
    requests += 1;
    app.document.fullscreenElement = stage;
    return new Promise(() => {});
  };
  click.call(button);
  click.call(button);
  assert.equal(requests, 1);
});

test("native live HLS starts playback without a separate play click", () => {
  const app = environment({}, { canPlayType: () => "probably" });
  const video = app.elements.get("video-player");
  app.play({ name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  assert.equal(video.playCalls, 1);
  assert.equal(video.paused, false);
  app.closePlayer();
});

test("the guide starts at the current programme and hides past programmes", () => {
  const app = environment();
  app.state.channels = [
    { id: 1, name: "CCTV-1 综合", group_name: "央视", sources: [{ url: "https://a.example/1.m3u8" }] },
  ];
  app.state.epg = {
    updated_at: 0,
    matched_channels: 1,
    programs: [
      { channel_id: 1, start: 0, stop: 3600, title: "已过期节目", description: "" },
      { channel_id: 1, start: 3600, stop: 7200, title: "新闻联播", description: "每日新闻" },
      { channel_id: 1, start: 7200, stop: 10800, title: "后续节目", description: "" },
    ],
  };
  const view = app.guideView(3600, 14400, 5400);
  assert.match(view.textContent, /CCTV-1 综合/);
  assert.match(view.textContent, /新闻联播/);
  assert.match(view.textContent, /后续节目/);
  assert.doesNotMatch(view.textContent, /已过期节目/);
  assert.match(view.textContent, /从当前节目开始/);
  const programs = [];
  const walk = (node) => {
    if (node?.className?.includes("epg-program")) programs.push(node);
    (node?.children || []).forEach((child) => {
      if (typeof child === "object") walk(child);
    });
  };
  walk(view);
  assert.equal(programs.length, 2);
  assert.equal(programs[0].className, "epg-program current");
  assert.equal(programs[0].style.cssText, "left:0px;width:120px");
  assert.match(programs[0].attributes.title, /^新闻联播 /);
  assert.equal(programs[1].style.cssText, "left:120px;width:120px");
  app.state.liveGroup = "央视";
  const filtered = app.guideView(3600, 14400, 5400);
  assert.match(filtered.textContent, /CCTV-1 综合/);
});
test("the live guide keeps only channels that have programme data", () => {
  const app = environment();
  const channels = [
    { id: 1, name: "有节目单频道" },
    { id: 2, name: "无节目单频道" },
  ];
  const programs = [{ channel_id: 1, start: 0, stop: 3600, title: "节目" }];
  assert.deepEqual(
    app.channelsWithPrograms(channels, programs).map((channel) => channel.id),
    [1],
  );
  assert.deepEqual(app.channelsWithPrograms(channels, []), []);
});
test("the theme toggle switches between light and dark", () => {
  const app = environment();
  const toggle = app.elements.get("theme-toggle");
  assert.equal(app.documentElement.dataset.theme, "light");
  toggle.listeners.click[0]();
  assert.equal(app.documentElement.dataset.theme, "dark");
  assert.equal(toggle.getAttribute("aria-label"), "切换浅色模式");
  toggle.listeners.click[0]();
  assert.equal(app.documentElement.dataset.theme, "light");
});

test("discover aggregates sources and offers type and year filters", async () => {
  const app = environment({
    "/api/sources": [
      { id: 1, name: "甲源", enabled: true },
      { id: 2, name: "乙源", enabled: true },
    ],
    "/api/catalog/categories": { categories: ["国产剧", "动作片"] },
    "/api/catalog/discover?page=1": {
      results: [],
      errors: [],
      page_count: 1,
      total: 0,
    },
    "/api/catalog/discover?page=1&category=%E5%9B%BD%E4%BA%A7%E5%89%A7&year=2025": {
      results: [],
      errors: [],
      page_count: 1,
      total: 0,
    },
  });
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const rows = flatten(page).filter(
    (node) => node.className === "discover-filter-row",
  );
  assert.deepEqual(
    rows.map((row) => row.children[0].textContent),
    ["影视源", "类型", "年份"],
  );
  const chips = rows.flatMap((row) => row.children[1].children);
  const labels = chips
    .filter((node) => node.tagName === "BUTTON")
    .map((node) => node.textContent);
  assert.ok(labels.includes("全部影视源"), JSON.stringify(labels));
  assert.ok(labels.includes("甲源"), JSON.stringify(labels));
  assert.ok(labels.includes("全部类型"), JSON.stringify(labels));
  assert.ok(labels.includes("全部年份"), JSON.stringify(labels));
  assert.equal(
    chips.find((chip) => chip.textContent === "全部影视源").attributes[
      "aria-pressed"
    ],
    "true",
  );
  assert.deepEqual(Array.from(app.state.categories), ["国产剧", "动作片"]);
  const first = app.calls.find((call) =>
    call.url.startsWith("/api/catalog/discover"),
  );
  assert.ok(first, JSON.stringify(app.calls.map((call) => call.url)));
  app.calls.length = 0;
  app.state.onlineCategory = "国产剧";
  app.state.onlineYear = "2025";
  await app.online();
  const filtered = app.calls.find((call) =>
    call.url.startsWith("/api/catalog/discover"),
  );
  assert.match(filtered.url, /category=%E5%9B%BD%E4%BA%A7%E5%89%A7/);
  assert.match(filtered.url, /year=2025/);
});

test("discover appends the next page from the load-more sentinel", async () => {
  const app = environment({
    "/api/sources": [{ id: 1, name: "甲源", enabled: true }],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": {
      results: [{ title: "第一部", source_name: "甲源" }],
      errors: [],
      page: 1,
      page_count: 2,
      total: 2,
    },
    "/api/catalog/discover?page=2": {
      results: [{ title: "第二部", source_name: "甲源" }],
      errors: [],
      page: 2,
      page_count: 2,
      total: 2,
    },
  });
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const page = await app.online();
  const summary = flatten(page).find(
    (node) => node.className === "discover-summary",
  );
  const more = flatten(page).find(
    (node) => node.className === "discover-more",
  );
  assert.equal(summary.textContent, "已显示 1 组影片");
  assert.ok(more, "missing load-more sentinel");
  await more.listeners.click[0]();
  assert.equal(
    flatten(page).filter((node) => node.className === "media-card").length,
    2,
  );
  assert.equal(summary.textContent, "已显示 2 组影片");
  assert.equal(
    flatten(page).find((node) => node.className === "discover-end")
      .textContent,
    "已经到底了",
  );
});
test("discover merges the same media from multiple sources into one card", async () => {
  const app = environment({
    "/api/sources": [
      { id: 1, name: "甲源", enabled: true },
      { id: 2, name: "乙源", enabled: true },
    ],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": {
      results: [
        {
          id: "11",
          source_id: 1,
          source_name: "甲源",
          title: "同一部电影",
          year: "2025",
          remark: "正片",
        },
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "同一部电影",
          year: "2025",
          remark: "高清",
        },
        {
          id: "33",
          source_id: 1,
          source_name: "甲源",
          title: "另一部电影",
          year: "2024",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 3,
    },
  });
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 2);
  const merged = cards.find((card) => card._sourceBadge.hidden === false);
  assert.ok(merged, "same media should show an aggregation badge");
  assert.equal(merged._sourceBadge.textContent, "聚合 2 源");
  assert.match(merged._meta.textContent, /2 个影视源 · 点击切换/);
  assert.equal(
    cards.find((card) => card._sourceBadge.hidden === true)._meta.textContent,
    "2024 · 甲源",
  );
});
test("a later discover page adds its source to an existing group", async () => {
  const app = environment({
    "/api/sources": [
      { id: 1, name: "甲源", enabled: true },
      { id: 2, name: "乙源", enabled: true },
    ],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": {
      results: [
        {
          id: "11",
          source_id: 1,
          source_name: "甲源",
          title: "同一部电影",
          year: "2025",
        },
      ],
      errors: [],
      page: 1,
      page_count: 2,
      total: 2,
    },
    "/api/catalog/discover?page=2": {
      results: [
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "同一部电影",
          year: "2025",
        },
      ],
      errors: [],
      page: 2,
      page_count: 2,
      total: 2,
    },
  });
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const page = await app.online();
  const more = flatten(page).find(
    (node) => node.className === "discover-more",
  );
  await more.listeners.click[0]();
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 1);
  assert.equal(cards[0]._sourceBadge.textContent, "聚合 2 源");
  assert.equal(cards[0]._sourceBadge.hidden, false);
  assert.match(cards[0]._meta.textContent, /2 个影视源 · 点击切换/);
  assert.equal(
    flatten(page).find((node) => node.className === "discover-summary")
      .textContent,
    "已显示 1 组影片",
  );
});

test("aggregated details switch between sources", async () => {
  const detail = (sourceId, id) => ({
    id,
    source_id: sourceId,
    title: "同一部电影",
    source_name: sourceId === 1 ? "甲源" : "乙源",
    description: "",
    groups: [],
  });
  const app = environment({
    "/api/catalog/1/detail?id=11": detail(1, "11"),
    "/api/catalog/2/detail?id=22": detail(2, "22"),
    "/api/tracking": [],
  });
  await app.openCatalogDetail({
    id: "11",
    source_id: 1,
    title: "同一部电影",
    sources: [
      { source_id: 1, id: "11", source_name: "甲源" },
      { source_id: 2, id: "22", source_name: "乙源" },
    ],
  });
  const dialog = app.elements.get("catalog-dialog");
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const chips = flatten(dialog).filter(
    (node) => node.className === "filter-chip active",
  );
  assert.deepEqual(
    chips.map((chip) => chip.textContent),
    ["甲源"],
  );
  const second = flatten(dialog).find(
    (node) => node.className === "filter-chip" && node.textContent === "乙源",
  );
  assert.ok(second, "details expose the second source");
  await second.listeners.click[0]();
  assert.ok(
    app.calls.some(
      (call) => call.url === "/api/catalog/2/detail?id=22",
    ),
    "switching sources loads the other source's detail",
  );
  const active = flatten(dialog).filter(
    (node) => node.className === "filter-chip active",
  );
  assert.deepEqual(
    active.map((chip) => chip.textContent),
    ["乙源"],
  );
});
