import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const application = await readFile(
  new URL("../web/app.js", import.meta.url),
  "utf8",
);

function environment(routes = {}) {
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
      this.classList = { toggle() {} };
      this.open = false;
      this.currentTime = 0;
      this.duration = 100;
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
    canPlayType() {
      return "";
    }
    load() {}
    pause() {}
    focus() {}
    remove() {}
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
    "player-dialog",
    "player-title",
    "player-kind",
    "player-message",
    "video-player",
    "search-input",
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
    static Events = { ERROR: "error" };
    static isSupported() {
      return true;
    }
    constructor() {
      hlsInstances.push(this);
    }
    on() {}
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
    body: new Element("body"),
    getElementById: (id) => elements.get(id) ?? null,
    createElement: (tag) => new Element(tag),
    createTextNode: (text) => ({ nodeType: 3, textContent: text }),
    querySelectorAll: () => [],
    addEventListener() {},
  };
  const context = vm.createContext({
    document,
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
      "globalThis.hooks = {state, play, closePlayer, saveProgress, openCatalogDetail, authMode};\n})();",
    ),
    context,
  );
  return { ...context.hooks, elements, calls, order, hlsInstances };
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
