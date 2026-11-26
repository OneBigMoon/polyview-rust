import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const script = await readFile(new URL("../../web/cloud.js", import.meta.url), "utf8");
const quark = { provider: "quark", title: "山海 · 4K", url: "https://pan.quark.cn/s/spring", code: "1234", channel: "电影频道" };
const baidu = { provider: "baidu", title: "山海 · 高清", url: "https://pan.baidu.com/s/spring", code: null, channel: "资料馆" };
const drive = { id: 4, name: "家庭影音", provider: "quark", default_dir: "8" };
const recommendations = { origin: "douban", groups: [{ title: "热门电影", items: [{ title: "山海" }] }] };
const queryPath = `/api/cloud/search?q=${encodeURIComponent("山海")}`;

export function registerCloudPageTests({ environment, pageNodes, pageButton }) {
  function setup(role, respond) {
    const app = environment();
    vm.runInContext(script, app.context);
    app.state.user = { id: 1, role, username: role };
    const calls = [];
    app.context.fetch = async (path, opts = {}) => {
      const call = { path, method: opts.method || "GET", body: opts.body && JSON.parse(opts.body), headers: opts.headers };
      calls.push(call);
      const reply = await respond(call);
      return { ok: !reply?.error, status: reply?.error ? 400 : 200, text: async () => JSON.stringify(reply) || "" };
    };
    return { app, calls };
  }
  const find = (root, predicate) => {
    const node = pageNodes(root).find(predicate);
    assert.ok(node, "expected cloud page control");
    return node;
  };
  const named = (root, name) => find(root, (node) => node.attributes?.name === name);
  const click = (root, label) => pageButton(root, label).listeners.click[0]();
  const submit = (root, label) => find(root, (node) => node.tagName === "FORM" && pageNodes(node).some((child) => child.tagName === "BUTTON" && child.textContent === label)).listeners.submit[0]({ preventDefault() {} });
  const settle = () => new Promise((resolve) => setImmediate(resolve));
  async function openSettings(app) {
    app.state.settingsTab = "cloud";
    return app.settings();
  }
  function writesUseSession(calls) {
    for (const { method, headers } of calls) if (method !== "GET") assert.equal(headers["X-PolyView-Request"], "1");
  }

  test("cloud trends search by title, filter seven providers and transfer a checked share to a nested folder as a member", async () => {
    const { app, calls } = setup("member", ({ path }) => ({
      "/api/catalog/recommend": recommendations, [queryPath]: [quark, baidu], "/api/cloud/choices": [drive],
      "/api/cloud/check": { valid: true, title: "山海" }, "/api/cloud/folders?account_id=4&dir=8": [{ id: "9", name: "科幻" }],
      "/api/cloud/folders?account_id=4&dir=9": [], "/api/cloud/save": { path: "/影音/科幻", task_id: "task-9" },
    })[path]);
    const root = await app.cloud();
    await settle();
    assert.match(root.textContent, /豆瓣热门榜/);
    await click(root, "山海");
    assert.equal(named(root, "cloud_query").value, "山海");
    const filter = named(root, "cloud_provider");
    assert.equal(filter.children.length, 8);
    assert.match(root.textContent, /山海 · 高清/);
    filter.value = "quark";
    filter.dispatchEvent("change");
    assert.doesNotMatch(root.textContent, /山海 · 高清/);
    await click(filter, "百度");
    assert.equal(pageButton(filter, "百度").getAttribute("aria-pressed"), "true");
    assert.match(root.textContent, /山海 · 高清/);
    await click(filter, "夸克");
    assert.match(root.textContent, /电影频道 · 提取码 1234/);
    await click(root, "校验与转存");
    const dialog = find(root, (node) => node.tagName === "DIALOG");
    assert.equal(dialog.open, true);
    assert.deepEqual(calls.find((c) => c.path === "/api/cloud/check").body, { account_id: 4, url: quark.url, code: "1234" });
    assert.doesNotMatch(dialog.textContent, /STRM/);
    await click(dialog, "科幻");
    assert.match(dialog.textContent, /默认目录（8） \/ 科幻/);
    await click(dialog, "默认目录（8）");
    assert.ok(calls.at(-1).path.endsWith("dir=8"));
    await click(dialog, "科幻");
    await click(dialog, "上一级");
    await click(dialog, "科幻");
    await click(dialog, "确认转存");
    assert.deepEqual(calls.at(-1).body, { account_id: 4, url: quark.url, code: "1234", dir: "9" });
    assert.match(dialog.textContent, /\/影音\/科幻 · 任务 task-9/);
    assert.ok(calls.every((c) => !["/api/cloud/accounts", "/api/cloud/bot", "/api/cloud/channels"].includes(c.path)));
    writesUseSession(calls);
  });

  test("invalid cloud links can be rechecked, and admin STRM export requires explicit selection and reports the server outcome", async () => {
    let attempts = 0;
    const { app, calls } = setup("admin", ({ path, body }) => {
      if (path === "/api/cloud/check") return { valid: body.code === "good" };
      if (path === "/api/cloud/save") return ++attempts === 1 ? { error: "Configure POLYVIEW_PUBLIC_URL" } : { path: "/电影", strm_notice: "任务已提交，尚未生成 STRM。" };
      return { "/api/catalog/recommend": recommendations, [queryPath]: [quark], "/api/cloud/choices": [drive],
        "/api/cloud/folders?account_id=4&dir=8": [], "/api/cloud/folders?account_id=4": [] }[path];
    });
    const root = await app.cloud();
    named(root, "cloud_query").value = "山海";
    await submit(root, "搜索资源");
    await click(root, "校验与转存");
    assert.match(find(root, (node) => node.attributes?.role === "alert").textContent, /分享链接无效/);
    assert.ok(!calls.some((c) => c.path.includes("folders")));
    const code = named(root, "code");
    code.value = "good"; code.dispatchEvent("input");
    await click(root, "校验链接");
    await click(root, "浏览根目录");
    const strm = named(root, "export_strm");
    assert.equal(Boolean(strm.checked), false);
    strm.checked = true;
    await click(root, "确认转存");
    assert.match(root.textContent, /Configure POLYVIEW_PUBLIC_URL/);
    await click(root, "确认转存");
    assert.deepEqual(calls.at(-1).body, { account_id: 4, url: quark.url, code: "good", dir: "0", export_strm: true });
    assert.match(root.textContent, /任务已提交，尚未生成 STRM/);
    writesUseSession(calls);
  });

  test("cloud search keeps the latest query when older results arrive late and announces empty and failed searches", async () => {
    let release;
    const { app } = setup("member", ({ path }) => {
      if (path === "/api/catalog/recommend") return { groups: [] };
      if (path.endsWith("q=first")) return new Promise((resolve) => { release = resolve; });
      if (path.endsWith("q=latest")) return [baidu];
      if (path.endsWith("q=empty")) return [];
      return { error: "offline" };
    });
    const root = await app.cloud(), query = named(root, "cloud_query");
    query.value = "first";
    const old = submit(root, "搜索资源");
    query.value = "latest";
    await submit(root, "搜索资源");
    release([quark]); await old;
    assert.match(root.textContent, /山海 · 高清/);
    assert.doesNotMatch(root.textContent, /山海 · 4K/);
    assert.ok(!pageNodes(root).some((node) => node.tagName === "BUTTON" && node.textContent === "校验与转存"));
    query.value = "empty"; await submit(root, "搜索资源");
    assert.match(root.textContent, /没有找到匹配的资源/);
    query.value = "failure"; await submit(root, "搜索资源");
    assert.match(find(root, (node) => node.attributes?.role === "alert").textContent, /搜索失败/);
  });

  test("cloud account credentials remain write-only through create and update, and member sharing defaults off", async () => {
    const accounts = [], { app, calls } = setup("admin", ({ path, method, body }) => {
      if (path === "/api/cloud/accounts" && method === "GET") return accounts;
      if (method === "POST") { accounts.push({ ...drive, name: body.name, credential_configured: true }); return accounts[0]; }
      if (method === "DELETE") { accounts.pop(); return; }
      if (path === "/api/cloud/bot") return { bindings: [], enabled: false };
      return [];
    });
    const root = await openSettings(app);
    await click(root, "添加账号");
    named(root, "name").value = "家庭影音";
    named(root, "cookie").value = "local-cookie";
    named(root, "token").value = "local-token";
    await submit(root, "保存账号");
    const created = calls.find((c) => c.method === "POST");
    assert.equal(created.body.members_allowed, false);
    assert.equal(created.body.cookie, "local-cookie");
    assert.equal(created.body.token, "local-token");
    await click(root, "编辑");
    for (const key of ["cookie", "token"]) {
      assert.equal(named(root, key).value, "");
      assert.equal(named(root, key).attributes.type, "password");
    }
    named(root, "members_allowed").checked = true;
    await submit(root, "保存账号");
    const edited = calls.find((c) => c.method === "PUT");
    assert.equal(edited.path, "/api/cloud/accounts/4");
    assert.equal(edited.body.members_allowed, true);
    assert.ok(!("cookie" in edited.body || "token" in edited.body));
    assert.doesNotMatch(root.textContent, /local-cookie|local-token/);
    app.context.window.confirm = () => false;
    await click(root, "移除");
    assert.ok(!calls.some((c) => c.method === "DELETE"));
    app.context.window.confirm = () => true;
    await click(root, "移除");
    assert.equal(calls.filter((c) => c.method === "DELETE").length, 1);
    writesUseSession(calls);
  });

  test("resource channel settings edit subscriptions and support JSON file import and authenticated JSON export", async () => {
    const channels = [], { app, calls } = setup("admin", ({ method, path, body }) => {
      if (path === "/api/cloud/channels/import") { channels.push({ id: 7, ...body.channels[0] }); return { imported: 1 }; }
      if (path === "/api/cloud/channels") return channels;
      if (path === "/api/cloud/channels/7") { Object.assign(channels[0], body); return; }
      if (path === "/api/cloud/bot") return { bindings: [] };
      return [];
    });
    const root = await openSettings(app);
    await click(root, "导入频道");
    assert.match(root.textContent, /请输入含有频道列表的 JSON/);
    assert.ok(!calls.some((c) => c.method === "POST"));
    const channel = { name: "影视公告", kind: "public", target: "fixtures", enabled: true };
    const file = named(root, "channels_file");
    file.files = [{ text: async () => JSON.stringify([channel]) }];
    await file.listeners.change[0]();
    await click(root, "导入频道");
    assert.equal(named(root, "channels_json").value, "");
    assert.match(root.textContent, /已导入 1 个频道/);
    assert.deepEqual(calls.find((c) => c.method === "POST").body, { channels: [channel] });
    const link = find(root, (node) => node.tagName === "A" && node.textContent === "导出频道 JSON");
    assert.equal(link.attributes.href, "/api/cloud/channels/export");
    assert.match(link.attributes.download, /\.json$/);
    await click(root, "编辑");
    named(root, "kind").value = "api"; named(root, "target").value = "https://fixture.example/search";
    named(root, "enabled").checked = false;
    await submit(root, "保存频道");
    assert.deepEqual(calls.find((c) => c.method === "PUT").body, { name: "影视公告", kind: "api", target: "https://fixture.example/search", enabled: false });
    writesUseSession(calls);
  });

  test("bot settings bind a sender and chat to a member and preserve the configured token on unrelated edits", async () => {
    const config = { enabled: true, token_configured: true, bindings: [] };
    const { app, calls } = setup("admin", ({ path, method, body }) => {
      if (path === "/api/users") return [{ id: 12, username: "家人" }];
      if (path !== "/api/cloud/bot") return [];
      if (method === "PUT") Object.assign(config, { bindings: body.bindings, enabled: body.enabled });
      return config;
    });
    const root = await openSettings(app);
    assert.equal(named(root, "bot_token").value, "");
    assert.equal(named(root, "bot_token").attributes.type, "password");
    await click(root, "添加聊天授权");
    await submit(root, "保存 Bot 配置");
    assert.match(root.textContent, /请为每个授权/);
    assert.ok(!calls.some((c) => c.method === "PUT"));
    named(root, "sender_id").value = "123"; named(root, "chat_id").value = "-555";
    await submit(root, "保存 Bot 配置");
    assert.deepEqual(calls.find((c) => c.method === "PUT").body, { enabled: true, bindings: [{ sender_id: 123, chat_id: -555, user_id: 12 }] });
    await click(root, "移除授权");
    named(root, "bot_token").value = "replacement-token";
    await submit(root, "保存 Bot 配置");
    assert.deepEqual(calls.filter((c) => c.method === "PUT").at(-1).body, { enabled: true, bindings: [], token: "replacement-token" });
    assert.equal(named(root, "bot_token").value, "");
    writesUseSession(calls);
  });

  test("members cannot open cloud configuration or request its admin endpoints", async () => {
    const { app, calls } = setup("member", () => []);
    const root = await openSettings(app);
    assert.equal(app.state.settingsTab, "account");
    assert.doesNotMatch(root.textContent, /网盘账号配置|资源频道订阅管理|Bot Token/);
    assert.ok(!calls.some((c) => c.path.startsWith("/api/cloud/") || c.path === "/api/users"));
  });
}
