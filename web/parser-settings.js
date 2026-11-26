(() => {
  "use strict";

  globalThis.polyviewParserPanel = async function ({ make, button, api, toast }) {
    const path = "/api/parsers";
    const rows = new Map();
    const list = make("div", { class: "stack" });
    const add = make("div");
    const panel = make("section", { class: "panel" }, [
      make("h2", { text: "解析接口" }),
      make("p", { class: "panel-description", text: "非直链剧集会按优先级从高到低尝试匹配线路的接口。" }),
      add, list,
    ]);
    const label = (text, input) => make("label", {}, [text, input]);

    function paintList() {
      const ordered = [...rows.values()].sort((a, b) => b.priority - a.priority || a.id - b.id);
      list.replaceChildren(...(ordered.length ? ordered.map((row) => row.node) : [make("p", { class: "hint", text: "尚未配置解析接口。" })]));
    }

    function editor(parser, lock) {
      const name = make("input", { name: "name", required: true, value: parser?.name || "" });
      const secret = { type: "password", autocomplete: "new-password", spellcheck: "false" };
      const template = make("input", { ...secret, name: "url_template", required: !parser,
        placeholder: parser ? "留空保留已保存的模板" : "https://example.com/parse?url={url}" });
      const headers = make("input", { ...secret, name: "headers_json",
        placeholder: parser ? "留空保留，输入 {} 清除" : '{"User-Agent":"PolyView"}' });
      const flags = make("input", { name: "flags", value: parser?.flags ?? "*", required: true });
      const priority = make("input", { name: "priority", type: "number", step: 1, value: parser?.priority ?? 0, required: true });
      const timeout = make("input", { name: "timeout_ms", type: "number", min: 1, max: 120000, step: 1,
        value: parser?.timeout_ms ?? 5000, required: true });
      const enabled = make("input", { name: "enabled", type: "checkbox", role: "switch", checked: parser?.enabled ?? true });
      async function save(event) {
        event.preventDefault();
        if (lock.disabled) return;
        const body = { name: name.value.trim(), flags: flags.value.trim(), priority: Number(priority.value),
          timeout_ms: Number(timeout.value), enabled: Boolean(enabled.checked) };
        if (!parser || template.value.trim()) body.url_template = template.value.trim();
        const raw = headers.value.trim();
        if (raw) {
          let values;
          try { values = JSON.parse(raw); } catch { /* Show the same guidance for malformed JSON and invalid values. */ }
          if (!values || typeof values !== "object" || Array.isArray(values) || Object.values(values).some((v) => typeof v !== "string")) {
            toast("请求头必须是 JSON 对象，且每个值都是字符串。", true);
            return;
          }
          body.headers_json = JSON.stringify(values);
        } else if (!parser) body.headers_json = "{}";
        lock.disabled = true;
        try {
          const saved = await api(parser ? `${path}/${parser.id}` : path, { method: parser ? "PUT" : "POST", body });
          template.value = "";
          headers.value = "";
          putRow(saved);
          paintList();
          if (!parser) newEditor();
          toast(parser ? "解析接口已保存" : "解析接口已添加");
        } catch { toast("保存失败，请检查接口配置后重试。", true); }
        finally { lock.disabled = false; }
      }
      return make("form", { class: "settings-form-grid", autocomplete: "off", on: { submit: save } }, [
        label("名称", name),
        make("label", {}, ["接口地址模板", parser ? make("span", { class: "configured-badge", text: "已配置" }) : null, template]),
        label("请求头（JSON）", headers),
        make("p", { class: "hint settings-full-row", text: "模板用 {url} 代表剧集地址，也可用 url= 结尾。地址和请求头仅供写入。" }),
        label("线路标识（逗号分隔，* 表示全部）", flags), label("优先级", priority), label("超时（毫秒）", timeout),
        make("label", { class: "checkbox-label" }, [enabled, "启用"]),
        button(parser ? "保存" : "添加接口", "primary", null, { type: "submit" }),
      ]);
    }

    function newEditor() {
      const box = make("fieldset", { class: "stack" }, make("legend", { text: "添加解析接口" }));
      box.append(editor(null, box));
      add.replaceChildren(box);
    }

    function putRow(parser) {
      const box = make("fieldset", { class: "stack" }, make("legend", { text: parser.name }));
      const endpoint = `${path}/${parser.id}`;
      const url = make("input", { name: "test_url", type: "url", required: true, autocomplete: "off", placeholder: "https://…" });
      const result = make("span", { role: "status" });
      async function probe(event) {
        event.preventDefault();
        if (box.disabled) return;
        box.disabled = true;
        result.textContent = "测试中…";
        try {
          const resolved = await api(`${endpoint}/test`, { method: "POST", body: { url: url.value.trim() } });
          result.textContent = resolved.is_hls ? "测试成功（HLS）" : "测试成功";
        } catch { result.textContent = "测试失败，请检查接口配置和剧集地址。"; }
        finally { box.disabled = false; }
      }
      async function remove() {
        if (box.disabled || !window.confirm(`移除解析接口“${parser.name}”？`)) return;
        box.disabled = true;
        try {
          await api(endpoint, { method: "DELETE" });
          rows.delete(parser.id);
          paintList();
          toast("解析接口已移除");
        } catch { toast("移除失败，请重试。", true); }
        finally { box.disabled = false; }
      }
      box.append(
        make("p", { class: "hint", text: `${parser.enabled ? "已启用" : "已停用"} · 线路 ${parser.flags} · 优先级 ${parser.priority}` }),
        make("details", {}, [make("summary", { text: "编辑接口" }), editor(parser, box)]),
        make("form", { class: "form-row", on: { submit: probe } }, [label("测试剧集地址", url),
          button("测试", "subtle", null, { type: "submit" }), result, button("移除", "danger", remove)]),
      );
      rows.set(parser.id, { id: parser.id, priority: parser.priority, node: box });
    }

    try {
      const parsers = await api(path);
      parsers.forEach(putRow);
      paintList();
      newEditor();
    } catch { list.replaceChildren(make("p", { class: "form-error", text: "无法加载解析接口，请重新打开此分栏。" })); }
    return panel;
  };
})();
