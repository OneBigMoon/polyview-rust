import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const script = await readFile(new URL("../../web/parser-settings.js", import.meta.url), "utf8");

export function registerParserPanelTests({ environment, pageNodes, pageButton }) {
  async function openPanel(role, handle) {
    const app = environment();
    vm.runInContext(script, app.context);
    app.state.user = { id: 1, username: "viewer", role };
    app.state.settingsTab = "parsers";
    const calls = [];
    app.context.fetch = async (path, options) => {
      const method = options.method || "GET";
      const body = options.body && JSON.parse(options.body);
      calls.push({ method, path, body, headers: options.headers });
      const { status = 200, result } = handle(method, path, body);
      return { ok: status < 400, status, text: async () => JSON.stringify(result) || "" };
    };
    return { app, calls, root: await app.settings() };
  }
  const find = (root, predicate) => {
    const node = pageNodes(root).find(predicate);
    assert.ok(node, "expected form control in parser settings");
    return node;
  };
  const box = (root, title) => find(root, (node) => node.tagName === "FIELDSET" && node.children[0].textContent === title);
  const field = (root, name) => find(root, (node) => node.attributes?.name === name);
  const submit = (root, label) => {
    const form = find(root, (node) => node.tagName === "FORM" && pageNodes(node).some((child) => child.tagName === "BUTTON" && child.textContent === label));
    return form.listeners.submit[0]({ preventDefault() {} });
  };
  const saved = { id: 5, name: "主接口", flags: "qq,youku", priority: 9, timeout_ms: 4500, enabled: true };

  test("parser settings create, edit and remove through authenticated requests while preserving write-only credentials", async () => {
    const { app, calls, root } = await openPanel("admin", (method, path, body) => {
      if (method === "GET" && path === "/api/parsers") return { result: [saved] };
      if (method === "DELETE") return { status: 204 };
      const { name, flags, priority, timeout_ms, enabled } = body;
      return { result: { id: method === "PUT" ? 5 : 8, name, flags, priority, timeout_ms, enabled } };
    });
    let row = box(root, "主接口");
    assert.equal(field(row, "url_template").value, "");
    assert.equal(field(row, "headers_json").value, "");
    assert.equal(field(row, "url_template").attributes.type, "password");
    assert.equal(field(row, "headers_json").attributes.type, "password");
    field(row, "priority").value = "12";
    field(row, "enabled").checked = false;
    await submit(row, "保存");
    assert.equal(calls.at(-1).method, "PUT");
    assert.equal(calls.at(-1).path, "/api/parsers/5");
    assert.deepEqual(calls.at(-1).body, { name: "主接口", flags: "qq,youku", priority: 12, timeout_ms: 4500, enabled: false });

    row = box(root, "主接口");
    field(row, "headers_json").value = '{"Authorization":123}';
    await submit(row, "保存");
    assert.match(app.elements.get("toast").textContent, /JSON/);
    assert.equal(calls.length, 2);
    field(row, "headers_json").value = "{}";
    await submit(row, "保存");
    assert.equal(calls.at(-1).body.headers_json, "{}");
    assert.equal(calls.at(-1).body.url_template, undefined);

    const add = box(root, "添加解析接口");
    field(add, "name").value = "备用接口";
    field(add, "url_template").value = "https://parser.example/?token=hidden-key&url={url}";
    field(add, "headers_json").value = '{"Authorization":"Bearer secret-value"}';
    await submit(add, "添加接口");
    assert.equal(calls.at(-1).method, "POST");
    assert.equal(calls.at(-1).path, "/api/parsers");
    assert.deepEqual(calls.at(-1).body, { name: "备用接口", url_template: "https://parser.example/?token=hidden-key&url={url}",
      headers_json: '{"Authorization":"Bearer secret-value"}', flags: "*", priority: 0, timeout_ms: 5000, enabled: true });
    const secondary = box(root, "备用接口");
    for (const name of ["url_template", "headers_json"]) {
      assert.equal(field(add, name).value, "");
      assert.equal(field(secondary, name).value, "");
    }
    app.context.window.confirm = () => false;
    await pageButton(secondary, "移除").listeners.click[0]();
    assert.ok(!calls.some(({ method }) => method === "DELETE"));
    app.context.window.confirm = () => true;
    await pageButton(secondary, "移除").listeners.click[0]();
    assert.equal(calls.at(-1).method, "DELETE");
    assert.equal(calls.at(-1).path, "/api/parsers/8");
    assert.doesNotMatch(root.textContent, /备用接口/);
    for (const call of calls.filter(({ method }) => method !== "GET")) assert.equal(call.headers["X-PolyView-Request"], "1");
  });

  test("testing a saved parser reports success and a retryable failure without rendering its playback grant", async () => {
    let fail = true;
    const { calls, root } = await openPanel("admin", (method) => method === "GET" ? { result: [saved] } : fail
      ? { status: 502, result: { error: "upstream failed" } }
      : { result: { url: "https://secret.example/stream", playback_url: "/api/playback?grant=secret", is_hls: true } });
    const row = box(root, "主接口");
    field(row, "test_url").value = "https://episodes.example/show/1";
    await submit(row, "测试");
    assert.match(row.textContent, /测试失败/);
    assert.equal(row.disabled, false);
    fail = false;
    await submit(row, "测试");
    assert.equal(calls.at(-1).path, "/api/parsers/5/test");
    assert.equal(calls.at(-1).method, "POST");
    assert.equal(calls.at(-1).headers["X-PolyView-Request"], "1");
    assert.deepEqual(calls.at(-1).body, { url: "https://episodes.example/show/1" });
    assert.match(row.textContent, /测试成功（HLS）/);
    assert.doesNotMatch(row.textContent, /secret/);
  });

  test("members opening parser settings fall back to account settings without requesting parser configuration", async () => {
    const { app, calls, root } = await openPanel("user", () => ({ status: 403 }));
    assert.equal(app.state.settingsTab, "account");
    assert.match(root.textContent, /修改密码/);
    assert.doesNotMatch(root.textContent, /解析接口/);
    assert.deepEqual(calls, []);
  });
}
