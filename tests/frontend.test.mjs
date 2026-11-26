import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";
import { registerParserPanelTests } from "./cases/parser-panel.mjs";

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
      this.hidden = false;
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
    insertBefore(child, reference) {
      const index = this.children.indexOf(reference);
      if (index < 0) this.children.push(child);
      else this.children.splice(index, 0, child);
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
    "auth-reveal",
    "connection-banner",
    "player-best-source",
    "player-timeshift",
    "player-skip",
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
    "player-favorite",
    "player-speed",
    "player-guide-toggle",
    "player-guide",
    "player-stage",
    "player-prev",
    "player-next",
    "player-hud",
    "player-hud-title",
    "player-hud-kind",
    "player-hud-close",
    "player-dialog",
    "player-title",
    "player-kind",
    "player-message",
    "video-player",
    "search-input",
    "search-popup",
    "search-clear",
    "series-badge",
    "downloads-badge",
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
  const meta = new Element("meta");
  meta.setAttribute("name", "theme-color");
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
    querySelector: (selector) =>
      selector === 'meta[name="theme-color"]' ? meta : null,
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
    navigator: { onLine: true, clipboard: { writeText: async () => {} } },
    location: { origin: "https://polyview.test" },
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
      "globalThis.hooks = {calendar, state, play, closePlayer, saveProgress, openCatalogDetail, authMode, switchLiveSource, guideView, channelsWithPrograms, live, online, updateNavBadges, library, favorites, settings, trackingRows, downloadList, downloads, spotlightCard, syncConnection, cycleSpeed, playerGuide, statusGroup};\n})();",
    ),
    context,
  );
  return {
    document,
    elements,
    meta,
    calls,
    order,
    hlsInstances,
    documentElement: document.documentElement,
    ...context.hooks,
    context,
    localStorage: context.localStorage,
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

const fallbackPath = "/api/catalog/fallback?source_id=3&vod_id=43&episode=1&flag=share";
const resolvePath = "/api/playback/resolve";

async function parserEpisodeFixture(routes = {}) {
  const item = { id: "43", source_id: 3, title: "故事", cover: "https://img.example/poster" };
  const episodes = [1, 2].map((index) => ({ name: `第${index}集`, url: `https://source.example/share/${index}` }));
  const app = environment({
    "/api/catalog/3/detail?id=43": { ...item, description: "<p>简介</p>", year: "2024", groups: [{ name: "share", direct: false, episodes }] },
    "/api/tracking": [],
    ...routes,
  });
  await app.openCatalogDetail(item);
  return { app, item, episodes, dialog: app.elements.get("catalog-dialog"), video: app.elements.get("video-player") };
}

test("catalog playback resolves the selected share URL and attaches only the protected HLS stream", async () => {
  const grant = "/api/playback/grants/opaque";
  const { app, episodes, dialog, video } = await parserEpisodeFixture({
    [resolvePath]: { url: "https://media.example/title.m3u8", playback_url: grant, is_hls: true },
  });
  pageButton(dialog, "第2集").dispatchEvent("click");
  assert.equal(dialog.open, false);
  assert.equal(app.elements.get("player-dialog").open, true);
  assert.match(app.elements.get("player-message").textContent, /正在解析/);
  assert.equal(video.src, undefined);
  assert.equal(app.hlsInstances.length, 0);
  await new Promise(setImmediate);
  assert.deepEqual(savedMutation(app, resolvePath, "POST"), { url: episodes[1].url, flag: "share" });
  assert.equal(app.hlsInstances[0].source, grant);
  video.onloadedmetadata();
  assert.match(app.elements.get("player-message").textContent, /已解析/);
});

test("single and batch downloads queue original share URLs, parser flags and catalog metadata", async () => {
  const { app, item, episodes, dialog } = await parserEpisodeFixture();
  pageButton(dialog, "倒序").dispatchEvent("click");
  await pageButton(dialog, "下载本集").listeners.click[0]();
  pageButton(dialog, "选择集数").dispatchEvent("click");
  pageButton(dialog, "全选").dispatchEvent("click");
  await pageButton(dialog, "批量加入下载队列（2 集）").listeners.click[0]();
  assert.deepEqual(queuedEpisodeBodies(app), [2, 2, 1].map((i) => ({
    url: episodes[i - 1].url, title: `故事 S01E0${i}`, direct: false, flag: "share", cover: item.cover, year: "2024", description: "简介",
  })));
  assert.ok(!app.calls.some(({ url }) => url === resolvePath));
});

test("native and HLS failures use each backup once and resume the original playlist and progress", async () => {
  const fixture = { source_id: 8, vod_id: "other", title: "故事" };
  const alternate = { ...fixture, flag: "hls", url: "https://backup.example/1.m3u8", direct: true };
  const next = { ...fixture, flag: "mp4", url: "https://last.example/1.mp4", direct: true };
  const grant = "/api/playback/grants/original.mp4";
  const { app, dialog, episodes, video } = await parserEpisodeFixture({
    [resolvePath]: { playback_url: grant, is_hls: false },
    [fallbackPath]: { items: [alternate, alternate, next] },
  });
  pageButton(dialog, "在线播放").dispatchEvent("click");
  await new Promise(setImmediate);
  assert.equal(video.src, grant);
  video.onloadedmetadata();
  video.currentTime = 37.25;
  const originalError = video.onerror;
  originalError();
  originalError();
  await new Promise(setImmediate);
  const hls = app.hlsInstances[0];
  assert.equal(hls.source, `/api/playback?url=${encodeURIComponent(alternate.url)}`);
  video.currentTime = 0;
  video.onloadedmetadata();
  assert.equal(video.currentTime, 37.25);
  hls.emit("manifest_parsed");
  assert.equal(video.playCalls, 1);
  video.currentTime = 50.75;
  hls.emit("error", { fatal: false });
  assert.ok(!hls.destroyed);
  hls.emit("error", { fatal: true });
  hls.emit("error", { fatal: true });
  originalError();
  assert.equal(video.src, `/api/playback?url=${encodeURIComponent(next.url)}`);
  video.currentTime = 0;
  video.onloadedmetadata();
  assert.equal(video.currentTime, 50.75);
  assert.equal(app.state.player.item.remote_url, episodes[0].url);
  assert.equal(app.state.player.playlistIndex, 0);
  const history = app.calls.filter(({ url }) => url === "/api/progress").map(({ options }) => JSON.parse(options.body));
  assert.deepEqual(history.map(({ remote_url, episode_index, position }) => [remote_url, episode_index, position]), [
    [episodes[0].url, 0, 37.25], [episodes[0].url, 0, 50.75],
  ]);
  video.onerror();
  video.onerror();
  assert.match(app.elements.get("player-message").textContent, /没有更多可用片源/);
  assert.equal(app.calls.filter(({ url }) => url === fallbackPath).length, 1);
  app.elements.get("player-next").dispatchEvent("click");
  assert.equal(app.state.player.item.remote_url, episodes[1].url);
});

test("a failed parser tries the next parser-backed source and reports exhaustion", async () => {
  const backup = { source_id: 8, vod_id: "b", title: "故事", flag: "other", url: "https://other.example/share/1", direct: false };
  const { app, dialog, episodes } = await parserEpisodeFixture({ [fallbackPath]: { items: [backup, backup] } });
  pageButton(dialog, "在线播放").dispatchEvent("click");
  await new Promise(setImmediate);
  const attempts = app.calls.filter(({ url }) => url === resolvePath).map(({ options }) => JSON.parse(options.body));
  assert.deepEqual(attempts, [{ url: episodes[0].url, flag: "share" }, { url: backup.url, flag: backup.flag }]);
  assert.equal(app.hlsInstances.length, 0);
  assert.match(app.elements.get("player-message").textContent, /解析失败.*没有更多可用片源/);
});

test("resuming a share episode preserves its saved position when the first parser fails", async () => {
  const record = { remote_url: "https://source.example/share/2", episode_index: 1, episode_name: "第2集", position: 42.5 };
  const backup = { source_id: 8, vod_id: "b", title: "故事", flag: "mp4", url: "https://other.example/2.mp4", direct: true };
  const { app, dialog, video } = await parserEpisodeFixture({
    "/api/progress?source_id=3&media_id=43": [record],
    "/api/catalog/fallback?source_id=3&vod_id=43&episode=2&flag=share": { items: [backup] },
  });
  pageButton(dialog, "倒序").dispatchEvent("click");
  pageButton(dialog, "续播 第2集（00:42）").dispatchEvent("click");
  await new Promise(setImmediate);
  assert.equal(video.src, `/api/playback?url=${encodeURIComponent(backup.url)}`);
  video.onloadedmetadata();
  assert.equal(video.currentTime, record.position);
  assert.equal(video.playCalls, 1);
  assert.equal(app.state.player.playlistIndex, 0);
});

test("late resolution and fallback responses cannot attach media after playback moves on", async () => {
  for (const delayedPath of [resolvePath, fallbackPath]) {
    const delayed = {};
    const pending = new Promise((resolve) => { delayed.release = resolve; });
    const { app, dialog, video } = await parserEpisodeFixture({
      [resolvePath]: { playback_url: "/api/playback/grants/old", is_hls: false },
      [fallbackPath]: { items: [{ source_id: 8, vod_id: "b", flag: "hls", url: "https://old.example/b.m3u8", direct: true }] },
    });
    const fetch = app.context.fetch;
    app.context.fetch = async (url, options) => {
      const result = await fetch(url, options);
      if (url === delayedPath) await pending;
      return result;
    };
    pageButton(dialog, "在线播放").dispatchEvent("click");
    if (delayedPath === fallbackPath) {
      await new Promise(setImmediate);
      video.onerror();
    }
    assert.ok(app.calls.some(({ url }) => url === delayedPath));
    app.elements.get("player-close").dispatchEvent("click");
    app.play({ id: 7, title: "本地电影" });
    delayed.release();
    await new Promise(setImmediate);
    assert.equal(video.src, "/api/library/7/stream");
    assert.equal(app.hlsInstances.length, 0);
    assert.equal(app.elements.get("player-title").textContent, "本地电影");
  }
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

test("playback toggles the background pause state", () => {
  const app = environment();
  const video = app.elements.get("video-player");
  app.play({ name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  app.hlsInstances[0].emit("manifest_parsed");
  video.dispatchEvent("play");
  assert.equal(app.document.body.dataset.playing, "1");
  video.paused = true;
  video.dispatchEvent("pause");
  assert.equal(app.document.body.dataset.playing, "0");
  app.closePlayer();
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
  assert.equal(app.document.fullscreenElement, app.elements.get("player-stage"));
  assert.equal(app.elements.get("player-fullscreen").getAttribute("aria-label"), "退出全屏");
});

test("rapid fullscreen clicks request once while a request is pending", async () => {
  const app = environment();
  app.play({ id: 1, name: "测试频道", url: "http://iptv.example/live.m3u8" }, true);
  const button = app.elements.get("player-fullscreen");
  const click = button.listeners.click[0];
  const stage = app.elements.get("player-stage");
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
    if (/(^|\s)epg-program(\s|$)/.test(node?.className || "")) programs.push(node);
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
  app.state.epgOnlyCurrent = true;
  const currentOnly = app.guideView(3600, 14400, 5400);
  let currentCount = 0;
  const walkCurrent = (node) => {
    if (/(^|\s)epg-program(\s|$)/.test(node?.className || "")) currentCount += 1;
    (node?.children || []).forEach((child) => {
      if (typeof child === "object") walkCurrent(child);
    });
  };
  walkCurrent(currentOnly);
  assert.equal(currentCount, 1);
  assert.match(currentOnly.textContent, /显示全部节目/);
  app.state.epgOnlyCurrent = false;
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
  assert.equal(app.meta.getAttribute("content"), "#f4f5f9");
  toggle.listeners.click[0]();
  assert.equal(app.documentElement.dataset.theme, "dark");
  assert.equal(app.meta.getAttribute("content"), "#080c14");
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
    ["类型", "语言", "地区", "影视源", "分类", "年份"],
  );
  const chips = rows.flatMap((row) => row.children[1].children);
  const labels = chips
    .filter((node) => node.tagName === "BUTTON")
    .map((node) => node.textContent);
  assert.ok(labels.includes("全部"), JSON.stringify(labels));
  assert.ok(labels.includes("电影"), JSON.stringify(labels));
  assert.ok(labels.includes("电视剧"), JSON.stringify(labels));
  assert.ok(labels.includes("全部分类"), JSON.stringify(labels));
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
test("discover filters by language and category options follow the selected type", async () => {
  const app = environment({
    "/api/sources": [{ id: 1, name: "甲源", enabled: true }],
    "/api/catalog/categories": { categories: ["动作片", "国产剧"] },
    "/api/catalog/categories?kind=movie": { categories: ["动作片"] },
    "/api/catalog/discover?page=1": {
      results: [
        { id: "1", title: "国语片", year: "2026", lang: "汉语普通话" },
        { id: "2", title: "英语片", year: "2026", lang: "英语" },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 2,
    },
    "/api/catalog/discover?page=1&kind=movie": {
      results: [
        { id: "1", title: "国语片", year: "2026", lang: "汉语普通话" },
        { id: "2", title: "英语片", year: "2026", lang: "英语" },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 2,
    },
  });
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  let page = await app.online();
  assert.ok(flatten(page).some((node) => node.textContent === "语言"));

  app.state.onlineLang = "英语";
  page = await app.online();
  const titles = flatten(page)
    .filter((node) => node.className === "poster-name")
    .map((node) => node.textContent);
  assert.deepEqual(titles, ["英语片"]);

  app.state.categories = null;
  app.state.onlineKind = "movie";
  app.state.onlineCategory = "国产剧";
  page = await app.online();
  assert.ok(
    app.calls.some(
      (call) => call.url === "/api/catalog/categories?kind=movie",
    ),
  );
  assert.equal(app.state.onlineCategory, "");
  const chips = flatten(page)
    .filter((node) => String(node.className || "").startsWith("filter-chip"))
    .map((node) => node.textContent);
  assert.ok(chips.includes("动作片"));
  assert.ok(!chips.includes("国产剧"));
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
test("discover renders twenty cards at a time", async () => {
  const app = environment({
    "/api/sources": [{ id: 1, name: "甲源", enabled: true }],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": {
      results: Array.from({ length: 25 }, (_, index) => ({
        title: `影片${index + 1}`,
        source_name: "甲源",
        year: "2026",
      })),
      errors: [],
      page: 1,
      page_count: 1,
      total: 25,
    },
  });
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const page = await app.online();
  const count = () =>
    flatten(page).filter((node) => node.className === "media-card").length;
  const summary = flatten(page).find(
    (node) => node.className === "discover-summary",
  );
  const more = flatten(page).find(
    (node) => node.className === "discover-more",
  );
  assert.equal(count(), 20);
  assert.equal(summary.textContent, "已显示 20 组影片");
  assert.ok(more, "missing load-more sentinel");
  await more.listeners.click[0]();
  assert.equal(count(), 25);
  assert.equal(summary.textContent, "已显示 25 组影片");
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
test("discover merges titles with a trailing year and chooses the better source", async () => {
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
          title: "片名.2026",
          year: "2026",
          remark: "正片",
        },
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "片名",
          year: "2026",
          remark: "1080P",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 2,
    },
  });
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 1);
  assert.equal(cards[0]._sourceBadge.textContent, "聚合 2 源");
  assert.equal(
    flatten(cards[0]).find((node) => node.className === "poster-name")
      .textContent,
    "片名",
  );
  assert.match(cards[0]._meta.textContent, /2 个影视源/);
});
test("discover shows the most aggregated groups first", async () => {
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
          title: "单源甲",
          year: "2026",
        },
        {
          id: "12",
          source_id: 1,
          source_name: "甲源",
          title: "单源乙",
          year: "2026",
        },
        {
          id: "21",
          source_id: 1,
          source_name: "甲源",
          title: "多源影片",
          year: "2026",
        },
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "多源影片",
          year: "2026",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 4,
    },
  });
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 3);
  assert.equal(cards[0]._sourceBadge.textContent, "聚合 2 源");
  assert.equal(
    flatten(cards[0]).find((node) => node.className === "poster-name")
      .textContent,
    "多源影片",
  );
});
test("year-only titles still aggregate and non-matching trailing years stay intact", async () => {
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
          title: "1917",
          year: "2019",
          remark: "720P",
        },
        {
          id: "12",
          source_id: 1,
          source_name: "甲源",
          title: "1917",
          year: "2019",
          remark: "4K",
        },
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "1917",
          year: "2019",
          cover: "https://example.test/1917.jpg",
          remark: "1080P",
        },
        {
          id: "31",
          source_id: 1,
          source_name: "甲源",
          title: "银翼杀手2049",
          year: "2017",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 4,
    },
  });
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 2);
  assert.equal(cards[0]._sourceBadge.textContent, "聚合 2 源");
  assert.equal(
    flatten(cards[0]).find((node) => node.className === "poster-name")
      .textContent,
    "1917",
  );
  assert.equal(
    flatten(cards[0]).find((node) => node.tagName === "IMG")?.attributes.src,
    "https://example.test/1917.jpg",
  );
  assert.equal(
    flatten(cards[1]).find((node) => node.className === "poster-name")
      .textContent,
    "银翼杀手2049",
  );
});
test("search ranks exact title matches ahead of multi-source partial matches", async () => {
  const app = environment({
    "/api/sources": [
      { id: 1, name: "甲源", enabled: true },
      { id: 2, name: "乙源", enabled: true },
    ],
    "/api/catalog/search?q=%E7%8B%82%E9%A3%99": {
      results: [
        {
          id: "11",
          source_id: 1,
          source_name: "甲源",
          title: "狂飙之城",
          year: "2026",
        },
        {
          id: "12",
          source_id: 2,
          source_name: "乙源",
          title: "狂飙之城",
          year: "2026",
        },
        {
          id: "21",
          source_id: 1,
          source_name: "甲源",
          title: "狂飙",
          year: "2023",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 3,
    },
  });
  app.state.query = "狂飙";
  const page = await app.online();
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const cards = flatten(page).filter(
    (node) => node.className === "media-card",
  );
  assert.equal(cards.length, 2);
  assert.equal(
    flatten(cards[0]).find((node) => node.className === "poster-name")
      .textContent,
    "狂飙",
  );
});
test("movie and series with the same title and year stay separate", async () => {
  const results = [
    {
      id: "11",
      source_id: 1,
      source_name: "甲源",
      title: "同一部电影",
      year: "2025",
      kind: "movie",
    },
    {
      id: "22",
      source_id: 2,
      source_name: "乙源",
      title: "同一部电影",
      year: "2025",
      kind: "series",
    },
  ];
  const app = environment({
    "/api/sources": [
      { id: 1, name: "甲源", enabled: true },
      { id: 2, name: "乙源", enabled: true },
    ],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": {
      results,
      errors: [],
      page: 1,
      page_count: 1,
      total: 2,
    },
    "/api/catalog/discover?page=1&kind=movie": {
      results: [results[0]],
      errors: [],
      page: 1,
      page_count: 1,
      total: 1,
    },
    "/api/catalog/discover?page=1&kind=series": {
      results: [results[1]],
      errors: [],
      page: 1,
      page_count: 1,
      total: 1,
    },
  });
  const flatten = (node) =>
    [node, ...(node.children || []).flatMap(flatten)].filter(Boolean);
  const kinds = (page) =>
    flatten(page)
      .filter((node) => node.className === "media-card")
      .map((card) => card._kindBadge.textContent);
  assert.deepEqual(kinds(await app.online()), ["电影", "电视剧"]);
  app.state.onlineKind = "movie";
  assert.deepEqual(kinds(await app.online()), ["电影"]);
  app.state.onlineKind = "series";
  assert.deepEqual(kinds(await app.online()), ["电视剧"]);
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
      { source_id: 1, id: "11", source_name: "甲源", remark: "正片" },
      { source_id: 2, id: "22", source_name: "乙源", remark: "HD" },
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
    ["甲源 · 正片"],
  );
  const second = flatten(dialog).find(
    (node) =>
      node.className === "filter-chip" && node.textContent === "乙源 · HD",
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
    ["乙源 · HD"],
  );
});

test("search clear resets the current query", () => {
  const app = environment();
  const input = app.elements.get("search-input");
  const clear = app.elements.get("search-clear");
  clear.hidden = true;
  assert.equal(clear.hidden, true);
  input.value = "流浪地球";
  input.dispatchEvent("input", { target: input });
  assert.equal(app.state.query, "流浪地球");
  assert.equal(clear.hidden, false);
  clear.listeners.click[0]();
  assert.equal(input.value, "");
  assert.equal(app.state.query, "");
  assert.equal(clear.hidden, true);
});

test("navigation badges report tracking and active downloads", () => {
  const app = environment();
  app.state.series = [{ id: 1 }, { id: 2 }];
  app.state.downloads = [
    { id: "a", status: "downloading" },
    { id: "b", status: "completed" },
  ];
  app.updateNavBadges();
  const seriesBadge = app.elements.get("series-badge");
  const downloadBadge = app.elements.get("downloads-badge");
  assert.equal(seriesBadge.hidden, false);
  assert.equal(seriesBadge.textContent, "2");
  assert.equal(downloadBadge.hidden, false);
  assert.equal(downloadBadge.textContent, "1");
});

test("discover spotlights the most aggregated title", async () => {
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
          title: "单源影片",
          year: "2026",
        },
        {
          id: "21",
          source_id: 1,
          source_name: "甲源",
          title: "焦点影片",
          year: "2026",
        },
        {
          id: "22",
          source_id: 2,
          source_name: "乙源",
          title: "焦点影片",
          year: "2026",
        },
      ],
      errors: [],
      page: 1,
      page_count: 1,
      total: 3,
    },
  });
  const page = await app.online();
  const flatten = (node) => [
    node,
    ...(node.children || []).flatMap((child) => flatten(child)),
  ];
  const nodes = flatten(page);
  const spotlight = nodes.find((node) => node.className === "spotlight");
  assert.ok(spotlight);
  assert.equal(
    flatten(spotlight).find((node) => node.tagName === "H2")?.textContent,
    "焦点影片",
  );
  assert.equal(nodes.filter((node) => node.className === "media-card").length, 2);
});

test("vod playback exposes previous and next episodes", () => {
  const app = environment();
  const playlist = [
    { title: "示例剧集 · 第1集", remote_url: "https://cdn.test/1.mp4" },
    { title: "示例剧集 · 第2集", remote_url: "https://cdn.test/2.mp4" },
  ];
  app.play(playlist[0], false, playlist, 0);
  const prev = app.elements.get("player-prev");
  const next = app.elements.get("player-next");
  assert.equal(prev.hidden, true);
  assert.equal(next.hidden, false);
  assert.equal(app.elements.get("player-hud-title").textContent, "示例剧集 · 第1集");
  next.listeners.click[0]();
  assert.equal(app.state.player.playlistIndex, 1);
  assert.equal(app.state.player.item.title, "示例剧集 · 第2集");
  assert.equal(prev.hidden, false);
  assert.equal(next.hidden, true);
});

test("library shows continue watching and groups episodes by series", async () => {
  const app = environment({
    "/api/library": [
      {
        id: "m1",
        title: "未看完电影",
        kind: "movie",
        position: 600,
        duration: 1200,
      },
      {
        id: "e1",
        title: "第一集",
        series: "测试剧",
        kind: "episode",
        position: 0,
        duration: 1200,
      },
      {
        id: "e2",
        title: "第二集",
        series: "测试剧",
        kind: "episode",
        position: 0,
        duration: 1200,
      },
      {
        id: "e3",
        title: "另一集",
        series: "另一部剧",
        kind: "episode",
        position: 0,
        duration: 1200,
      },
    ],
  });
  const flatten = (node) => [
    node,
    ...(node.children || []).flatMap((child) => flatten(child)),
  ];
  app.state.filter = "episode";
  const page = await app.library();
  const nodes = flatten(page);
  assert.equal(nodes.filter((node) => node.className === "continue-section").length, 1);
  assert.equal(nodes.filter((node) => node.className === "library-series").length, 2);
  assert.equal(nodes.filter((node) => node.className === "media-card").length, 4);
});

function pageNodes(root) {
  return [root, ...(root.children || []).flatMap(pageNodes)];
}

function pageClass(root, name) {
  return pageNodes(root).filter((node) => node.className?.split(" ").includes(name));
}

function pageButton(root, label) {
  const button = pageNodes(root).find((node) => node.tagName === "BUTTON" && node.textContent === label);
  assert.ok(button, `Missing button ${label}`);
  return button;
}

function playbackNode(root, matches, description) {
  const node = pageNodes(root).find(matches);
  assert.ok(node, `Missing ${description}`);
  return node;
}

async function openTwoEpisodeDetail() {
  const item = { id: "42", source_id: 3, title: "故事" };
  const episodes = [1, 2].map((number) => ({
    name: `第${number}集`, url: `https://source.example/${number}.m3u8`,
  }));
  const app = environment({
    "/api/catalog/3/detail?id=42": { ...item, groups: [{ name: "HLS", direct: true, episodes }] },
    "/api/tracking": [],
  });
  await app.openCatalogDetail(item);
  return { app, dialog: app.elements.get("catalog-dialog") };
}

function queuedEpisodeBodies(app) {
  return app.calls
    .filter(({ url, options }) => url === "/api/downloads" && options.method === "POST")
    .map(({ options }) => JSON.parse(options.body));
}

test("descending episode order downloads the matching source episode", async () => {
  const { app, dialog } = await openTwoEpisodeDetail();
  pageButton(dialog, "倒序").dispatchEvent("click");
  await pageButton(dialog, "下载本集").listeners.click[0]();
  assert.deepEqual(queuedEpisodeBodies(app), [
    { url: "https://source.example/2.m3u8", title: "故事 S01E02" },
  ]);
});

test("batch selection queues every chosen episode with its source index", async () => {
  const { app, dialog } = await openTwoEpisodeDetail();
  pageButton(dialog, "选择集数").dispatchEvent("click");
  pageButton(dialog, "第2集").dispatchEvent("click");
  pageButton(dialog, "第1集").dispatchEvent("click");
  const submit = pageButton(dialog, "批量加入下载队列（2 集）");
  assert.equal(submit.disabled, false);
  assert.equal(submit.hidden, false);
  await submit.listeners.click[0]();
  assert.deepEqual(queuedEpisodeBodies(app), [
    { url: "https://source.example/1.m3u8", title: "故事 S01E01" },
    { url: "https://source.example/2.m3u8", title: "故事 S01E02" },
  ]);
  assert.match(app.elements.get("toast").textContent, /已加入 2 集下载队列/);
});

test("ended playback advances to the next playlist episode", async () => {
  const app = environment();
  const playlist = [1, 2].map((number) => ({
    title: `第${number}集`, remote_url: `https://source.example/${number}.m3u8`,
  }));
  app.play(playlist[0], false, playlist, 0);
  const video = app.elements.get("video-player");
  video.currentTime = video.duration;
  video.ended = true;
  video.dispatchEvent("ended");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(app.state.player.playlistIndex, 1);
  assert.equal(app.elements.get("player-title").textContent, "第2集");
  const plays = video.playCalls;
  app.hlsInstances.at(-1).emit("manifest_parsed");
  assert.equal(video.playCalls, plays + 1);
});

test("playback speed cycles and persists", () => {
  const app = environment();
  app.play({ id: 1, title: "故事" });
  const speed = app.elements.get("player-speed");
  const video = app.elements.get("video-player");
  assert.equal(video.volume, 1);
  assert.equal(speed.hidden, false);
  assert.equal(speed.textContent, "1.0×");
  assert.equal(video.playbackRate, 1);
  for (const rate of [1.25, 1.5]) {
    speed.dispatchEvent("click");
    assert.equal(speed.textContent, `${rate}×`);
    assert.equal(video.playbackRate, rate);
    assert.equal(app.localStorage.getItem("polyview-speed"), String(rate));
  }
});

test("stored volume and mute are restored when a title opens", () => {
  const app = environment();
  app.localStorage.setItem("polyview-volume", "0.35");
  app.localStorage.setItem("polyview-muted", "1");
  app.play({ id: 1, title: "故事" });
  const video = app.elements.get("video-player");
  assert.equal(video.volume, 0.35);
  assert.equal(video.muted, true);
  video.volume = 0;
  video.muted = false;
  video.dispatchEvent("volumechange");
  assert.equal(app.localStorage.getItem("polyview-volume"), "0");
  assert.equal(app.localStorage.getItem("polyview-muted"), "0");
});

function livePlaybackFixture(channels, scheduled) {
  const now = 1800000000;
  const programs = scheduled.map((id) => ({
    channel_id: id, start: now, stop: now + 3600, title: `节目 ${id}`,
  }));
  const app = environment({
    "/api/channels": channels.map(([id, name, group_name]) => ({
      id, name, group_name, sources: [{ url: `https://source.example/${id}.m3u8` }],
    })),
    [`/api/epg?start=${now - 21600}&end=${now + 86400}`]: { programs },
  });
  app.context.Date = class extends Date { static now() { return now * 1000; } };
  app.state.page = "live";
  return app;
}

test("live channels without an EPG stay hidden until the toggle is used", async () => {
  const app = livePlaybackFixture([[1, "综合一台", "央视"], [2, "本地二台", "地方"]], [1]);
  const view = await app.live();
  assert.match(view.textContent, /综合一台/);
  assert.doesNotMatch(view.textContent, /本地二台/);
  pageButton(view, "显示全部频道（含 1 个无节目单）").dispatchEvent("click");
  assert.equal(app.state.showAllChannels, true);
  await new Promise((resolve) => setImmediate(resolve));
  const updated = app.elements.get("content");
  assert.match(updated.textContent, /综合一台/);
  assert.match(updated.textContent, /本地二台/);
});

test("live group chips show channel counts and pan the timeline", async () => {
  const app = livePlaybackFixture([[1, "一台", "央视"], [2, "地方台", "地方"], [3, "三台", "央视"]], [1, 2, 3]);
  const view = await app.live();
  pageButton(view, "央视 2").dispatchEvent("click");
  const guide = app.elements.get("live-guide");
  const scroller = playbackNode(guide, (node) => node.className === "epg-scroll", "EPG scroller");
  scroller.scrollLeft = 240;
  pageButton(guide, "+2 小时 →").dispatchEvent("click");
  assert.equal(scroller.scrollLeft, 480);
  pageButton(guide, "← -2 小时").dispatchEvent("click");
  assert.equal(scroller.scrollLeft, 240);
  playbackNode(guide, (node) => node.attributes?.["aria-label"] === "播放 一台", "first channel").dispatchEvent("click");
  const toggle = app.elements.get("player-guide-toggle");
  const sidebar = app.elements.get("player-guide");
  assert.equal(toggle.hidden, false);
  assert.match(sidebar.textContent, /节目 1/);
  toggle.dispatchEvent("click");
  assert.equal(sidebar.hidden, true);
  assert.equal(toggle.getAttribute("aria-pressed"), "false");
  app.playerGuide(app.state.player.item);
  assert.equal(sidebar.hidden, true);
  toggle.dispatchEvent("click");
  assert.equal(sidebar.hidden, false);
  assert.equal(toggle.getAttribute("aria-pressed"), "true");
  const next = app.elements.get("player-next");
  const prev = app.elements.get("player-prev");
  assert.equal(next.getAttribute("aria-label"), "下一台");
  assert.equal(prev.getAttribute("aria-label"), "上一台");
  assert.equal(next.hidden, false);
  next.dispatchEvent("click");
  assert.equal(app.state.player.live, true);
  assert.equal(app.elements.get("player-title").textContent, "三台");
  assert.match(sidebar.textContent, /节目 3/);
  assert.equal(next.hidden, true);
  assert.equal(prev.hidden, false);
  prev.dispatchEvent("click");
  assert.equal(app.elements.get("player-title").textContent, "一台");
});

function protectedGuideFixture() {
  const now = 1800000000;
  const first = { id: 40, hls: false, catchup: false, label: "主线", latency_ms: 18, last_ok_at: now, fail_count: 0 };
  const replay = { id: 41, hls: false, catchup_hls: true, catchup: true, label: "备线", latency_ms: null, fail_count: 3 };
  const channels = [
    { id: 1, name: "新闻", group_name: "央视", sources: [first, replay, { ...replay, id: 42, label: "第三线" }] },
    { id: 2, name: "地方台", group_name: "本地", sources: [{ ...first, id: 51, last_ok_at: null, latency_ms: null }] },
  ];
  const liveUrl = `/api/epg?start=${now - 21600}&end=${now + 86400}`;
  const routes = { "/api/channels": channels, [liveUrl]: { programs: [{ channel_id: 1, start: now, stop: now + 3600, title: "直播新闻" }] } };
  const app = environment(routes);
  app.context.Date = class extends Date { static now() { return now * 1000; } };
  app.state.page = "live";
  app.state.user = { role: "member" };
  return { app, now, channels, routes };
}

test("members navigate the seven-day archive and replay only capable lines through their protected streams", async () => {
  const { app, now, routes } = protectedGuideFixture();
  const page = await app.live();
  assert.doesNotMatch(page.textContent, /直播线路管理|测活\/测速|清理失效线路|导入频道列表|各线路状态/);
  pageClass(page, "epg-program")[0].dispatchEvent("click");
  assert.equal(app.elements.get("video-player").src, "/api/channels/lines/40/stream");
  pageButton(app.elements.get("player-timeshift"), "-5分").dispatchEvent("click");
  assert.equal(app.elements.get("video-player").src, "/api/channels/lines/40/stream?start=-300");
  app.closePlayer();
  const date = pageNodes(pageClass(page, "guide-date")[0]).find((node) => node.tagName === "SELECT");
  const oldest = date.children.at(-1);
  const cutoff = now - 7 * 86400;
  const nextMidnight = new Date(Number(oldest.value) * 1000);
  nextMidnight.setDate(nextMidnight.getDate() + 1);
  const start = cutoff + 60, stop = start + 300;
  const historyUrl = `/api/epg?start=${cutoff}&end=${nextMidnight.getTime() / 1000}`;
  routes[historyUrl] = { programs: [
    { channel_id: 1, start: cutoff - 600, stop: cutoff + 60, title: "回看窗口以外" },
    { channel_id: 1, start, stop, title: "早间新闻" },
    { channel_id: 2, start, stop, title: "无回看模板" },
  ] };
  date.value = oldest.value;
  await date.listeners.change[0]();
  const content = app.elements.get("content");
  assert.ok(app.calls.some(({ url }) => url === historyUrl));
  assert.doesNotMatch(content.textContent, /回看窗口以外|无回看模板/);
  const programs = pageClass(content, "epg-program");
  assert.equal(programs.length, 1);
  assert.match(programs[0].textContent, /回看 · 早间新闻/);
  programs[0].dispatchEvent("click");
  assert.equal(app.elements.get("player-kind").textContent, "节目回看");
  assert.equal(app.elements.get("player-title").textContent, "新闻 · 早间新闻");
  assert.equal(app.hlsInstances.at(-1).source, `/api/channels/lines/41/stream?start=${start}&end=${stop}`);
  app.hlsInstances.at(-1).emit("error", { fatal: true });
  assert.equal(app.hlsInstances.at(-1).source, `/api/channels/lines/42/stream?start=${start}&end=${stop}`);
  pageButton(app.elements.get("player-timeshift"), "返回直播").dispatchEvent("click");
  assert.equal(app.elements.get("video-player").src, "/api/channels/lines/40/stream");
  app.closePlayer();
  await pageButton(content, "回到现在").listeners.click[0]();
  assert.match(content.textContent, /直播新闻/);
  assert.equal(app.state.guideDay, "live");
});

test("administrator inspects health, probes all lines and confirms group cleanup", async () => {
  const { app, channels, routes } = protectedGuideFixture();
  app.state.user.role = "admin";
  const probePath = "/api/channels/probe", cleanupPath = "/api/channels/cleanup";
  routes[probePath] = { checked: 4, healthy: 2, failed: 2 };
  routes[cleanupPath] = { removed: 2 };
  const fetch = app.context.fetch;
  app.context.fetch = (url, options) => {
    if (url === cleanupPath) channels[0].sources.splice(1);
    return fetch(url, options);
  };
  function health(page) {
    const panel = pageClass(page, "channel-health")[0];
    const details = pageNodes(panel).find((node) => node.tagName === "DETAILS");
    details.open = true;
    details.dispatchEvent("toggle");
    return panel;
  }
  let panel = health(await app.live());
  assert.match(panel.textContent, /可用 · 18 ms/);
  assert.match(panel.textContent, /连续失败 3 次/);
  assert.match(panel.textContent, /未测活/);
  await pageButton(panel, "一键测活/测速").listeners.click[0]();
  assert.match(app.elements.get("toast").textContent, /已检测 4 条线路，2 条可用，2 条失败/);
  panel = health(app.elements.get("content"));
  const scope = pageNodes(panel).find((node) => node.tagName === "SELECT");
  scope.value = "央视";
  scope.dispatchEvent("change");
  assert.doesNotMatch(pageClass(panel, "channel-health-list")[0].textContent, /地方台/);
  app.context.window.confirm = () => false;
  await pageButton(panel, "清理失效线路").listeners.click[0]();
  assert.equal(app.calls.filter(({ url }) => url === cleanupPath).length, 0);
  app.context.window.confirm = (message) => message.includes("央视") && message.includes("3 次");
  await pageButton(panel, "清理失效线路").listeners.click[0]();
  assert.match(app.elements.get("toast").textContent, /已清理 2 条失效线路/);
  assert.doesNotMatch(health(app.elements.get("content")).textContent, /连续失败/);
  const mutations = app.calls.filter(({ options }) => options.method === "POST");
  assert.deepEqual(mutations.map(({ url, options }) => [url, JSON.parse(options.body)]), [[probePath, {}], [cleanupPath, { group: "央视" }]]);
  assert.ok(mutations.every(({ options }) => options.headers["X-PolyView-Request"] === "1"));
});

test("guide errors retain date navigation and failed operator probes can be retried", async () => {
  const { app, routes } = protectedGuideFixture();
  app.state.user.role = "admin";
  const original = app.context.fetch;
  const fail = { ok: false, status: 502, text: async () => JSON.stringify({ error: "连接失败" }) };
  let failProbe = true;
  app.context.fetch = (url, options) => url.startsWith("/api/epg?") || (url === "/api/channels/probe" && failProbe) ? fail : original(url, options);
  routes["/api/channels/probe"] = { checked: 0, healthy: 0, failed: 0 };
  const page = await app.live();
  assert.match(page.textContent, /节目单加载失败：连接失败/);
  assert.equal(pageClass(page, "guide-date").length, 1);
  const panel = pageClass(page, "channel-health")[0];
  const scope = pageNodes(panel).find((node) => node.tagName === "SELECT");
  scope.value = "本地";
  scope.dispatchEvent("change");
  const probe = pageButton(panel, "一键测活/测速");
  await probe.listeners.click[0]();
  assert.equal(probe.disabled, false);
  assert.match(panel.textContent, /连接失败/);
  failProbe = false;
  await probe.listeners.click[0]();
  const retry = app.calls.find(({ url }) => url === "/api/channels/probe");
  assert.deepEqual(JSON.parse(retry.options.body), { group: "本地" });
});

const tvboxPaths = { config_path: "/feeds/config", catalog_path: "/feeds/catalog", live_path: "/feeds/live" };

test("settings tabs render one panel at a time", async () => {
  const app = environment({ "/api/users": [], "/api/tvbox": tvboxPaths });
  app.state.page = "settings";
  app.state.user = { id: 1, username: "root", role: "admin" };
  const page = await app.settings();
  assert.deepEqual(pageClass(page, "filter-chip").map((chip) => chip.textContent),
    ["账户安全", "家庭成员", "在线影视源", "解析接口", "直播与节目单", "通知与日历", "备份与工具", "影视仓订阅"]);
  const panelTitles = (view) => pageNodes(pageClass(view, "settings-panel")[0])
    .filter((node) => node.tagName === "H2").map((node) => node.textContent);
  assert.deepEqual(panelTitles(page), ["修改密码"]);
  assert.equal(app.calls.length, 0);
  pageButton(page, "家庭成员").dispatchEvent("click");
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(panelTitles(app.elements.get("content")), ["家庭成员"]);
  assert.ok(app.calls.some((call) => call.url === "/api/users"));
});

test("tvbox copy buttons confirm success briefly", async () => {
  const app = environment({ "/api/tvbox": tvboxPaths });
  app.state.settingsTab = "tvbox";
  let copied;
  app.context.navigator.clipboard.writeText = async (value) => { copied = value; };
  const page = await app.settings();
  const copy = pageButton(page, "复制");
  const timers = [];
  app.context.setTimeout = (callback, delay) => timers.push({ callback, delay });
  await copy.listeners.click[0]();
  assert.equal(copied, "https://polyview.test/feeds/config");
  assert.equal(copy.textContent, "✓ 已复制");
  timers.find(({ delay }) => delay === 1500).callback();
  assert.equal(copy.textContent, "复制");
});

test("tracking rows show covers and stepper buttons adjust the watched count", async () => {
  const app = environment({ "/api/sources": [] });
  const cover = "https://images.test/show.jpg";
  const list = app.trackingRows([{ id: 7, title: "故事", cover, total_episodes: 4, watched_episodes: 1 }]);
  const image = pageNodes(pageClass(list, "series-cover")[0]).find((node) => node.tagName === "IMG");
  assert.equal(image.attributes.src, cover);
  const form = pageClass(list, "tracking-progress")[0];
  const input = pageNodes(form).find((node) => node.attributes?.name === "watched");
  const step = pageNodes(form).find((node) => node.attributes?.["aria-label"] === "故事 增加一集");
  step.dispatchEvent("click");
  assert.equal(input.value, "2");
  assert.equal(app.calls.length, 0);
  await form.listeners.submit[0]({ preventDefault() {} });
  const saves = app.calls.filter((call) => call.url === "/api/tracking/7" && call.options.method === "PUT");
  assert.equal(saves.length, 1);
  assert.deepEqual(JSON.parse(saves[0].options.body), { watched_episodes: 2 });
});

test("download filters narrow the queue and count each status", async () => {
  const jobs = ["running", "failed", "completed"].map((status, id) => ({ id, status, title: `任务 ${id}` }));
  const app = environment({ "/api/downloads": jobs });
  app.state.downloads = jobs;
  app.state.downloadFilter = "running";
  const rows = pageClass(app.downloadList(), "download-row");
  assert.equal(rows.length, 1);
  assert.match(rows[0].textContent, /任务 0/);
  const page = await app.downloads();
  const chips = pageClass(page, "filter-chip");
  assert.deepEqual(chips.map((chip) => chip.textContent), ["全部 3", "下载中 1", "排队中 0", "已完成 1", "异常 1"]);
  assert.equal(chips[1].attributes["aria-pressed"], "true");
});

test("all-view folds one series into a stack and expands on demand", async () => {
  const app = environment({ "/api/library": [
    { id: "e1", title: "第一集", kind: "episode", series: "故事", series_id: 7 },
    { id: "e2", title: "第二集", kind: "episode", series: "故事", series_id: 7 },
    { id: "m1", title: "电影", kind: "movie" },
  ] });
  app.state.page = "library";
  app.state.filter = "all";
  const page = await app.library();
  const stacks = pageClass(page, "library-stack");
  assert.equal(stacks.length, 1);
  assert.equal(pageClass(page, "media-card").length, 2);
  pageClass(stacks[0], "poster-button")[0].dispatchEvent("click");
  assert.ok(app.state.expandedSeries.has("id:7"));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(pageClass(app.elements.get("content"), "media-card").length, 4);
});

function pageDiscoveryRoutes(results) {
  return {
    "/api/sources": [{ id: 1, name: "甲源", enabled: true }, { id: 2, name: "乙源", enabled: true }],
    "/api/catalog/categories": { categories: [] },
    "/api/catalog/discover?page=1": { results, errors: [], page: 1, page_count: 1, total: results.length },
  };
}

test("spotlight dots switch the featured title and its detail link", async () => {
  const results = [1, 2, 3].map((id) => ({
    id: String(id), title: `影片 ${id}`, source_id: 1, source_name: "甲源", year: String(2027 - id), cover: `https://images.test/${id}.jpg`,
  }));
  const detailUrl = "/api/catalog/1/detail?id=3";
  const app = environment({ ...pageDiscoveryRoutes(results), "/api/tracking": [], [detailUrl]: { ...results[2], groups: [] } });
  const spotlight = pageClass(await app.online(), "spotlight")[0];
  const dots = pageClass(spotlight, "spotlight-dot");
  assert.equal(dots.length, 3);
  dots[2].dispatchEvent("click");
  assert.equal(pageNodes(spotlight).find((node) => node.tagName === "H2").textContent, "影片 3");
  assert.deepEqual(pageClass(spotlight, "spotlight-dot").map((dot) => dot.attributes["aria-pressed"]), ["false", "false", "true"]);
  await pageButton(spotlight, "查看片源").listeners.click[0]();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(app.calls.filter((call) => call.url.includes("/detail?")).map((call) => call.url), [detailUrl]);
});

test("Enter in the top search jumps to discovery from settings", () => {
  const app = environment({ "/api/sources": [] });
  app.state.page = "settings";
  const search = app.elements.get("search-input");
  search.value = "流浪地球";
  search.dispatchEvent("keydown", { key: "Enter", preventDefault() {} });
  assert.equal(app.state.page, "online");
  assert.equal(app.state.query, "流浪地球");
});

test("the password reveal toggles the input type", () => {
  const app = environment();
  const input = app.elements.get("auth-password");
  const reveal = app.elements.get("auth-reveal");
  input.type = "password";
  reveal.dispatchEvent("click");
  assert.equal(input.type, "text");
  assert.equal(reveal.attributes["aria-pressed"], "true");
  reveal.dispatchEvent("click");
  assert.equal(input.type, "password");
  assert.equal(reveal.attributes["aria-pressed"], "false");
});

test("the offline banner reflects connectivity", () => {
  const app = environment();
  const banner = app.elements.get("connection-banner");
  app.syncConnection(false);
  assert.equal(banner.hidden, false);
  app.syncConnection(true);
  assert.equal(banner.hidden, true);
});

test("existing discover spotlight without covers keeps a single feature", async () => {
  const app = environment(pageDiscoveryRoutes([
    { id: "a", source_id: 1, source_name: "甲源", title: "单源片" },
    { id: "b", source_id: 1, source_name: "甲源", title: "聚合片" },
    { id: "c", source_id: 2, source_name: "乙源", title: "聚合片" },
  ]));
  const page = await app.online();
  const spots = pageClass(page, "spotlight");
  assert.equal(spots.length, 1);
  assert.equal(pageNodes(spots[0]).find((node) => node.tagName === "H2").textContent, "聚合片");
  assert.equal(pageClass(page, "spotlight-dot").length, 0);
});

test("discovery filters source regions before grouping", async () => {
  const areas = ["内地", "中國香港", "臺灣", "USA", "Japan", "韓國", "法国", "泰国"];
  const app = environment(pageDiscoveryRoutes(areas.map((area, id) => ({ id: String(id), source_id: 1, title: area, area }))));
  let page = await app.online();
  const expected = { 中国大陆: ["内地"], 港台: ["中國香港", "臺灣"], 美国: ["USA"], 日本: ["Japan"], 韩国: ["韓國"], 欧美: ["法国"], 其他: ["泰国"] };
  pageButton(page, "全部地区");
  for (const [region, titles] of Object.entries(expected)) {
    const areaRow = pageClass(page, "discover-filter-row").find((row) => row.children[0].textContent === "地区");
    pageButton(areaRow, region).dispatchEvent("click");
    assert.equal(app.state.onlineArea, region);
    page = await app.online();
    assert.deepEqual(pageClass(page, "poster-name").map((node) => node.textContent).sort(), titles.slice().sort());
  }
});

test("search history displays, deletes individual entries, and clears all", async () => {
  const history = ["流浪地球", "海岸"];
  const app = environment({ "/api/search/history": history });
  const input = app.elements.get("search-input");
  const popup = app.elements.get("search-popup");
  await input.listeners.focus[0]();
  assert.equal(popup.hidden, false);
  pageButton(popup, "流浪地球");
  const remove = pageNodes(popup).find((node) => node.attributes?.["aria-label"] === "删除搜索记录 流浪地球");
  history.shift();
  await remove.listeners.click[0]();
  assert.equal(pageClass(popup, "search-history-chip").length, 1);
  history.length = 0;
  await pageButton(popup, "清空历史").listeners.click[0]();
  assert.equal(popup.hidden, true);
  const deletions = app.calls.filter(({ options }) => options.method === "DELETE");
  assert.deepEqual(deletions.map(({ url }) => url), [`/api/search/history?q=${encodeURIComponent("流浪地球")}`, "/api/search/history"]);
  assert.ok(deletions.every(({ options }) => options.headers["X-PolyView-Request"] === "1"));
});

test("search suggestions debounce, ignore IME and stale results, and submit the chosen title", async () => {
  const app = environment({ "/api/sources": [], "/api/catalog/suggest?q=a": [{ title: "旧标题" }], "/api/catalog/suggest?q=ab": [{ title: "新标题", year: "2026", kind: "movie" }] });
  const input = app.elements.get("search-input");
  const popup = app.elements.get("search-popup");
  const timers = [];
  app.context.setTimeout = (callback, delay) => timers.push({ callback, delay });
  app.state.page = "settings";
  input.dispatchEvent("compositionstart");
  input.value = "a";
  input.dispatchEvent("input", { isComposing: true });
  input.dispatchEvent("keydown", { key: "Enter", isComposing: true });
  assert.equal(app.state.page, "settings");
  assert.equal(timers.length, 0);
  input.dispatchEvent("compositionend");
  assert.equal(app.calls.length, 0);
  const stale = timers.find(({ delay }) => delay === 300).callback();
  input.value = "ab";
  input.dispatchEvent("input");
  await stale;
  assert.equal(popup.hidden, true);
  await timers.at(-1).callback();
  assert.equal(popup.hidden, false);
  input.dispatchEvent("keydown", { key: "Escape", preventDefault() {} });
  assert.equal(popup.hidden, true);
  input.dispatchEvent("input");
  await timers.at(-1).callback();
  input.dispatchEvent("blur");
  assert.equal(popup.hidden, true);
  input.dispatchEvent("input");
  await timers.at(-1).callback();
  pageClass(popup, "search-suggestion")[0].dispatchEvent("click");
  assert.equal(app.state.page, "online");
  assert.equal(app.state.query, "新标题");
  assert.equal(popup.hidden, true);
  const post = app.calls.find(({ url, options }) => url === "/api/search/history" && options.method === "POST");
  assert.deepEqual(JSON.parse(post.options.body), { keyword: "新标题" });
  assert.equal(post.options.headers["X-PolyView-Request"], "1");
});

function savedMediaFixture() {
  const item = { id: "a/b", source_id: 3, title: "故事", cover: "https://example.test/poster.jpg", kind: "series" };
  const episodes = [1, 2].map((i) => ({ name: `第${i}集`, url: `https://example.test/${i}.mp4` }));
  const record = { entry_kind: "online", source_id: 3, media_id: item.id, title: item.title, cover: item.cover,
    remote_url: episodes[1].url, episode_index: 1, episode_name: episodes[1].name, position: 62.75, duration: 100 };
  const local = { id: 9, title: "本地片", favorite: true, position: 10, duration: 100 };
  const channel = { id: 4, name: "地方台", url: "https://example.test/live.m3u8", favorite: true };
  const routes = {
    "/api/catalog/3/detail?id=a%2Fb": { ...item, groups: [{ name: "直链", direct: true, episodes }] },
    "/api/tracking": [],
    "/api/sources": [],
    "/api/favorites": { catalog: [{ ...item, media_id: item.id }], local: [local], channels: [channel] },
    "/api/progress?source_id=3&media_id=a%2Fb": [record],
    "/api/progress": [record, { ...local, entry_kind: "local", local_id: local.id }],
  };
  return { app: environment(routes), item, record, local, channel, routes };
}

function savedAction(root, label) {
  return playbackNode(root, (node) => node.attributes?.["aria-label"] === label, label);
}

function savedMutation(app, url, method) {
  const call = app.calls.find((call) => call.url === url && call.options.method === method);
  assert.ok(call, `${method} ${url}`);
  assert.equal(call.options.headers["X-PolyView-Request"], "1");
  return call.options.body && JSON.parse(call.options.body);
}

test("detail resumes the saved episode after sorting and persists exact online seconds", async () => {
  const { app, item, record } = savedMediaFixture();
  await app.openCatalogDetail(item);
  const dialog = app.elements.get("catalog-dialog");
  pageButton(dialog, "倒序").dispatchEvent("click");
  pageButton(dialog, "续播 第2集（01:02）").dispatchEvent("click");
  assert.equal(app.state.player.playlistIndex, 0);
  assert.equal(app.state.player.item.remote_url, record.remote_url);
  const video = app.elements.get("video-player");
  video.onloadedmetadata();
  assert.equal(video.currentTime, record.position);
  assert.equal(video.playCalls, 1);
  video.currentTime = 70.125;
  await app.saveProgress(true);
  const { entry_kind, ...expected } = record;
  assert.deepEqual(savedMutation(app, "/api/progress", "PUT"), { ...expected, position: 70.125 });
  assert.ok(app.calls.every(({ url }) => !url.startsWith("/api/library/")));
});

test("discovery resumes and removes online and local entries and skips history during search", async () => {
  const { app } = savedMediaFixture();
  const page = await app.online();
  const strip = pageClass(page, "continue-section")[0];
  assert.ok(strip);
  assert.match(strip.textContent, /继续观看/);
  assert.match(strip.textContent, /第2集/);
  assert.equal(pageNodes(strip).filter((node) => node.tagName === "PROGRESS").length, 2);
  savedAction(strip, "续播 故事").dispatchEvent("click");
  assert.equal(app.state.player.item.episode_index, 1);
  assert.equal(app.state.player.autoplay, true);
  await savedAction(strip, "移除 故事").listeners.click[0]();
  savedMutation(app, "/api/progress/online/3/a%2Fb", "DELETE");
  assert.doesNotMatch(strip.textContent, /故事/);
  savedAction(strip, "续播 本地片").dispatchEvent("click");
  assert.equal(app.state.player.item.id, 9);
  assert.equal(app.elements.get("video-player").src, "/api/library/9/stream");
  await savedAction(strip, "移除 本地片").listeners.click[0]();
  savedMutation(app, "/api/progress/local/9", "DELETE");
  assert.equal(strip.hidden, true);
  app.state.query = "故事";
  app.calls.length = 0;
  assert.equal(pageClass(await app.online(), "continue-section").length, 0);
  assert.ok(app.calls.every(({ url }) => url !== "/api/progress"));
});

test("catalog detail reads and toggles the current source's favorite", async () => {
  const { app, item } = savedMediaFixture();
  await app.openCatalogDetail(item);
  const dialog = app.elements.get("catalog-dialog");
  const star = pageButton(dialog, "★ 已收藏");
  await star.listeners.click[0]();
  savedMutation(app, "/api/favorites/3/a%2Fb", "DELETE");
  assert.equal(star.textContent, "☆ 收藏");
  await star.listeners.click[0]();
  const body = savedMutation(app, "/api/favorites", "POST");
  assert.equal(body.media_id, item.id);
  assert.equal(body.title, item.title);
  assert.equal(body.source_id, item.source_id);
  assert.equal(star.getAttribute("aria-pressed"), "true");
});

test("favorites offers one-click playback and removal in all three sections", async () => {
  const { app, record } = savedMediaFixture();
  const page = await app.favorites();
  assert.deepEqual(pageNodes(page).filter((node) => node.tagName === "H2").map((node) => node.textContent), ["在线影视收藏", "本地媒体收藏", "收藏直播频道"]);
  await savedAction(page, "播放 故事").listeners.click[0]();
  assert.equal(app.state.player.item.remote_url, record.remote_url);
  assert.equal(app.state.player.autoplay, true);
  savedAction(page, "播放 本地片").dispatchEvent("click");
  assert.equal(app.state.player.item.id, 9);
  savedAction(page, "播放 地方台").dispatchEvent("click");
  assert.equal(app.state.player.live, true);
  assert.equal(app.state.player.item.id, 4);
  for (const title of ["故事", "本地片", "地方台"]) await savedAction(page, `取消收藏 ${title}`).listeners.click[0]();
  savedMutation(app, "/api/favorites/3/a%2Fb", "DELETE");
  assert.deepEqual(savedMutation(app, "/api/library/9/favorite", "PUT"), { favorite: false });
  assert.deepEqual(savedMutation(app, "/api/channels/4/favorite", "PUT"), { favorite: false });
  assert.equal(pageClass(page, "saved-card").length, 0);
});

test("guide stars filter favorite channels and the player star removes a favorite", async () => {
  const app = livePlaybackFixture([[1, "一台", "央视"], [2, "二台", "地方"]], [1, 2]);
  const page = await app.live();
  await savedAction(page, "收藏 一台").listeners.click[0]();
  assert.deepEqual(savedMutation(app, "/api/channels/1/favorite", "PUT"), { favorite: true });
  pageButton(page, "★ 收藏").dispatchEvent("click");
  assert.match(page.textContent, /一台/);
  assert.doesNotMatch(page.textContent, /二台/);
  savedAction(page, "播放 一台").dispatchEvent("click");
  const star = app.elements.get("player-favorite");
  assert.equal(star.hidden, false);
  assert.equal(star.getAttribute("aria-pressed"), "true");
  app.calls.length = 0;
  await star.listeners.click[0]();
  assert.deepEqual(savedMutation(app, "/api/channels/1/favorite", "PUT"), { favorite: false });
  assert.equal(star.getAttribute("aria-pressed"), "false");
});

async function adminSourcePage() {
  const sources = [
    { id: 4, name: "甲", url: "https://one.example/", enabled: true, adult: false, last_probe_ok: true, last_latency_ms: 499, priority: 1 },
    { id: 7, name: "乙", url: "https://two.example/", enabled: true, adult: false, last_probe_ok: true, last_latency_ms: 500, priority: 2 },
    { id: 9, name: "丙", url: "https://three.example/", enabled: false, last_probe_ok: false, cooldown_until: Math.floor(Date.now() / 1000) + 120 },
  ];
  const app = environment({ "/api/sources": sources, "/api/catalog/settings": {}, "/api/settings/play": {
    ad_block: false, play_referer: "https://player.example/", play_ua: "old", image_referer: "https://images.example/",
  }, "/api/sources/probe": [{ source_id: 4, ok: true }], "/api/sources/batch": { imported: 1, skipped: 0 } });
  app.state.page = "settings";
  app.state.settingsTab = "sources";
  app.state.user = { id: 1, role: "admin" };
  return { app, page: await app.settings(), sources };
}

test("admin probes individual or all sources and sees latency and cooldown", async () => {
  const { app, page, sources } = await adminSourcePage();
  assert.deepEqual(pageClass(page, "source-health").map((badge) => badge.textContent), ["499 ms", "500 ms", "测速失败 · 冷却中"]);
  assert.equal(pageClass(page, "fast").length, 1);
  assert.equal(pageClass(page, "medium").length, 1);
  sources[0].last_latency_ms = 1710;
  await pageButton(page, "测速").listeners.click[0]();
  assert.equal(pageClass(page, "slow")[0].textContent, "1710 ms");
  await pageButton(page, "全部测速").listeners.click[0]();
  assert.deepEqual(app.calls.filter((call) => call.url === "/api/sources/probe").map((call) => JSON.parse(call.options.body)), [{ source_id: 4 }, {}]);
});

test("admin saves playback request headers and conservative ad blocking", async () => {
  const { app, page } = await adminSourcePage();
  const panel = pageClass(page, "network-settings")[0];
  const field = (key) => pageNodes(panel).find((node) => node.attributes?.name === key);
  field("ad_block").checked = true;
  field("play_ua").value = "PolyView Test";
  await pageNodes(panel).find((node) => node.tagName === "FORM").listeners.submit[0]({ preventDefault() {} });
  const call = app.calls.find((item) => item.url === "/api/settings/play" && item.options.method === "PUT");
  assert.deepEqual(JSON.parse(call.options.body), { ad_block: true, play_ua: "PolyView Test", play_referer: "https://player.example/", image_referer: "https://images.example/" });
});

test("admin saves advanced fields, exports configuration, imports JSON and reorders sources", async () => {
  const { app, page } = await adminSourcePage();
  const row = pageClass(page, "source-row")[0];
  const field = (key) => pageNodes(row).find((node) => node.attributes?.name === key);
  field("custom_cookie").value = "demo=1";
  field("searchable").checked = false;
  field("timeout_secs").value = "24";
  await row.listeners.submit[0]({ preventDefault() {} });
  const saved = JSON.parse(app.calls.find((call) => call.url === "/api/sources/4").options.body);
  assert.equal(saved.custom_cookie, "demo=1");
  assert.equal(saved.timeout_secs, 24);
  assert.equal(saved.searchable, false);
  pageButton(page, "导出 JSON").dispatchEvent("click");
  const dialog = pageClass(page, "source-batch-dialog")[0];
  const data = pageNodes(dialog).find((node) => node.tagName === "TEXTAREA");
  assert.equal(dialog.open, true);
  const exported = JSON.parse(data.value);
  assert.equal(exported[0].custom_cookie, "demo=1");
  assert.equal("last_probe_ok" in exported[0], false);
  pageButton(dialog, "关闭").dispatchEvent("click");
  pageButton(page, "导入 JSON").dispatchEvent("click");
  data.value = JSON.stringify(exported);
  await pageButton(dialog, "确认导入").listeners.click[0]();
  assert.deepEqual(JSON.parse(app.calls.find((call) => call.url === "/api/sources/batch").options.body), exported);
  assert.equal(pageButton(row, "上移").disabled, true);
  await pageButton(row, "下移").listeners.click[0]();
  assert.deepEqual(JSON.parse(app.calls.find((call) => call.url === "/api/sources/reorder").options.body), { ids: [7, 4, 9] });
});

test("admin source metrics show real zeroes, unknown rates and probe updates", async () => {
  const { app, sources } = await adminSourcePage();
  Object.assign(sources[0], { health_score: 0, success_rate: 0, fail_streak: 5 });
  Object.assign(sources[1], { health_score: 100, success_rate: null, fail_streak: 0, last_latency_ms: null, last_probe_ok: null });
  const page = await app.settings();
  assert.deepEqual(pageClass(page, "source-score").map((badge) => badge.textContent), [
    "健康度 0/100 · 成功率 0% · 连续失败 5 次", "健康度 100/100 · 成功率 暂无记录 · 连续失败 0 次",
  ]);
  assert.equal(pageClass(page, "source-health")[1].textContent, "未测速");
  Object.assign(sources[0], { health_score: 72, success_rate: 0.625, fail_streak: 0, last_latency_ms: 112 });
  await pageButton(page, "测速").listeners.click[0]();
  assert.equal(pageClass(page, "source-score")[0].textContent, "健康度 72/100 · 成功率 63% · 连续失败 0 次");
  assert.equal(pageClass(page, "source-health")[0].textContent, "112 ms");
});

test("catalog detail chooses the healthiest source, then the higher priority, then quality", async () => {
  const results = [[40, 1, "4K"], [90, 2, "4K"], [90, 1, "HD"], [90, 1, "1080P"]].map(([health_score, source_priority, remark], index) => ({
    title: "同一部电影", source_name: `源${index}`, source_id: index + 1, id: `m${index}`, health_score, source_priority, remark,
  }));
  const path = "/api/catalog/4/detail?id=m3";
  const app = environment({ ...pageDiscoveryRoutes(results), "/api/tracking": [], [path]: { ...results[3], groups: [] } });
  const page = await app.online();
  await pageButton(page, "查看片源").listeners.click[0]();
  assert.ok(app.calls.some(({ url }) => url === path));
  const bar = pageClass(app.elements.get("catalog-dialog"), "catalog-source-switch")[0];
  assert.deepEqual(pageClass(bar, "filter-chip").map((chip) => chip.getAttribute("aria-label")), ["源3 1080P", "源2 HD", "源1 4K", "源0 4K"]);
});

const subscriptionPath = "/api/source-subscriptions";
async function openSourceSubscriptions(rows, items, intercept) {
  const app = environment({ [subscriptionPath]: rows, [`${subscriptionPath}/preview`]: { items },
    [`${subscriptionPath}/sync`]: { synced: 1, failed: 0 }, "/api/sources": [], "/api/catalog/settings": {}, "/api/settings/play": {} });
  app.state.page = "settings";
  app.state.settingsTab = "sources";
  app.state.user = { role: "admin" };
  const fetch = app.context.fetch;
  app.context.fetch = (url, options) => {
    const override = intercept?.(url, options);
    if (override) return override;
    const body = options.body && JSON.parse(options.body);
    if (url === subscriptionPath && options.method === "POST") rows.push({ ...body, id: 7 });
    if (url === `${subscriptionPath}/7` && options.method === "PUT") Object.assign(rows[0], body);
    if (url === `${subscriptionPath}/7` && options.method === "DELETE") rows.splice(0, 1);
    return fetch(url, options);
  };
  const page = await app.settings();
  assert.ok(app.calls.every(({ url }) => url !== subscriptionPath));
  await pageButton(page, "订阅导入 / 远程配置").listeners.click[0]();
  const panel = pageClass(app.elements.get("content"), "subscription-controls")[0];
  const field = (name) => pageNodes(panel).find((node) => node.getAttribute?.("name") === name);
  const click = (label) => pageButton(panel, label).listeners.click[0]();
  const submit = () => pageNodes(panel).find((node) => node.tagName === "FORM").listeners.submit[0]({ preventDefault() {} });
  const checks = () => pageNodes(pageClass(panel, "subscription-choices")[0]).filter((node) => node.tagName === "INPUT");
  return { app, panel, field, click, submit, checks };
}

test("source subscriptions preview, select by kind and URL, edit, toggle, sync and delete", async () => {
  const rows = [], url = "https://feeds.test/list?token=private";
  const shared = "https://media.test/api?key=private";
  const items = [{ kind: "cms", name: "电影", url: shared }, { kind: "parser", name: "解析", url: shared },
    { kind: "live", name: "https://tv.test/list?token=private", url: "https://tv.test/list?token=private" }];
  const { app, panel, field, click, submit, checks } = await openSourceSubscriptions(rows, items);
  assert.match(panel.textContent, /尚未添加远程订阅/);
  assert.equal(pageButton(panel, "同步已启用订阅").disabled, true);
  field("name").value = "家庭仓";
  field("url").value = url;
  await click("预览配置");
  assert.deepEqual(JSON.parse(app.calls.at(-1).options.body), { url });
  assert.equal(checks().length, 3);
  assert.ok(checks().every((check) => check.checked));
  assert.match(panel.textContent, /影视源 · 电影/);
  assert.match(panel.textContent, /解析器 · 解析/);
  assert.match(panel.textContent, /直播源 · 项目 3/);
  checks()[1].checked = false;
  checks()[1].dispatchEvent("change");
  assert.equal(field("all").checked, false);
  await submit();
  const selected = [`cms:${shared}`, `live:${items[2].url}`];
  assert.deepEqual(rows, [{ id: 7, name: "家庭仓", enabled: true, url, selected }]);
  assert.equal(field("url").value, "");
  await click("编辑");
  assert.equal(field("url").value, "");
  await click("预览配置");
  assert.deepEqual(checks().map((check) => Boolean(check.checked)), [true, false, true]);
  field("name").value = "改名";
  await submit();
  assert.equal(rows[0].name, "改名");
  await click("停用");
  assert.equal(rows[0].enabled, false);
  assert.equal(pageButton(panel, "同步").disabled, true);
  await click("启用");
  assert.deepEqual(rows[0], { id: 7, name: "改名", enabled: true, url, selected });
  await click("同步");
  await click("同步已启用订阅");
  assert.deepEqual(app.calls.filter(({ url }) => url === `${subscriptionPath}/sync`).map(({ options }) => JSON.parse(options.body)), [{ id: 7 }, {}]);
  assert.match(panel.textContent, /已同步 1 个订阅，0 个失败/);
  await click("编辑");
  await click("取消编辑");
  assert.equal(field("name").value, "");
  app.context.window.confirm = () => false;
  await click("删除");
  assert.equal(rows.length, 1);
  app.context.window.confirm = () => true;
  await click("删除");
  assert.match(panel.textContent, /尚未添加远程订阅/);
  assert.equal(rows.length, 0);
  assert.doesNotMatch(panel.textContent, /private/);
  assert.ok(app.calls.filter(({ options }) => options.method && options.method !== "GET").every(({ options }) => options.headers["X-PolyView-Request"] === "1"));
  await pageButton(app.elements.get("content"), "影视源管理").listeners.click[0]();
  assert.equal(pageClass(app.elements.get("content"), "subscription-controls").length, 0);
});

test("subscription selection preserves automatic import, explicit none and a changed Base58 address", async () => {
  const rows = [];
  const items = [{ kind: "live", name: "直播", url: "https://media.test/live" }];
  const { panel, field, click, submit, checks } = await openSourceSubscriptions(rows, items);
  field("name").value = "仓库";
  field("url").value = "https://feeds.test/a";
  await submit();
  assert.equal(rows[0].selected, null);
  await click("编辑");
  await click("预览配置");
  checks()[0].checked = false;
  checks()[0].dispatchEvent("change");
  await submit();
  assert.deepEqual(rows[0].selected, []);
  await click("编辑");
  await click("预览配置");
  assert.equal(Boolean(checks()[0].checked), false);
  checks()[0].checked = true;
  checks()[0].dispatchEvent("change");
  field("url").value = "3mJr7AoUXx2Wqd";
  field("url").dispatchEvent("input");
  assert.equal(checks().length, 0);
  assert.match(panel.textContent, /已勾选 0 项/);
  field("all").checked = true;
  field("all").dispatchEvent("change");
  await submit();
  assert.deepEqual(rows[0], { id: 7, name: "仓库", enabled: true, url: "3mJr7AoUXx2Wqd", selected: null });
});

test("subscription errors keep the draft, allow retry and omit backend credentials", async () => {
  const rows = [];
  let failing = subscriptionPath;
  const failure = { ok: false, status: 502, text: async () => JSON.stringify({ error: "https://feed.test/?key=private" }) };
  const { panel, field, click, submit } = await openSourceSubscriptions(rows, [], (url) => {
    if (url === failing) return failure;
    if (url === `${subscriptionPath}/sync`) return { ok: true, text: async () => JSON.stringify({ synced: 0, failed: 1 }) };
  });
  assert.match(panel.textContent, /订阅列表加载失败/);
  failing = "";
  await click("重新加载");
  assert.match(panel.textContent, /尚未添加远程订阅/);
  await click("预览配置");
  assert.match(panel.textContent, /请先填写订阅地址/);
  field("name").value = "订阅";
  field("url").value = "https://feed.test/?key=private";
  failing = `${subscriptionPath}/preview`;
  await click("预览配置");
  assert.match(panel.textContent, /预览失败/);
  assert.doesNotMatch(panel.textContent, /private/);
  failing = "";
  await click("预览配置");
  assert.match(panel.textContent, /没有可导入的项目/);
  failing = subscriptionPath;
  await submit();
  assert.match(panel.textContent, /保存订阅失败/);
  assert.equal(field("name").value, "订阅");
  failing = "";
  await submit();
  rows[0].error = "https://feed.test/?key=private";
  failing = `${subscriptionPath}/7`;
  await click("停用");
  assert.match(panel.textContent, /更改订阅状态失败/);
  await click("删除");
  assert.match(panel.textContent, /删除订阅失败/);
  assert.equal(rows.length, 1);
  failing = `${subscriptionPath}/sync`;
  await click("同步");
  assert.match(panel.textContent, /同步请求失败/);
  failing = "";
  await click("同步");
  assert.match(panel.textContent, /已同步 0 个订阅，1 个失败/);
  assert.match(panel.textContent, /上次同步失败/);
  assert.doesNotMatch(panel.textContent, /private/);
});

test("members cannot open remote subscriptions through stale admin settings", async () => {
  const app = environment();
  app.state.settingsTab = "sources";
  app.state.sourceSettingsTab = "subscriptions";
  app.state.user = { role: "member" };
  const page = await app.settings();
  assert.equal(app.state.settingsTab, "account");
  assert.doesNotMatch(page.textContent, /远程配置/);
  assert.ok(app.calls.every(({ url }) => !url.startsWith(subscriptionPath)));
});

const flushSkipEvents = () => new Promise((resolve) => setImmediate(resolve));

function skipInputs(root) {
  const panel = playbackNode(root, (node) => node.tagName === "DETAILS" && node.textContent.includes("跳过片头/片尾"), "skip panel");
  const input = (attr, value) => playbackNode(panel, (node) => node.tagName === "INPUT" && node.getAttribute(attr) === value, value);
  return { panel, enabled: input("type", "checkbox"), intro: input("name", "intro_sec"), outro: input("name", "outro_sec") };
}

test("detail skip settings persist for the selected catalog source and open with disabled zero defaults", async () => {
  const { app, dialog } = await openTwoEpisodeDetail();
  await flushSkipEvents();
  const { panel, enabled, intro, outro } = skipInputs(dialog);
  assert.equal(panel.open, false);
  assert.equal(enabled.checked, false);
  assert.equal(intro.value, "0");
  assert.equal(outro.value, "0");
  assert.ok(app.calls.some(({ url }) => url === "/api/skip?key=online%3A3%3A42"));
  assert.doesNotMatch(panel.textContent, /设当前时间为片头/);
  enabled.checked = true;
  intro.value = 601;
  const save = pageButton(panel, "保存跳过设置");
  await save.listeners.click[0]();
  assert.equal(app.calls.some(({ url }) => url === "/api/skip"), false);
  intro.value = 11.5;
  outro.value = 35;
  await save.listeners.click[0]();
  assert.deepEqual(savedMutation(app, "/api/skip", "PUT"), {
    media_key: "online:3:42", enabled: true, intro_sec: 11.5, outro_sec: 35,
  });
});

test("the VOD player saves the current intro time and remaining outro time", async () => {
  const app = environment();
  app.play({ id: 9, title: "本地测试" });
  await flushSkipEvents();
  assert.ok(app.calls.some(({ url }) => url === "/api/skip?key=local%3A9"));
  const { panel, enabled, intro, outro } = skipInputs(app.elements.get("player-skip"));
  const video = app.elements.get("video-player");
  video.onloadedmetadata();
  enabled.checked = true;
  video.currentTime = 12.25;
  await pageButton(panel, "设当前时间为片头").listeners.click[0]();
  assert.equal(intro.value, "12.25");
  video.currentTime = 75.5;
  await pageButton(panel, "设当前剩余为片尾").listeners.click[0]();
  assert.equal(outro.value, "24.5");
  assert.deepEqual(app.calls.filter(({ url }) => url === "/api/skip").map(({ options }) => JSON.parse(options.body)), [
    { media_key: "local:9", enabled: true, intro_sec: 12.25, outro_sec: 0 },
    { media_key: "local:9", enabled: true, intro_sec: 12.25, outro_sec: 24.5 },
  ]);
});

test("catalog skipping seeks past the intro once, completes progress and tracking, and autoplays the next episode once", async () => {
  const key = "online:7:a/b";
  const app = environment({ [`/api/skip?key=${encodeURIComponent(key)}`]: { media_key: key, enabled: true, intro_sec: 10, outro_sec: 30 } });
  const playlist = [1, 2].map((number) => ({ title: `第${number}集`, source_id: 7, media_id: "a/b", episode_number: number,
    episode_index: number - 1, tracking_id: 5, remote_url: `https://media.test/${number}.mp4` }));
  const video = app.elements.get("video-player");
  app.play(playlist[0], false, playlist, 0, true);
  video.onloadedmetadata();
  await flushSkipEvents();
  assert.equal(video.currentTime, 10);
  assert.match(app.elements.get("toast").textContent, /已跳过片头/);
  assert.equal(video.paused, false);
  video.currentTime = 3;
  video.dispatchEvent("timeupdate");
  assert.equal(video.currentTime, 3, "seeking back allows rewatching the intro");
  video.currentTime = 69.9;
  video.dispatchEvent("timeupdate");
  assert.equal(app.state.player.playlistIndex, 0);
  video.currentTime = 70;
  video.dispatchEvent("timeupdate");
  video.dispatchEvent("timeupdate");
  video.ended = true;
  video.dispatchEvent("ended");
  await flushSkipEvents();
  assert.equal(app.state.player.playlistIndex, 1);
  assert.equal(app.state.player.autoplay, true);
  assert.deepEqual(savedMutation(app, "/api/tracking/5", "PUT"), { watched_episodes: 1 });
  const completed = app.calls.filter(({ url, options }) => url === "/api/progress" && options.method === "PUT")
    .map(({ options }) => JSON.parse(options.body)).filter((body) => body.position === body.duration);
  assert.equal(completed.length, 1);
  assert.equal(completed[0].remote_url, playlist[0].remote_url);
  video.ended = false;
  video.currentTime = 0;
  video.onloadedmetadata();
  assert.equal(video.playCalls, 2);
});

test("local series opened from the library marks a skipped episode watched before advancing", async () => {
  const episodes = [1, 2].map((id) => ({ id, title: `剧集 ${id}`, kind: "episode", series_id: 8, series: "故事" }));
  const app = environment({ "/api/library": episodes, "/api/skip?key=local%3A1": { enabled: true, intro_sec: 0, outro_sec: 35 } });
  app.state.filter = "episode";
  const page = await app.library();
  savedAction(page, "播放 剧集 1").dispatchEvent("click");
  const video = app.elements.get("video-player");
  await flushSkipEvents();
  video.onloadedmetadata();
  await video.play();
  video.currentTime = 65;
  video.dispatchEvent("timeupdate");
  await flushSkipEvents();
  assert.deepEqual(savedMutation(app, "/api/library/1/progress", "PUT"), { position: 100, duration: 100, watched: true });
  assert.equal(app.state.player.item.id, 2);
  assert.equal(app.state.player.autoplay, true);
});

test("resume preserves later positions, short episodes keep their outro, and live channels do not load skip settings", async () => {
  const app = environment({ "/api/skip?key=local%3A4": { enabled: true, intro_sec: 15, outro_sec: 25 } });
  const video = app.elements.get("video-player");
  app.play({ id: 4, title: "恢复", position: 48.5 });
  await flushSkipEvents();
  video.duration = 70;
  video.onloadedmetadata();
  assert.equal(video.currentTime, 48.5);
  assert.equal(video.playCalls, 0);
  await video.play();
  video.dispatchEvent("timeupdate");
  await flushSkipEvents();
  assert.equal(video.currentTime, 48.5, "the duration must strictly exceed intro + outro + 30");
  video.duration = 100;
  video.pause();
  video.currentTime = 77;
  video.dispatchEvent("timeupdate");
  assert.equal(video.currentTime, 77, "previewing the credits while paused does not finish the episode");
  app.play({ name: "直播", id: 9, url: "https://media.test/live.m3u8" }, true);
  assert.equal(app.elements.get("player-skip").hidden, true);
  assert.deepEqual(app.calls.filter(({ url }) => url.startsWith("/api/skip?")).map(({ url }) => url), ["/api/skip?key=local%3A4"]);
});

test("a delayed skip reply cannot seek the replacement title", async () => {
  const app = environment();
  const fetch = app.context.fetch;
  let release;
  app.context.fetch = (url, options) => url === "/api/skip?key=local%3A1"
    ? new Promise((resolve) => { release = () => resolve({ ok: true, text: async () => JSON.stringify({ enabled: true, intro_sec: 32, outro_sec: 0 }) }); })
    : fetch(url, options);
  const video = app.elements.get("video-player");
  app.play({ id: 1, title: "上一个标题" });
  video.onloadedmetadata();
  app.play({ id: 2, title: "当前标题" });
  video.onloadedmetadata();
  release();
  await flushSkipEvents();
  assert.equal(video.currentTime, 0);
  assert.equal(skipInputs(app.elements.get("player-skip")).enabled.checked, false);
});

test("poster surfaces retry the original URL once through the image proxy before removing the image", async () => {
  const cover = "https://images.test/海报.jpg?size=large&key=A%2FB";
  const item = { id: "1", source_id: 1, source_name: "甲源", title: "故事", cover };
  const app = environment({ ...pageDiscoveryRoutes([item]), "/api/tracking": [], "/api/catalog/1/detail?id=1": { ...item, groups: [] },
    "/api/progress": [{ ...item, entry_kind: "online", media_id: item.id, position: 30, duration: 90 }], "/api/favorites": { catalog: [item] } });
  const discovery = await app.online();
  await app.openCatalogDetail(item);
  const roots = [
    pageClass(discovery, "catalog-art")[0], pageClass(discovery, "spotlight")[0], pageClass(discovery, "continue-section")[0],
    app.elements.get("catalog-dialog"), await app.favorites(), app.trackingRows([{ ...item, watched_episodes: 0, total_episodes: 2 }]),
  ];
  for (const root of roots) {
    const image = playbackNode(root, (node) => node.tagName === "IMG", "poster");
    let removed = false;
    image.remove = () => { removed = true; };
    assert.equal(image.getAttribute("src"), cover);
    image.dispatchEvent("error");
    assert.equal(image.getAttribute("src"), `/api/image?url=${encodeURIComponent(cover)}`);
    assert.equal(removed, false);
    image.dispatchEvent("error");
    assert.equal(removed, true);
  }
});

test("opening playback while a detail save is pending uses the newly saved skip times", async () => {
  const { app, dialog } = await openTwoEpisodeDetail();
  await flushSkipEvents();
  const key = "/api/skip?key=online%3A3%3A42";
  const fetch = app.context.fetch;
  let persisted = { enabled: false, intro_sec: 0, outro_sec: 0 };
  let commit;
  app.context.fetch = async (url, options) => {
    if (url === "/api/skip" && options.method === "PUT") {
      await new Promise((resolve) => { commit = resolve; });
      persisted = JSON.parse(options.body);
    } else if (url !== key) return fetch(url, options);
    const raw = JSON.stringify(persisted);
    return { ok: true, text: async () => raw };
  };
  const { panel, enabled, intro } = skipInputs(dialog);
  enabled.checked = true;
  intro.value = 18.5;
  const saving = pageButton(panel, "保存跳过设置").listeners.click[0]();
  pageButton(dialog, "在线播放").dispatchEvent("click");
  const video = app.elements.get("video-player");
  video.onloadedmetadata();
  commit();
  await saving;
  await flushSkipEvents();
  assert.equal(video.currentTime, 18.5);
  assert.equal(video.playCalls, 0);
});

test("tracking pause and resume persist only the selected subscription and update its badge", async () => {
  const app = environment();
  const list = app.trackingRows([{ id: 7, title: "故事", paused: false }, { id: 9, title: "另一部", paused: true }]);
  const [row, other] = pageClass(list, "tracking-row");
  const badge = pageClass(row, "paused")[0];
  assert.equal(badge.hidden, true);
  assert.equal(pageClass(other, "paused")[0].hidden, false);
  await pageButton(row, "暂停检查").listeners.click[0]();
  assert.equal(badge.hidden, false);
  assert.equal(badge.textContent, "已暂停");
  await pageButton(row, "恢复检查").listeners.click[0]();
  assert.equal(badge.hidden, true);
  assert.ok(pageButton(row, "暂停检查"));
  assert.equal(pageClass(other, "paused")[0].hidden, false);
  assert.deepEqual(app.calls.map(({ url, options }) => [url, options.method, JSON.parse(options.body)]), [
    ["/api/tracking/7", "PUT", { paused: true }], ["/api/tracking/7", "PUT", { paused: false }],
  ]);
  app.context.fetch = async () => { throw new Error("无法保存"); };
  await pageButton(row, "暂停检查").listeners.click[0]();
  assert.equal(badge.hidden, true);
  assert.equal(pageButton(row, "暂停检查").disabled, false);
  assert.match(app.elements.get("toast").textContent, /无法保存/);
});

test("notification and calendar forms load and save their respective configurations", async () => {
  const notifyPath = "/api/settings/notify", calendarPath = "/api/settings/calendar";
  const app = environment({
    [notifyPath]: { bark_url: "https://push.test/key", webhook_post_url: "", webhook_get_url: "https://get.test/old" },
    [calendarPath]: { enabled: true, api_url: "https://calendar.test/original" },
  });
  app.state.user = { role: "admin" };
  app.state.settingsTab = "notify";
  const page = await app.settings();
  const field = (name) => pageNodes(page).find((node) => node.attributes?.name === name);
  assert.equal(field("bark_url").value, "https://push.test/key");
  assert.equal(field("enabled").checked, true);
  assert.equal(field("api_url").value, "https://calendar.test/original");
  field("webhook_post_url").value = "https://post.test/new";
  field("webhook_get_url").value = "";
  field("enabled").checked = false;
  field("api_url").value = "https://mirror.test/calendar";
  for (const form of pageNodes(page).filter((node) => node.tagName === "FORM")) {
    await form.listeners.submit[0]({ preventDefault() {} });
  }
  assert.deepEqual(app.calls.filter(({ options }) => options.method === "PUT").map(({ url, options }) =>
    [url, JSON.parse(options.body), options.headers["X-PolyView-Request"]]), [
    [notifyPath, { bark_url: "https://push.test/key", webhook_post_url: "https://post.test/new", webhook_get_url: "" }, "1"],
    [calendarPath, { enabled: false, api_url: "https://mirror.test/calendar" }, "1"],
  ]);
});

test("test sending reports each notification channel, empty configurations and request failures", async () => {
  const result = { results: [
    { channel: "bark", ok: true, message: "accepted" },
    { channel: "webhook_post", ok: false, message: "unreachable" },
  ] };
  const path = "/api/settings/notify/test";
  const app = environment({ [path]: result });
  app.state.user = { role: "admin" };
  app.state.settingsTab = "notify";
  const page = await app.settings();
  const send = pageButton(page, "测试发送");
  const output = pageNodes(page).find((node) => node.attributes?.role === "status");
  await send.listeners.click[0]();
  assert.deepEqual(app.calls.filter(({ url }) => url === path).map(({ options }) =>
    [options.method, options.headers["X-PolyView-Request"]]), [["POST", "1"]]);
  assert.match(output.textContent, /bark：成功 · accepted/);
  assert.match(output.textContent, /webhook_post：失败 · unreachable/);
  result.results = [];
  await send.listeners.click[0]();
  assert.equal(output.textContent, "尚未配置通知渠道");
  app.context.fetch = async () => { throw new Error("无法连接"); };
  await send.listeners.click[0]();
  assert.match(output.textContent, /测试发送失败：无法连接/);
  assert.equal(send.disabled, false);
});

test("notification and calendar settings stay private to administrators", async () => {
  const app = environment();
  app.state.user = { role: "member" };
  app.state.settingsTab = "notify";
  const page = await app.settings();
  assert.equal(app.state.settingsTab, "account");
  assert.doesNotMatch(page.textContent, /通知与日历/);
  assert.equal(app.calls.length, 0);
});

test("recommendations work without CMS and search the selected title across sources", async () => {
  const cover = "https://images.example.test/poster.jpg";
  const title = "远方";
  const search = `/api/catalog/search?q=${encodeURIComponent(title)}`;
  const sources = [];
  const app = environment({
    "/api/sources": sources,
    "/api/catalog/recommend": { origin: "douban", groups: [
      { title: "热门电影", kind: "movie", items: [{ title, cover, rating: "8.5" }] },
      { title: "热门剧集", kind: "series", items: [{ title: "晴天" }] },
    ] },
    [search]: { results: [] },
  });
  const page = await app.online();
  await new Promise(setImmediate);
  assert.deepEqual(pageClass(page, "recommend-heading").map((node) => node.textContent), ["热门电影豆瓣", "热门剧集豆瓣"]);
  const card = pageClass(page, "discovery-poster")[0];
  const image = pageNodes(card).find((node) => node.tagName === "IMG");
  assert.equal(image.attributes.src, cover);
  image.dispatchEvent("error");
  assert.equal(image.attributes.src, `/api/image?url=${encodeURIComponent(cover)}`);
  card.dispatchEvent("click");
  assert.equal(app.elements.get("search-input").value, title);
  await new Promise(setImmediate);
  assert.match(app.elements.get("content").textContent, /先连接一个影视源/);
  sources.push({ id: 1, enabled: true });
  await app.online();
  assert.ok(app.calls.some(({ url }) => url === search));
  assert.equal(app.calls.filter(({ url }) => url === "/api/catalog/recommend").length, 1);
});

test("CMS fallback recommendations open their source detail directly", async () => {
  const item = { title: "晴天", source_id: 2, id: "42" };
  const detail = "/api/catalog/2/detail?id=42";
  const app = environment({
    "/api/sources": [{ id: 2, enabled: true }],
    "/api/catalog/discover?page=1": { results: [] },
    "/api/catalog/recommend": { origin: "fallback", groups: [{ title: "热门剧集", kind: "series", items: [item] }] },
    [detail]: { ...item, groups: [] }, "/api/tracking": [],
  });
  const page = await app.online();
  assert.equal(pageClass(page, "recommend-origin")[0].textContent, "各源最新");
  await pageClass(page, "discovery-poster")[0].listeners.click[0]();
  assert.ok(app.calls.some(({ url }) => url === detail));
  assert.equal(app.elements.get("catalog-dialog").open, true);
  assert.equal(app.state.query, "");
});

test("recommendations are absent for every discovery filter and search", async () => {
  for (const [key, value] of Object.entries({ onlineSource: "2", onlineKind: "movie", onlineCategory: "喜剧", onlineYear: "2025", onlineLang: "国语", onlineArea: "日本", query: "题名" })) {
    const app = environment({ "/api/sources": [] });
    app.state[key] = value;
    const page = await app.online();
    assert.equal(pageClass(page, "recommend-section").length, 0, key);
    assert.equal(app.calls.some(({ url }) => url === "/api/catalog/recommend"), false, key);
  }
});

test("calendar switches weekdays locally and tracks the selected catalog match", async () => {
  const title = "漫长的旅程";
  const match = { title, source_id: 3, id: "91", kind: "series" };
  const search = `/api/catalog/search?q=${encodeURIComponent(title)}`;
  const app = environment({
    "/api/calendar": { enabled: true, days: [
      { weekday: 1, items: [{ id: 123, title_cn: title, title_raw: "Journey", score: 8.1, cover: "https://poster.test/one.jpg" }] },
      { weekday: 7, items: [{ id: 456, title_raw: "Sunday show" }] },
    ] },
    "/api/sources": [{ id: 3, enabled: true }], [search]: { results: [match] },
    "/api/catalog/3/detail?id=91": { ...match, groups: [{ name: "HLS", direct: true, episodes: [{ name: "第1集", url: "https://source.test/1.m3u8" }] }] },
    "/api/tracking": [],
  });
  Object.assign(app.state, { page: "calendar", query: "old", onlinePage: 2, onlineSource: "7", onlineKind: "movie", onlineYear: "2024", onlineCategory: "喜剧", onlineLang: "英语", onlineArea: "美国" });
  const page = await app.calendar();
  const weekdays = pageClass(page, "filter-chip");
  assert.equal(weekdays.length, 7);
  assert.equal(weekdays[(new Date().getDay() + 6) % 7].attributes["aria-pressed"], "true");
  pageButton(page, "周日").dispatchEvent("click");
  assert.match(page.textContent, /Sunday show/);
  pageButton(page, "周一").dispatchEvent("click");
  assert.doesNotMatch(page.textContent, /Sunday show/);
  assert.equal(pageButton(page, "周一").attributes["aria-pressed"], "true");
  assert.equal(app.calls.filter(({ url }) => url === "/api/calendar").length, 1);
  const card = pageClass(page, "discovery-poster")[0];
  assert.equal(card.attributes["aria-label"], title);
  assert.match(card.textContent, /8\.1 分/);
  card.dispatchEvent("click");
  await new Promise(setImmediate);
  assert.equal(app.state.page, "online");
  assert.equal(app.elements.get("search-input").value, title);
  assert.equal(app.state.onlineSource, "all");
  assert.equal(app.state.onlinePage, 1);
  for (const key of ["onlineKind", "onlineCategory", "onlineYear", "onlineLang", "onlineArea"]) assert.equal(app.state[key], "");
  assert.ok(app.calls.some(({ url }) => url === search));
  const result = pageClass(app.elements.get("content"), "poster-button")[0];
  await result.listeners.click[0]();
  await pageButton(app.elements.get("catalog-dialog"), "加入追剧").listeners.click[0]();
  const request = app.calls.find(({ url, options }) => url === "/api/tracking" && options.method === "POST");
  assert.equal(JSON.parse(request.options.body).source_id, 3);
  assert.equal(JSON.parse(request.options.body).media_id, "91");
});

test("empty and disabled calendar responses are usable without sources", async () => {
  const missing = environment();
  const page = await missing.calendar();
  assert.equal(pageClass(page, "filter-chip").length, 7);
  assert.match(page.textContent, /当天暂无放送/);
  const disabled = environment({ "/api/calendar": { enabled: false, days: [] } });
  const disabledPage = await disabled.calendar();
  assert.equal(pageClass(disabledPage, "filter-chip").length, 0);
  assert.match(disabledPage.textContent, /放送日历已关闭/);
});

test("live timeshift replaces upstream start without losing credentials and restores the live URL", () => {
  const app = environment();
  const original = "https://a.example/live.m3u8?token=A%2BB&quality=hd#stream";
  app.play({ name: "频道", sources: [original, "https://b.example/live.m3u8?start=old"] }, true);
  const bar = app.elements.get("player-timeshift");
  const upstream = () => new URL(new URL(app.hlsInstances.at(-1).source, "https://polyview.test").searchParams.get("url"));
  assert.match(bar.textContent, /依赖上游支持/);
  for (const minutes of [5, 15, 30]) {
    pageButton(bar, `-${minutes}分`).dispatchEvent("click");
    assert.equal(upstream().searchParams.get("start"), String(-minutes * 60));
    assert.equal(upstream().searchParams.get("token"), "A+B");
    assert.equal(upstream().hash, "#stream");
    assert.equal(pageButton(bar, `-${minutes}分`).getAttribute("aria-pressed"), "true");
  }
  app.switchLiveSource(1);
  assert.deepEqual(upstream().searchParams.getAll("start"), ["-1800"]);
  app.switchLiveSource(1);
  pageButton(bar, "直播").dispatchEvent("click");
  assert.equal(upstream().href, original);
  app.closePlayer();
  assert.equal(bar.hidden, true);
});

test("live probes race the proxied sources and play the first successful response", async () => {
  const app = environment();
  const urls = ["slow", "failed", "fast"].map((name) => `https://${name}.example/live.m3u8`);
  app.play({ name: "频道", sources: urls }, true);
  const pending = new Map();
  app.context.fetch = (url) => new Promise((resolve) => pending.set(new URL(url, "https://polyview.test").searchParams.get("url"), resolve));
  const probe = app.elements.get("player-best-source");
  assert.equal(probe.hidden, false);
  const done = probe.listeners.click[0]();
  assert.deepEqual([...pending.keys()], urls);
  pending.get(urls[1])({ ok: false });
  pending.get(urls[2])({ ok: true });
  await done;
  assert.equal(app.state.player.sourceIndex, 2);
  assert.equal(app.hlsInstances.at(-1).source, `/api/playback?url=${encodeURIComponent(urls[2])}`);
  pending.get(urls[0])({ ok: true });
  app.play({ name: "单线路", url: urls[0] }, true);
  assert.equal(probe.hidden, true);
});

test("admin live settings add, disable, enable, refresh and delete M3U subscriptions", async () => {
  const path = "/api/live-sources";
  const rows = [];
  const app = environment({ [path]: rows, "/api/epg/aliases": [], [path + "/refresh"]: { refreshed: 1, failed: 0 } });
  app.state.user = { id: 1, role: "admin" };
  app.state.settingsTab = "epg";
  const originalFetch = app.context.fetch;
  app.context.fetch = (url, options) => {
    if (url === path && options.method === "POST") rows.push({ ...JSON.parse(options.body), id: 7, channel_count: 2 });
    if (url === path + "/7" && options.method === "PUT") Object.assign(rows[0], JSON.parse(options.body));
    if (url === path + "/7" && options.method === "DELETE") rows.pop();
    return originalFetch(url, options);
  };
  const panel = pageNodes(await app.settings()).find((node) => node.tagName === "SECTION" && node.textContent.startsWith("远程 M3U 直播源管理"));
  const form = pageNodes(panel).find((node) => node.tagName === "FORM");
  const input = (name) => pageNodes(form).find((node) => node.attributes?.name === name);
  input("name").value = "家庭电视";
  input("url").value = "https://feeds.example/live.m3u";
  await form.listeners.submit[0]({ preventDefault() {} });
  assert.deepEqual(rows[0], { id: 7, channel_count: 2, name: "家庭电视", url: "https://feeds.example/live.m3u", enabled: true });
  await pageButton(panel, "停用").listeners.click[0]();
  assert.equal(rows[0].enabled, false);
  await pageButton(panel, "启用").listeners.click[0]();
  assert.equal(rows[0].enabled, true);
  await pageButton(panel, "刷新已启用直播源").listeners.click[0]();
  assert.match(panel.textContent, /已刷新 1 个直播源，0 个失败/);
  await pageButton(panel, "删除").listeners.click[0]();
  assert.equal(rows.length, 0);
  assert.doesNotMatch(panel.textContent, /家庭电视/);
  const writes = app.calls.filter(({ options }) => options.method && options.method !== "GET");
  assert.deepEqual(writes.map(({ options }) => options.method), ["POST", "PUT", "PUT", "POST", "DELETE"]);
  assert.ok(writes.every(({ options }) => options.headers["X-PolyView-Request"] === "1"));
});

test("admin edits, adds and removes channel alias pairs", async () => {
  const path = "/api/epg/aliases";
  const app = environment({ [path]: [{ from: "CCTV1", to: "一套" }], "/api/live-sources": [] });
  app.state.user = { id: 1, role: "admin" };
  app.state.settingsTab = "epg";
  const panel = pageNodes(await app.settings()).find((node) => node.tagName === "SECTION" && node.textContent.startsWith("频道别名映射"));
  const form = pageNodes(panel).find((node) => node.tagName === "FORM");
  const fields = (name) => pageNodes(form).filter((node) => node.attributes?.name === name);
  assert.equal(fields("from")[0].value, "CCTV1");
  fields("to")[0].value = "综合";
  pageButton(panel, "添加映射").dispatchEvent("click");
  fields("from")[1].value = " 本地 ";
  fields("to")[1].value = "新闻";
  const save = async () => {
    await form.listeners.submit[0]({ preventDefault() {} });
    const call = app.calls.at(-1);
    assert.equal(call.url, path);
    assert.equal(call.options.method, "PUT");
    assert.equal(call.options.headers["X-PolyView-Request"], "1");
    return JSON.parse(call.options.body);
  };
  assert.deepEqual(await save(), [{ from: "CCTV1", to: "综合" }, { from: "本地", to: "新闻" }]);
  pageButton(panel, "移除映射").dispatchEvent("click");
  assert.deepEqual(await save(), [{ from: "本地", to: "新闻" }]);
  pageButton(panel, "移除映射").dispatchEvent("click");
  assert.deepEqual(await save(), []);
});

test("TVBox presents both configuration formats and copies the selected address", async () => {
  const app = environment({ "/api/tvbox": { ...tvboxPaths, config_b58_path: "/feeds/config?encode=base58" } });
  app.state.settingsTab = "tvbox";
  const group = pageClass(await app.settings(), "tvbox-configs")[0];
  const rows = pageClass(group, "feed-row");
  assert.equal(rows.length, 2);
  const copied = [];
  app.context.navigator.clipboard.writeText = async (value) => { copied.push(value); };
  for (const [index, label] of ["标准配置地址", "Base58 配置地址（旧版影视仓兼容）"].entries()) {
    const input = pageNodes(rows[index]).find((node) => node.tagName === "INPUT");
    assert.equal(input.getAttribute("aria-label"), label);
    await pageButton(rows[index], "复制").listeners.click[0]();
  }
  assert.deepEqual(copied, ["https://polyview.test/feeds/config", "https://polyview.test/feeds/config?encode=base58"]);
});

test("admins change member roles, disable and enable accounts, and reset another member's password", async () => {
  const admin = { id: 1, username: "root", role: "admin", disabled: false };
  const app = environment({ "/api/users": [admin, { id: 7, username: "viewer", role: "user", disabled: false }] });
  app.state.settingsTab = "family";
  app.state.user = admin;
  const panel = await app.settings();
  const [self, member] = pageClass(panel, "member-settings-row");
  const role = (row) => pageNodes(row).find((node) => node.tagName === "SELECT");
  const toggle = (row) => pageNodes(row).find((node) => node.getAttribute?.("role") === "switch");
  assert.equal(role(self).disabled, true);
  assert.equal(toggle(self).disabled, true);
  assert.doesNotMatch(self.textContent, /重置密码|移除/);
  assert.equal(role(member).value, "user");
  role(member).value = "admin";
  await role(member).listeners.change[0]();
  assert.equal(role(member).value, "admin");
  await toggle(member).listeners.click[0]();
  assert.equal(toggle(member).getAttribute("aria-checked"), "false");
  assert.match(member.textContent, /已禁用/);
  await toggle(member).listeners.click[0]();
  assert.equal(toggle(member).getAttribute("aria-checked"), "true");
  assert.deepEqual(app.calls.filter(({ options }) => options.method === "PUT").map(({ url, options }) =>
    [url, JSON.parse(options.body)]), [false, true, false].map((disabled) => ["/api/users/7", { role: "admin", disabled }]));
  const dialog = pageNodes(panel).find((node) => node.tagName === "DIALOG");
  pageButton(member, "重置密码").dispatchEvent("click");
  assert.equal(dialog.open, true);
  assert.match(dialog.textContent, /viewer/);
  const password = pageNodes(dialog).find((node) => node.tagName === "INPUT");
  const form = pageNodes(dialog).find((node) => node.tagName === "FORM");
  const submit = () => form.listeners.submit[0]({ preventDefault() {} });
  for (const invalid of ["too short", "x".repeat(129)]) {
    password.value = invalid;
    await submit();
    assert.equal(dialog.open, true);
    assert.equal(app.calls.some(({ url }) => url.endsWith("/password")), false);
  }
  password.value = "new-password-123";
  await submit();
  const last = app.calls.at(-1);
  assert.equal(last.url, "/api/users/7/password");
  assert.equal(last.options.method, "POST");
  assert.deepEqual(JSON.parse(last.options.body), { password: "new-password-123" });
  assert.equal(dialog.open, false);
  assert.equal(password.value, "");
  for (const { options } of app.calls.filter(({ options }) => options.method)) {
    assert.equal(options.headers["X-PolyView-Request"], "1");
  }
});

test("a rejected member update restores the stored role and leaves the switch enabled", async () => {
  const app = environment({ "/api/users": [{ id: 7, username: "viewer", role: "user", disabled: false }] });
  app.state.user = { id: 1, role: "admin" };
  app.state.settingsTab = "family";
  const row = pageClass(await app.settings(), "member-settings-row")[0];
  const role = pageNodes(row).find((node) => node.tagName === "SELECT");
  const toggle = pageClass(row, "switch")[0];
  app.context.fetch = async () => ({ ok: false, status: 400, text: async () => JSON.stringify({ error: "role rejected" }) });
  role.value = "admin";
  await role.listeners.change[0]();
  assert.equal(role.value, "user");
  assert.equal(role.disabled, false);
  assert.equal(toggle.disabled, false);
  assert.equal(toggle.getAttribute("aria-checked"), "true");
  assert.match(app.elements.get("toast").textContent, /role rejected/);
});

test("backup tools are admin-only, label private exports and only restore a confirmed JSON file", async () => {
  const app = environment();
  app.state.user = { id: 2, role: "user" };
  app.state.settingsTab = "backup";
  assert.doesNotMatch((await app.settings()).textContent, /配置导出|备份与工具/);
  assert.equal(app.state.settingsTab, "account");
  app.state.user = { id: 1, role: "admin" };
  app.state.settingsTab = "backup";
  const page = await app.settings();
  assert.match(page.textContent, /完整配置备份含私密配置/);
  const download = pageNodes(page).find((node) => node.tagName === "A" && node.getAttribute("download"));
  assert.equal(download.getAttribute("href"), "/api/settings/export");
  assert.equal(download.getAttribute("download"), "polyview-config.json");
  const file = pageNodes(page).find((node) => node.getAttribute?.("type") === "file");
  const backup = { version: 1, exported_at: 1, catalog_sources: [], live_sources: [], epg_config: {}, app_settings: { private: "value" } };
  const restore = pageButton(page, "导入备份");
  file.files = [{ name: "broken.json", text: async () => "{broken" }];
  await restore.listeners.click[0]();
  assert.equal(app.calls.length, 0);
  file.files = [{ name: "backup.json", text: async () => JSON.stringify(backup) }];
  app.context.window.confirm = () => false;
  await restore.listeners.click[0]();
  assert.equal(app.calls.length, 0);
  app.context.window.confirm = () => true;
  await restore.listeners.click[0]();
  assert.equal(app.calls.length, 1);
  const { url, options } = app.calls[0];
  assert.equal(url, "/api/settings/import");
  assert.equal(options.method, "POST");
  assert.equal(options.headers["X-PolyView-Request"], "1");
  assert.deepEqual(JSON.parse(options.body), backup);
});

test("the resource tool extracts unique HTTP links, copies them, and imports the edited results", async () => {
  const app = environment({ "/api/sources/batch": { imported: 1, skipped: 0 }, "/api/channels/import": { imported: 2 } });
  app.state.user = { id: 1, role: "admin" };
  app.state.settingsTab = "backup";
  const tool = pageNodes(await app.settings()).find((node) => node.tagName === "DETAILS" && node.textContent.includes("从文本提取资源链接"));
  const [input, result] = pageNodes(tool).filter((node) => node.tagName === "TEXTAREA");
  const destination = pageNodes(tool).find((node) => node.tagName === "SELECT");
  const video = "https://media.test/film(2).mp4", live = "http://live.test/s.m3u8?key=a%2Bb&v=2", cms = "https://cms.test/api.php/provide/vod/";
  input.value = `视频 (${video}) ${video}，直播 ${live}。\n接口 <${cms}> ftp://invalid.test/x.mp4 javascript:alert(1)`;
  pageButton(tool, "提取链接").dispatchEvent("click");
  assert.equal(result.value, [video, live, cms].join("\n"));
  let copied;
  app.context.navigator.clipboard.writeText = async (value) => { copied = value; };
  await pageButton(tool, "复制链接").listeners.click[0]();
  assert.equal(copied, result.value);
  const submit = pageButton(tool, "导入链接");
  result.value = [video, live].join("\n");
  await submit.listeners.click[0]();
  assert.equal(app.calls[0].url, "/api/channels/import");
  assert.deepEqual(JSON.parse(app.calls[0].options.body).playlist.split("\n"), ["#EXTM3U", "#EXTINF:-1,media.test/film(2).mp4", video, "#EXTINF:-1,live.test/s.m3u8", live]);
  destination.value = "sources";
  result.value = cms;
  await submit.listeners.click[0]();
  assert.equal(app.calls[1].url, "/api/sources/batch");
  assert.deepEqual(JSON.parse(app.calls[1].options.body), [{ name: "cms.test/api.php/provide/vod/", url: cms }]);
  for (const { options } of app.calls) {
    assert.equal(options.method, "POST");
    assert.equal(options.headers["X-PolyView-Request"], "1");
  }
});

function m6DownloadsFixture(routes, user = { id: 12, role: "member" }) {
  const app = environment();
  app.state.user = user;
  app.state.page = "downloads";
  app.context.fetch = async (url, options = {}) => {
    const method = options.method || "GET";
    app.calls.push({ url, options });
    if (method !== "GET") assert.equal(options.headers["X-PolyView-Request"], "1");
    const route = routes[url] ?? (url === "/api/downloads" ? [] : { state: "idle" });
    const data = typeof route === "function" ? await route(method, options.body && JSON.parse(options.body)) : route;
    return { ok: true, status: 200, text: async () => JSON.stringify(data) };
  };
  return app;
}

const m6Click = (root, label) => pageButton(root, label).listeners.click[0]();
const m6Field = (root, name) => pageNodes(root).find((node) => node.attributes?.name === name);
function m6Form(root, button) {
  return pageNodes(root).find((node) => node.tagName === "FORM" && pageNodes(node).some((child) => child.tagName === "BUTTON" && child.textContent === button));
}
const m6Submit = (root, button) => {
  const form = m6Form(root, button);
  return form.listeners.submit[0]({ preventDefault() {}, currentTarget: form });
};

test("local library cards and series stacks use real posters, proxied retry and text fallback", async () => {
  const metadata = { cover: "https://fixture.example/cover", description: "影片简介", year: "2018" };
  const app = environment({ "/api/library": [
    { id: 1, kind: "movie", title: "电影甲", ...metadata },
    { id: 2, kind: "episode", title: "第一集", series: "剧甲", series_id: 6 },
    { id: 3, kind: "episode", title: "第二集", series: "剧甲", series_id: 6, ...metadata },
    { id: 4, kind: "movie", title: "电影乙" },
  ] });
  const page = await app.library();
  const posters = pageClass(page, "poster-button");
  for (const name of ["电影甲", "剧甲"]) {
    const poster = posters.find((p) => p.attributes.title.startsWith(name));
    assert.match(poster.attributes.title, /2018 · 影片简介/);
    const art = pageClass(poster, "poster-art")[0];
    const img = pageNodes(art).find((node) => node.tagName === "IMG");
    assert.equal(img.attributes.src, metadata.cover);
    img.remove = () => { art.children = art.children.filter((node) => node !== img); };
    img.dispatchEvent("error");
    assert.equal(img.attributes.src, `/api/image?url=${encodeURIComponent(metadata.cover)}`);
    img.dispatchEvent("error");
    assert.ok(!art.children.includes(img));
    assert.equal(pageClass(art, "poster-name")[0].textContent, name);
  }
  assert.ok(!pageNodes(posters.find((p) => p.attributes.title === "电影乙")).some((node) => node.tagName === "IMG"));
});

test("single and batch catalog downloads carry sanitized movie metadata", async () => {
  const item = { id: "x", source_id: 5, title: "影片", cover: "https://fixture.example/poster" };
  const app = environment({ "/api/catalog/5/detail?id=x": { ...item, year: "2025", description: "<b>简介</b>", groups: [
    { name: "线路", direct: true, episodes: [{ name: "第1集", url: "https://fixture.example/video" }] },
  ] }, "/api/tracking": [] });
  await app.openCatalogDetail(item);
  const dialog = app.elements.get("catalog-dialog");
  await m6Click(dialog, "下载本集");
  await m6Click(dialog, "选择集数");
  await m6Click(dialog, "全选");
  await m6Click(dialog, "批量加入下载队列（1 集）");
  const body = { title: "影片 S01E01", url: "https://fixture.example/video", cover: item.cover, year: "2025", description: "简介" };
  assert.deepEqual(queuedEpisodeBodies(app), [body, body]);
});

test("manual download form sends optional poster, year and description", async () => {
  const app = m6DownloadsFixture({});
  const root = await app.downloads();
  const form = m6Form(root, "开始下载");
  const expected = { title: "影片", url: "https://fixture.example/file.mp4", cover: "https://fixture.example/image", year: "2025", description: "简介" };
  form.elements = Object.fromEntries(Object.entries(expected).map(([key, value]) => {
    const field = m6Field(form, key);
    field.value = value;
    return [key, field];
  }));
  form.reset = () => {};
  await m6Submit(root, "开始下载");
  assert.deepEqual(queuedEpisodeBodies(app), [expected]);
});

test("queue pause, resume and completed cleanup repaint server state and counts", async () => {
  let jobs = [{ id: 41, title: "当前片", status: "running" }, { id: 42, title: "历史片", status: "completed" }];
  const app = m6DownloadsFixture({
    "/api/downloads": () => jobs,
    "/api/downloads/41/pause": () => { jobs[0].status = "paused"; },
    "/api/downloads/41/resume": () => { jobs[0].status = "queued"; },
    "/api/downloads/completed": () => { jobs = jobs.slice(0, 1); },
  });
  const root = await app.downloads();
  await m6Click(root, "暂停");
  assert.equal(pageClass(root, "download-status")[0].textContent, "已暂停");
  pageButton(root, "已暂停 1");
  await m6Click(root, "继续");
  pageButton(root, "排队中 1");
  await m6Click(root, "清理已完成");
  pageButton(root, "全部 1");
  assert.doesNotMatch(root.textContent, /历史片/);
  assert.deepEqual(app.calls.filter(({options}) => options.method && options.method !== "GET").map(({url, options}) => [url, options.method]), [
    ["/api/downloads/41/pause", "POST"], ["/api/downloads/41/resume", "POST"], ["/api/downloads/completed", "DELETE"],
  ]);
});

test("Telegram request, cancellation, host authorization and video import are scoped to the member", async () => {
  const states = new Map();
  const jobs = [];
  const app = m6DownloadsFixture({
    "/api/telegram/login": (method) => {
      const id = app.state.user.id;
      if (method === "POST") states.set(id, "waiting");
      if (method === "DELETE") states.delete(id);
      return { state: states.get(id) || "idle", qr: ["private-qr"], message: "private-stdout" };
    },
    "/api/downloads/telegram": (_, body) => { jobs.push({ id: 1, title: "视频", status: "queued", kind: "telegram", url: body.url }); },
    "/api/downloads": () => app.state.user.id === 12 ? jobs : [],
  });
  const page = await app.downloads();
  await m6Click(page, "申请连接");
  assert.match(page.textContent, /等待服务器管理员/);
  await m6Click(page, "取消申请");
  await m6Click(page, "申请连接");
  states.set(12, "authorized");
  await m6Click(page, "刷新授权状态");
  m6Field(page, "telegram_url").value = "https://t.me/c/12345/7";
  await m6Submit(page, "导入视频");
  assert.equal(jobs[0].url, "https://t.me/c/12345/7");
  assert.equal(m6Field(page, "telegram_url").value, "");
  assert.doesNotMatch(page.textContent, /private-qr|private-stdout|12345/);
  app.state.user = { id: 13, role: "member" };
  const second = await app.downloads();
  pageButton(second, "申请连接");
  assert.equal(m6Field(second, "telegram_url"), undefined);
  assert.equal(app.state.downloads.length, 0);
  const count = app.calls.length;
  await m6Submit(page, "导入视频");
  await page.refreshQueue();
  assert.equal(app.calls.length, count);
  assert.ok(!app.calls.some(({url}) => url.includes("requests") || url.includes("password") || url.startsWith("/api/settings")));
});

test("late queue polls cannot overwrite fresh data or another account's downloads", async () => {
  let reply = () => [];
  const app = m6DownloadsFixture({ "/api/downloads": () => reply() });
  const page = await app.downloads();
  let resolve;
  reply = () => new Promise((done) => { resolve = done; });
  const older = page.refreshQueue();
  const started = app.calls.length;
  await page.refreshQueue(true);
  assert.equal(app.calls.length, started);
  reply = () => [{ id: 5, title: "最新", status: "paused" }];
  await page.refreshQueue();
  resolve([{ id: 5, title: "过期", status: "running" }]);
  await older;
  assert.match(page.textContent, /最新/);
  assert.doesNotMatch(page.textContent, /过期/);
  reply = () => new Promise((done) => { resolve = done; });
  const previousAccount = page.refreshQueue();
  app.state.user = { id: 22, role: "member" };
  reply = () => [];
  await app.downloads();
  resolve([{ id: 7, title: "私密", status: "queued" }]);
  await previousAccount;
  assert.equal(app.state.downloads.length, 0);
});

test("administrator sees official host commands and updates download settings with write-only proxy", async () => {
  const saved = { enabled: true, concurrency: 1, max_attempts: 3, n_m3u8dl_path: "N_m3u8DL-RE", ffmpeg_path: "ffmpeg", ffprobe_path: "ffprobe", tdl_path: "tdl", telegram_directory: "./telegram", telegram_namespace: "family", telegram_proxy_configured: true };
  const changes = [];
  const app = m6DownloadsFixture({
    "/api/telegram/login/requests": [{ user_id: 4, username: "family", command: "tdl -n family_u4 login", uses_proxy: true }],
    "/api/settings/downloads": (method, body) => {
      if (method === "PUT") { changes.push(body); Object.assign(saved, { concurrency: body.concurrency }); }
      return saved;
    },
  }, { id: 1, role: "admin" });
  const page = await app.downloads();
  assert.match(page.textContent, /tdl -n family_u4 login/);
  assert.match(page.textContent, /TDL_PROXY/);
  const proxy = m6Field(page, "telegram_proxy");
  assert.equal(proxy.attributes.type, "password");
  assert.equal(proxy.value, "");
  assert.equal(m6Field(page, "concurrency").attributes.max, "4");
  assert.equal(m6Field(page, "max_attempts").attributes.max, "10");
  m6Field(page, "concurrency").value = "3";
  await m6Submit(page, "保存下载设置");
  assert.ok(!Object.hasOwn(changes[0], "telegram_proxy"));
  assert.equal(changes[0].concurrency, 3);
  assert.equal(changes[0].enabled, true);
  proxy.value = "socks5://fixture:password@localhost";
  await m6Submit(page, "保存下载设置");
  assert.equal(changes[1].telegram_proxy, "socks5://fixture:password@localhost");
  assert.equal(proxy.value, "");
  m6Field(page, "clear_proxy").checked = true;
  await m6Submit(page, "保存下载设置");
  assert.equal(changes[2].telegram_proxy, "");
  assert.doesNotMatch(page.textContent, /fixture:password/);
});

registerParserPanelTests({ environment, pageNodes, pageButton });
