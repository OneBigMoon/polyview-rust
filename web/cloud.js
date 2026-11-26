(() => {
  "use strict";

  const providers = {
    quark: "夸克网盘", "115": "115 网盘", tianyi: "天翼云盘", ali: "阿里云盘",
    baidu: "百度网盘", uc: "UC 网盘", xunlei: "迅雷云盘",
  };
  const transferable = ["quark", "115", "tianyi"];
  const channelKinds = { public: "公开 Telegram 频道", private: "已登录 Telegram 会话", api: "外部搜索 API" };
  const base = "/api/cloud";

  globalThis.polyviewCloud = function ({ make, button, head, api, admin }) {
    const label = (name, control) => make("label", {}, [name, control]);
    const message = (text = "") => make("p", { class: "hint", role: "status", text });
    const error = (text) => make("p", { class: "form-error", role: "alert", text });
    const actions = (...children) => make("div", { class: "cloud-actions" }, children);
    const submit = (text) => make("button", { type: "submit", class: "button primary", text });
    const option = (value, text) => make("option", { value, text });
    const input = (name, value = "", attrs = {}) => make("input", { name, value, ...attrs });
    const select = (name, items, value) => {
      const node = make("select", { name }, items.map(([key, title]) => option(key, title)));
      if (value != null) node.value = String(value);
      return node;
    };
    const checkbox = (text, name, checked) => {
      const control = input(name, "1", { type: "checkbox", checked });
      return { control, node: make("label", { class: "cloud-check" }, [control, text]) };
    };
    const secret = (name, placeholder) => input(name, "", { type: "password", autocomplete: "new-password", spellcheck: "false", placeholder });
    const post = (path, body) => api(`${base}/${path}`, { method: "POST", body });
    const publicLink = (value) => {
      try {
        const url = new URL(value);
        return ["https:", "http:"].includes(url.protocol) && !url.username && !url.password ? url.href : null;
      } catch { return null; }
    };

    function page() {
      const search = input("cloud_query", "", { type: "search", required: true, placeholder: "输入片名或剧名" });
      const filter = select("cloud_provider", [["", "全部网盘"], ...Object.entries(providers)]);
      const results = make("div", { class: "cloud-results" });
      const status = make("div", { "aria-live": "polite" });
      const trends = make("section", { class: "panel", "aria-label": "热门榜单" });
      const dialog = make("dialog", { class: "info-dialog cloud-dialog", "aria-labelledby": "cloud-transfer-title" });
      let rows = [], searched = false, request = 0;
      function paintResults() {
        if (!searched) return;
        const visible = rows.filter((row) => !filter.value || filter.value === row.provider);
        status.replaceChildren(message(visible.length ? `找到 ${visible.length} 条资源` : "没有找到匹配的资源，请更换片名或网盘。"));
        results.replaceChildren(...visible.map((row) => {
          const link = publicLink(row.url);
          const links = actions();
          if (link) {
            links.append(make("a", { class: "button subtle small-button", href: link, target: "_blank", rel: "noopener noreferrer", text: "打开分享链接" }));
            if (transferable.includes(row.provider)) links.append(button("校验与转存", "primary small-button", () => openTransfer(dialog, row)));
          }
          return make("article", { class: "cloud-result" }, [make("div", {}, [
            make("h3", { text: row.title || row.url }),
            make("p", { class: "hint", text: [providers[row.provider] || row.provider, row.channel, row.code && `提取码 ${row.code}`].filter(Boolean).join(" · ") }),
          ]), links]);
        }));
      }
      async function find(title) {
        const query = title.trim();
        search.value = query;
        const current = ++request;
        rows = []; searched = false;
        results.replaceChildren();
        if (!query) { status.replaceChildren(error("请输入片名或剧名。")); return; }
        status.replaceChildren(message(`正在搜索《${query}》…`));
        try {
          const data = await api(`${base}/search?q=${encodeURIComponent(query)}`);
          if (current !== request) return;
          rows = data; searched = true; paintResults();
        } catch {
          if (current === request) status.replaceChildren(error("资源搜索失败，请重试。"));
        }
      }
      async function loadTrends() {
        trends.replaceChildren(message("正在加载热门榜单…"));
        try {
          const data = await api("/api/catalog/recommend");
          const groups = (data.groups || []).filter((group) => group.items?.length);
          trends.replaceChildren(make("h2", { text: data.origin === "douban" ? "豆瓣热门榜" : "热门片单" }), ...groups.map((group) =>
            make("details", { class: "cloud-trend", open: groups[0] === group }, [make("summary", { text: group.title }),
              actions(...group.items.map(({ title }) => button(title, "subtle small-button", () => find(title)))),
            ])));
          if (!groups.length) trends.append(message("暂无热门片单。"));
        } catch {
          trends.replaceChildren(error("热门榜单加载失败。"), button("重试榜单", "subtle small-button", loadTrends));
        }
      }
      filter.addEventListener("change", paintResults);
      const root = make("div", {}, [head("网盘搜片", "搜索资源频道，或从热门榜单选择片名。"),
        make("form", { class: "panel form-row", on: { submit: (event) => { event.preventDefault(); return find(search.value); } } },
          [label("片名或剧名", search), label("网盘类型", filter), submit("搜索资源")]),
        status, results, trends, dialog]);
      loadTrends();
      return root;
    }

    async function openTransfer(dialog, row) {
      const title = make("h2", { id: "cloud-transfer-title", text: `转存《${row.title || providers[row.provider]}》` });
      const close = button("关闭", "subtle small-button", () => dialog.close());
      const content = make("div", { class: "stack" }, message("正在读取可用账号…"));
      dialog.replaceChildren(make("div", { class: "dialog-top" }, [title, close]), content);
      if (!dialog.open) dialog.showModal();
      let choices;
      try { choices = (await api(`${base}/choices`)).filter((entry) => entry.provider === row.provider); }
      catch { content.replaceChildren(error("无法读取可用账号。"), button("重试", "subtle", () => openTransfer(dialog, row))); return; }
      if (!choices.length) { content.replaceChildren(message(`没有可用的${providers[row.provider]}账号，请联系管理员。`)); return; }
      const account = select("account_id", choices.map((entry) => [entry.id, entry.name]));
      const code = input("code", row.code || "");
      const detail = make("div", { class: "stack" });
      const status = make("div", { "aria-live": "polite" });
      const verify = button("校验链接", "subtle", check);
      let revision = 0;
      const selected = () => choices.find((entry) => String(entry.id) === account.value);
      const payload = () => ({ account_id: selected().id, url: row.url, ...(code.value.trim() ? { code: code.value.trim() } : {}) });
      function reset() { revision++; detail.replaceChildren(); status.replaceChildren(); verify.disabled = false; }
      async function check() {
        reset();
        const version = revision;
        verify.disabled = true;
        status.replaceChildren(message("正在校验分享链接…"));
        try {
          const result = await post("check", payload());
          if (version !== revision) return;
          if (!result.valid) { status.replaceChildren(error("分享链接无效或提取码错误。")); return; }
          status.replaceChildren(message(`链接有效${result.title ? ` · ${result.title}` : ""}`));
          await chooseFolder(version);
        } catch { if (version === revision) status.replaceChildren(error("校验失败，请检查提取码并重试。")); }
        finally { if (version === revision) verify.disabled = false; }
      }
      async function chooseFolder(version) {
        const trail = [], location = message(), folders = make("div", { class: "cloud-actions" });
        const state = make("div", { "aria-live": "polite" });
        const lock = make("fieldset", { class: "cloud-fields stack" });
        const save = button("确认转存", "primary", transfer);
        const defaultDir = selected().default_dir;
        const rootDir = { id: row.provider === "tianyi" ? "-11" : "0", name: "根目录" };
        const strm = admin() && row.provider === "quark" ? checkbox("转存完成后生成单个视频的 STRM 文件", "export_strm", false) : null;
        async function browse(nextTrail) {
          lock.disabled = true;
          state.replaceChildren(message("正在读取目录…"));
          const target = nextTrail.at(-1);
          try {
            const params = new URLSearchParams({ account_id: account.value });
            if (target.id !== rootDir.id) params.set("dir", target.id);
            const children = await api(`${base}/folders?${params}`);
            if (version !== revision) return;
            trail.splice(0, trail.length, ...nextTrail);
            location.textContent = `目标目录：${trail.map((dir) => dir.name).join(" / ")}`;
            folders.replaceChildren();
            if (trail.length > 1) folders.append(button("上一级", "subtle", () => browse(trail.slice(0, -1))));
            if (trail[0].id !== rootDir.id) folders.append(button("浏览根目录", "subtle", () => browse([rootDir])));
            for (const dir of children) folders.append(button(dir.name, "subtle", () => browse([...trail, dir])));
            state.replaceChildren(...(children.length ? [] : [message("此目录没有子目录。") ]));
          } catch {
            state.replaceChildren(error("无法读取目录。"), button("重试目录", "subtle", () => browse(nextTrail)));
            if (!trail.length && target.id !== rootDir.id) folders.replaceChildren(button("浏览根目录", "subtle", () => browse([rootDir])));
          } finally { lock.disabled = false; save.disabled = !trail.length; }
        }
        async function transfer() {
          if (save.disabled || lock.disabled || version !== revision) return;
          save.disabled = lock.disabled = account.disabled = code.disabled = verify.disabled = true;
          state.replaceChildren(message("正在转存…"));
          try {
            const result = await post("save", { ...payload(), dir: trail.at(-1).id, ...(strm?.control.checked ? { export_strm: true } : {}) });
            content.replaceChildren(message(`转存结果：${result.path}${result.task_id ? ` · 任务 ${result.task_id}` : ""}`));
            if (result.strm_file) content.append(message(`已生成 STRM：${result.strm_file}`));
            if (result.strm_notice) content.append(message(result.strm_notice));
          } catch (cause) {
            state.replaceChildren(error(`转存请求失败：${cause.message}`));
            save.disabled = lock.disabled = account.disabled = code.disabled = verify.disabled = false;
          }
        }
        lock.append(make("legend", { text: "选择保存目录" }), location, folders, state);
        if (strm) lock.append(strm.node);
        lock.append(save);
        detail.replaceChildren(lock);
        await browse([defaultDir && defaultDir !== rootDir.id ? { id: defaultDir, name: `默认目录（${defaultDir}）` } : rootDir]);
      }
      account.addEventListener("change", check);
      code.addEventListener("input", reset);
      content.replaceChildren(label("网盘账号", account), label("提取码", code), verify, status, detail);
      await check();
    }

    function channelFields(row) {
      const name = input("name", row?.name, { required: true });
      const kind = select("kind", Object.entries(channelKinds), row?.kind || "public");
      const target = input("target", row?.target, { required: true, spellcheck: "false" });
      function hint() {
        target.placeholder = { public: "频道用户名，如 my_channel", private: "Telegram 会话 ID", api: "https://example.com/search" }[kind.value];
      }
      kind.addEventListener("change", hint);
      hint();
      const enabled = checkbox("启用此搜索源", "enabled", row?.enabled ?? true);
      return {
        fields: [label("频道名称", name), label("来源类型", kind), label("频道或接口地址", target), enabled.node],
        body: () => ({ name: name.value.trim(), kind: kind.value, target: target.value.trim(), enabled: enabled.control.checked === true }),
      };
    }

    function accountFields(row) {
      const name = input("name", row?.name, { required: true });
      const provider = select("provider", transferable.map((key) => [key, providers[key]]), row?.provider);
      if (row) provider.disabled = true;
      const dir = input("default_dir", row?.default_dir, { placeholder: "留空使用根目录" });
      const note = row?.credential_configured ? "留空保留已保存的凭据" : "Cookie 或 Token 至少填写一项";
      const cookie = secret("cookie", note), token = secret("token", note);
      const share = checkbox("允许家庭成员使用此账号转存", "members_allowed", Boolean(row?.members_allowed));
      return {
        fields: [label("账号名称", name), label("网盘类型", provider), label("默认保存目录 ID", dir),
          label("Cookie", cookie), label("Token", token), share.node],
        body: () => ({ name: name.value.trim(), provider: provider.value, default_dir: dir.value.trim(),
          members_allowed: share.control.checked === true, ...(cookie.value.trim() ? { cookie: cookie.value.trim() } : {}), ...(token.value.trim() ? { token: token.value.trim() } : {}) }),
      };
    }

    async function collection(path, title, noun, fieldsFor, describe) {
      const url = `${base}/${path}`;
      const rows = make("div", { class: "cloud-settings-list" });
      const editor = make("div"), status = make("div", { "aria-live": "polite" });
      const controls = make("fieldset", { class: "cloud-fields stack", "aria-label": title });
      const panel = make("section", { class: "panel" }, [make("h2", { text: title }), controls, status]);
      async function load() {
        try {
          const data = await api(url);
          rows.replaceChildren(...data.map((row) => make("article", { class: "cloud-result" }, [
            make("div", {}, [make("h3", { text: row.name }), make("p", { class: "hint", text: describe(row) })]),
            actions(button("编辑", "subtle small-button", () => edit(row), { "aria-label": `编辑${row.name}` }),
              button("移除", "danger small-button", async () => {
                if (!window.confirm(`移除${noun}“${row.name}”？`)) return;
                await mutate(() => api(`${url}/${row.id}`, { method: "DELETE" }), `已移除${noun}。`);
              }, { "aria-label": `移除${row.name}` })),
          ])));
          if (!data.length) rows.append(message(`尚未添加${noun}。`));
        } catch {
          rows.replaceChildren(error(`${noun}列表加载失败。`), button("重新加载", "subtle", load));
        }
      }
      async function mutate(operation, success) {
        if (controls.disabled) return;
        controls.disabled = true;
        try {
          const result = await operation();
          editor.replaceChildren();
          status.replaceChildren(message(typeof success === "function" ? success(result) : success));
          await load();
        } catch { status.replaceChildren(error(`${noun}操作失败，请检查输入后重试。`)); }
        finally { controls.disabled = false; }
      }
      function edit(row) {
        const fields = fieldsFor(row);
        const form = make("form", { class: "stack cloud-editor", autocomplete: "off", on: { submit: (event) => {
          event.preventDefault();
          return mutate(() => api(row ? `${url}/${row.id}` : url, { method: row ? "PUT" : "POST", body: fields.body() }), `已保存${noun}。`);
        } } }, [make("h3", { text: `${row ? "编辑" : "添加"}${noun}` }), ...fields.fields,
          actions(submit(`保存${noun}`), button("取消", "subtle", () => editor.replaceChildren()))]);
        editor.replaceChildren(form);
        fields.fields[0]?.querySelector?.("input")?.focus();
      }
      controls.append(rows, actions(button(`添加${noun}`, "subtle", () => edit(null))), editor);
      if (path === "channels") controls.append(importExport(mutate));
      await load();
      return panel;
    }

    function importExport(mutate) {
      const data = make("textarea", { name: "channels_json", rows: 5, spellcheck: "false", placeholder: '{"channels":[{"name":"影片","kind":"public","target":"my_channel","enabled":true}]}' });
      const file = input("channels_file", "", { type: "file", accept: "application/json,.json" });
      const status = make("div", { "aria-live": "polite" });
      const go = button("导入频道", "subtle", async () => {
        let channels;
        try {
          const json = JSON.parse(data.value);
          channels = Array.isArray(json) ? json : json.channels;
          if (!Array.isArray(channels) || !channels.length) throw new Error();
        } catch { status.replaceChildren(error("请输入含有频道列表的 JSON。")); return; }
        status.replaceChildren();
        await mutate(() => post("channels/import", { channels }), (result) => {
          data.value = file.value = "";
          return `已导入 ${result.imported} 个频道。`;
        });
      });
      file.addEventListener("change", async () => {
        const selected = file.files?.[0];
        if (!selected) return;
        go.disabled = true;
        try { data.value = await selected.text(); status.replaceChildren(); }
        catch { status.replaceChildren(error("无法读取文件，请粘贴 JSON。")); }
        finally { go.disabled = false; }
      });
      return make("details", { class: "cloud-import" }, [make("summary", { text: "批量导入与导出" }),
        make("div", { class: "stack" }, [label("选择 JSON 文件", file), label("频道 JSON", data),
          actions(go, make("a", { class: "button subtle", href: `${base}/channels/export`, download: "polyview-cloud-channels.json", text: "导出频道 JSON" })), status])]);
    }

    async function botSettings() {
      const body = make("div"), status = make("div", { "aria-live": "polite" });
      const panel = make("section", { class: "panel" }, [make("h2", { text: "Telegram Bot" }), body, status]);
      const url = `${base}/bot`;
      async function load() {
        try {
          const [config, users] = await Promise.all([api(url), api("/api/users")]);
          const members = (Array.isArray(users) ? users : users.users || []).map((user) => [user.id, user.username]);
          const enabled = checkbox("启用 Telegram Bot", "enabled", config.enabled);
          const token = secret("bot_token", config.token_configured ? "留空保留已保存的 Token" : "输入 Bot Token");
          const clear = checkbox("清除已保存的 Token", "clear_token", false);
          clear.control.addEventListener("change", () => { token.disabled = clear.control.checked; if (token.disabled) token.value = ""; });
          const bindings = [], list = make("div", { class: "stack" });
          function add(binding) {
            const sender = input("sender_id", binding?.sender_id, { required: true, inputmode: "numeric", pattern: "[1-9][0-9]*" });
            const chat = input("chat_id", binding?.chat_id, { required: true, pattern: "-?[1-9][0-9]*" });
            const member = select("user_id", members, binding?.user_id);
            const entry = { sender, chat, member };
            entry.node = make("div", { class: "cloud-binding" }, [label("发送者 ID", sender), label("聊天 ID", chat), label("家庭成员", member),
              button("移除授权", "subtle small-button", () => {
                bindings.splice(bindings.indexOf(entry), 1);
                list.replaceChildren(...bindings.map((item) => item.node));
              })]);
            bindings.push(entry); list.append(entry.node);
          }
          for (const binding of config.bindings) add(binding);
          const lock = make("fieldset", { class: "cloud-fields stack", "aria-label": "Bot 配置" }, [enabled.node, label("Bot Token", token),
            ...(config.token_configured ? [clear.node] : []), make("h3", { text: "允许的聊天" }),
            message("私聊的聊天 ID 与发送者 ID 相同。群聊请填写负数聊天 ID，并指定允许发送指令的成员。"),
            list, actions(button("添加聊天授权", "subtle", () => add(null)), submit("保存 Bot 配置"))]);
          const form = make("form", { autocomplete: "off", on: { submit: async (event) => {
            event.preventDefault();
            if (lock.disabled) return;
            const mappings = bindings.map(({ sender, chat, member }) => ({ sender_id: Number(sender.value), chat_id: Number(chat.value), user_id: Number(member.value) }));
            if (mappings.some(({ sender_id, chat_id, user_id }) => !Number.isSafeInteger(sender_id) || sender_id <= 0 || !Number.isSafeInteger(chat_id) || chat_id === 0 || !Number.isSafeInteger(user_id) || user_id <= 0)) {
              status.replaceChildren(error("请为每个授权填写有效的发送者、聊天 ID 并选择家庭成员。")); return;
            }
            lock.disabled = true;
            try {
              const value = token.value.trim();
              await api(url, { method: "PUT", body: { enabled: enabled.control.checked === true, bindings: mappings,
                ...(clear.control.checked ? { token: "" } : value ? { token: value } : {}) } });
              token.value = "";
              status.replaceChildren(message("Bot 配置已保存。"));
              await load();
            } catch { status.replaceChildren(error("Bot 配置保存失败，请检查 Token 和聊天授权。")); }
            finally { lock.disabled = false; }
          } } }, lock);
          body.replaceChildren(form);
        } catch { body.replaceChildren(error("无法读取 Bot 配置或成员列表。"), button("重新加载 Bot 配置", "subtle", load)); }
      }
      await load();
      return panel;
    }

    async function settings() {
      if (!admin()) return error("仅管理员可管理网盘设置。");
      const panels = await Promise.all([
        collection("accounts", "网盘账号配置", "账号", accountFields, (row) => [providers[row.provider], row.credential_configured ? "凭据已配置" : "待配置凭据", row.members_allowed ? "家庭成员可用" : "管理员专用"].join(" · ")),
        collection("channels", "资源频道订阅管理", "频道", channelFields, (row) => [channelKinds[row.kind], row.target, row.enabled ? "已启用" : "已停用"].join(" · ")),
        botSettings(),
      ]);
      return make("div", { class: "cloud-settings" }, panels);
    }

    return { page, settings };
  };
})();
