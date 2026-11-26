(() => {
  "use strict";

  globalThis.polyviewMedia = function ({ make, button, api, play, posterImage, admin, current }) {
    const input = (name, value = "", attrs = {}) => make("input", { name, value: value ?? "", ...attrs });
    const label = (text, node) => make("label", {}, [text, node]);
    const check = (name, text, checked) => {
      const field = input(name, "1", { type: "checkbox", checked });
      return { field, label: make("label", { class: "checkbox-label" }, [field, text]) };
    };
    const select = (name, options, selected) => {
      const field = make("select", { name }, options.map(([value, text]) => make("option", { value, text })));
      if (selected != null) field.value = selected;
      return field;
    };
    const status = () => make("p", { class: "media-feedback", role: "status" });
    const hint = (text) => make("p", { class: "hint", text });
    const heading = (text) => make("h3", { text });
    const write = (path, method, body) => api(path, { method, body });
    const providers = [["qbittorrent", "qBittorrent"], ["transmission", "Transmission"]];
    const splitKeywords = (value) => value.split(/[,，\n]+/).map((word) => word.trim()).filter(Boolean);

    // Keep stored credentials out of control values. Omission preserves a secret on PUT.
    function secret(name, text, configured) {
      const field = input(name, "", { type: "password", autocomplete: "new-password", placeholder: configured ? "已配置，留空保留" : "尚未配置" });
      const clear = check(`clear_${name}`, `清除已保存的${text}`, false);
      const nodes = [label(text, field)];
      if (configured) nodes.push(clear.label);
      return { nodes, values: () => clear.field.checked ? { [name]: "" } : field.value ? { [name]: field.value } : {}, reset: () => { field.value = ""; } };
    }

    function submitForm(text, fields, feedback, run, success) {
      const submit = button(text, "primary small-button", null, { type: "submit" });
      return make("form", { class: "media-fields", on: { submit: async (event) => {
        event.preventDefault();
        if (!current() || submit.disabled) return;
        submit.disabled = true;
        feedback.textContent = "正在提交…";
        try {
          await run();
          if (current()) feedback.textContent = success;
        } catch (_) {
          if (current()) feedback.textContent = "提交失败，请检查填写内容或连接后重试。";
        } finally { submit.disabled = false; }
      } } }, [...fields, submit]);
    }

    function action(text, feedback, run, success, confirmText) {
      const control = button(text, "subtle small-button", async () => {
        if (!current() || control.disabled || (confirmText && !window.confirm(confirmText))) return;
        control.disabled = true;
        feedback.textContent = "处理中…";
        try {
          await run();
          if (current()) feedback.textContent = success;
        } catch (_) {
          if (current()) feedback.textContent = "操作失败，请检查连接与服务状态后重试。";
        } finally { control.disabled = false; }
      });
      return control;
    }

    async function detail(item) {
      if (!current()) return;
      let dialog = document.getElementById("library-detail-dialog");
      if (!dialog) {
        dialog = make("dialog", { id: "library-detail-dialog", class: "info-dialog media-detail", "aria-labelledby": "library-detail-title" });
        document.body.append(dialog);
      }
      const revision = String(Number(dialog.dataset.revision || 0) + 1);
      dialog.dataset.revision = revision;
      const active = () => current() && dialog.open && dialog.dataset.revision === revision;
      const close = button("关闭详情", "subtle small-button", () => dialog.close());
      const title = make("h2", { id: "library-detail-title", text: item.series || item.title });
      const top = make("div", { class: "dialog-top" }, [title, close]);
      const content = make("div", { class: "media-detail-body", "aria-live": "polite" }, hint("正在读取媒体详情…"));
      dialog.replaceChildren(top, content);
      if (!dialog.open) dialog.showModal();
      const path = `/api/library/${encodeURIComponent(item.id)}`;
      let media;
      try { media = await api(path); }
      catch (_) {
        if (active()) content.replaceChildren(hint("无法读取影片详情。"), button("重试", "subtle", () => detail(item)));
        return;
      }
      if (!active()) return;
      const episodes = media.episodes || [];
      function watch(selected) {
        if (!active()) return;
        dialog.close();
        const index = episodes.findIndex((episode) => episode.id === selected.id);
        if (index < 0) play(selected);
        else play(episodes[index], false, episodes, index);
      }
      title.textContent = media.series || media.title;
      const copy = make("div", { class: "media-detail-copy" }, [
        media.original_title && media.original_title !== title.textContent ? make("p", { class: "media-original", text: media.original_title }) : null,
        make("p", { class: "media-facts", text: [media.year, media.rating > 0 && `评分 ${media.rating}`, ...(media.genres || [])].filter(Boolean).join(" · ") }),
        media.description ? make("p", { text: media.description }) : null,
        button(media.position > 0 && !media.watched ? "继续观看" : "播放影片", "primary", () => watch(media)),
      ]);
      const hero = make("section", { class: "media-detail-hero" }, [
        media.backdrop ? posterImage(media.backdrop, { class: "media-backdrop" }) : null,
        media.cover ? posterImage(media.cover, { class: "media-cover" }) : null, copy,
      ]);
      content.replaceChildren(hero);
      for (const [name, people] of [["演员", media.cast], ["主创", media.crew]]) {
        if (people?.length) content.append(make("section", {}, [heading(name), make("ul", { class: "media-credits" }, people.map((person) => make("li", { text: [person.name, person.role].filter(Boolean).join(" · ") })))]));
      }
      if (episodes.length || media.seasons?.length) {
        const seasons = new Map((media.seasons || []).map((season) => [season.number, season]));
        for (const episode of episodes) {
          const number = episode.season ?? 1;
          if (!seasons.has(number)) seasons.set(number, { number });
        }
        const seasonSelect = select("library_season", [...seasons.keys()].sort((a, b) => a - b).map((number) => [number, seasons.get(number).title || `第 ${number} 季`]), seasons.has(media.season) ? media.season : undefined);
        const grid = make("div", { class: "episode-grid" });
        function paintSeason() {
          const number = Number(seasonSelect.value);
          const metadata = new Map((seasons.get(number)?.episodes || []).map((episode) => [episode.number, episode]));
          const files = episodes.filter((episode) => (episode.season ?? 1) === number);
          const available = new Set(files.map((episode) => episode.episode));
          const entries = files.map((episode) => [episode.episode, button(`第 ${episode.episode ?? "?"} 集 · ${metadata.get(episode.episode)?.title || episode.title}`, episode.watched ? "subtle watched" : "subtle", () => watch(episode))]);
          for (const [episode, info] of metadata) {
            if (!available.has(episode)) entries.push([episode, button(`第 ${episode} 集 · ${info.title || "未入库"}`, "subtle", null, { disabled: true, title: "未入库" })]);
          }
          grid.replaceChildren(...entries.sort(([a], [b]) => a - b).map(([, node]) => node));
          if (!entries.length) grid.append(hint("本季暂无分集。"));
        }
        seasonSelect.addEventListener("change", paintSeason);
        paintSeason();
        content.append(make("section", {}, [label("选择季", seasonSelect), grid]));
      }
      if (admin()) {
        const feedback = status();
        const name = input("title", media.series || media.title, { required: true });
        const year = input("year", media.year, { pattern: "[0-9]{4}", maxlength: 4 });
        const tmdb = input("tmdb_id", "", { type: "number", min: 1, step: 1 });
        const states = { queued: "排队中", running: "处理中", done: "已完成", retry: "等待重试" };
        const panel = make("details", { class: "media-editor" }, [make("summary", { text: "识别与整理" }),
          media.automation_status ? hint(`自动化状态：${states[media.automation_status] || media.automation_status}`) : null,
          submitForm("重新识别", [label("匹配片名", name), label("年份（可选）", year), label("TMDB 编号（可选）", tmdb)], feedback, () => write(`${path}/reidentify`, "POST", { title: name.value.trim(), ...(year.value.trim() && { year: year.value.trim() }), ...(tmdb.value && { tmdb_id: Number(tmdb.value) }) }), "已加入识别队列。"),
          make("div", { class: "head-actions" }, [action("重新刮削", feedback, () => write(`${path}/scrape`, "POST", {}), "已加入刮削队列。"),
            action("整理此文件", feedback, () => write(`${path}/organize`, "POST", {}), "文件已整理，可在媒体整理记录中查看。", `按当前模板整理“${media.title}”的文件？`),
            button("刷新影片详情", "subtle small-button", () => detail(item))]), feedback]);
        content.append(panel);
      }
    }

    function rules(subscription) {
      const existing = subscription.rules || {};
      const quality = input("quality", existing.quality, { placeholder: "例如 1080p、4K" });
      const include = input("include_keywords", (existing.include_keywords || []).join(", "));
      const exclude = input("exclude_keywords", (existing.exclude_keywords || []).join(", "));
      const feedback = status();
      return make("details", { class: "tracking-rules media-editor" }, [make("summary", { text: "订阅资源筛选" }), submitForm("保存筛选规则", [label("画质", quality), label("包含关键词（逗号分隔）", include), label("排除关键词（逗号分隔）", exclude)], feedback, async () => {
        const values = { quality: quality.value.trim(), include_keywords: splitKeywords(include.value), exclude_keywords: splitKeywords(exclude.value) };
        await write(`/api/tracking/${encodeURIComponent(subscription.id)}`, "PUT", { rules: values });
        subscription.rules = values;
      }, "筛选规则已保存。"), feedback]);
    }
    function wishlist(paintSubscriptions) {
      const panel = make("details", { class: "panel" }, make("summary", { text: "豆瓣想看同步" }));
      const body = make("div"), feedback = status();
      panel.append(body, feedback);
      const path = "/api/tracking/douban";
      let loaded = false, loading = false;
      async function load() {
        if (!current() || loading) return;
        loading = true;
        try {
          const saved = await api(path);
          if (!current()) return;
          const profile = input("profile", saved.profile, { placeholder: "豆瓣个人主页的用户 ID", pattern: "[A-Za-z0-9_\\-]+", maxlength: 80 });
          const interval = input("interval_hours", saved.interval_hours || 24, { type: "number", min: 1, max: 168, required: true });
          const enabled = check("enabled", "定时同步", saved.enabled), auto = check("auto_download", "新增订阅自动下载", saved.auto_download);
          const lastSync = (info) => info.last_error ? `上次同步失败：${info.last_error}` : info.last_checked ? `上次同步 ${new Date(info.last_checked * 1000).toLocaleString()} · 导入 ${info.imported || 0} 项` : "尚未同步";
          const result = hint(lastSync(saved));
          const save = submitForm("保存想看设置", [label("豆瓣用户", profile), label("同步间隔（小时）", interval), enabled.label, auto.label], feedback, async () => {
            const info = await write(path, "PUT", { profile: profile.value.trim(), interval_hours: Number(interval.value), enabled: Boolean(enabled.field.checked), auto_download: Boolean(auto.field.checked) });
            if (current()) result.textContent = lastSync(info);
          }, "想看同步设置已保存。");
          body.replaceChildren(save,
            result, action("同步已保存的想看列表", feedback, async () => {
              const info = await write(`${path}/sync`, "POST", {});
              if (!current()) return;
              result.textContent = lastSync(info);
              try {
                const subscriptions = await api("/api/tracking");
                if (current()) paintSubscriptions(subscriptions);
              } catch (_) {
                if (current()) result.textContent += " · 重新打开追剧清单可加载最新订阅";
              }
            }, "想看列表同步完成。"));
          loaded = true;
        } catch (_) { if (current()) body.replaceChildren(hint("无法读取豆瓣同步设置。"), button("重试", "subtle", load)); }
        finally { loading = false; }
      }
      panel.addEventListener("toggle", () => { if (panel.open && !loaded) return load(); });
      return panel;
    }

    function tasks() {
      const path = "/api/external-tasks";
      const panel = make("details", { class: "panel external-tasks" }, make("summary", { text: "外部下载任务" }));
      const formHost = make("div"), list = make("div"), feedback = status();
      panel.append(formHost, feedback, list);
      let loaded = false, loading = false, fetching = false, revision = 0;
      let filePanels = new Map();
      const labels = { queued: "排队中", running: "下载中", paused: "已暂停", completed: "已完成", failed: "失败", unavailable: "不可用" };
      async function refresh(background = false) {
        if (!panel.open || !current() || (background && (!loaded || fetching))) return;
        const request = ++revision;
        fetching = true;
        try {
          const jobs = await api(path);
          if (!current() || request !== revision) return;
          const previousPanels = filePanels;
          filePanels = new Map();
          list.replaceChildren(...jobs.map((job) => {
            const row = make("article", { class: "media-record" });
            const endpoint = `${path}/${encodeURIComponent(job.id)}`;
            const update = (suffix, method) => async () => { await write(`${endpoint}${suffix}`, method); await refresh(); };
            const percent = Math.max(0, Math.min(100, Math.round((job.progress || 0) * 100)));
            row.append(heading(job.name), hint(`${labels[job.status] || job.status} · ${percent}%`), make("progress", { max: 100, value: percent, "aria-label": `${job.name} 下载进度` }));
            if (job.files?.length) {
              const files = make("details", {}, [make("summary", { text: `文件（${job.files.length}）` }), make("ul", {}, job.files.map((file) => make("li", { text: file.name })))]);
              files.open = Boolean(previousPanels.get(job.id)?.open);
              filePanels.set(job.id, files);
              row.append(files);
            }
            const controls = make("div", { class: "head-actions" });
            if (["running", "queued"].includes(job.status)) controls.append(action("暂停任务", feedback, update("/pause", "POST"), "任务已暂停。"));
            if (job.status === "paused") controls.append(action("恢复任务", feedback, update("/resume", "POST"), "任务已恢复。"));
            controls.append(action("移除任务", feedback, update("", "DELETE"), "已移除任务。", `移除“${job.name}”任务并保留已下载文件？`));
            row.append(controls);
            return row;
          }));
          if (!jobs.length) list.append(hint("暂无外部下载任务。"));
        } catch (_) {
          if (current() && request === revision) list.replaceChildren(hint("无法读取外部下载任务。"), button("重试任务列表", "subtle", () => refresh()));
        } finally { if (request === revision) fetching = false; }
      }
      async function load() {
        if (!current() || loading) return;
        loading = true;
        try {
          const choices = await api("/api/downloaders/choices");
          if (!current()) return;
          if (choices.length) {
            const downloader = select("downloader_id", choices.map((client) => [client.id, client.name]));
            const url = input("url", "", { required: true, placeholder: "magnet:?… 或种子文件 URL" });
            formHost.replaceChildren(submitForm("提交外部下载", [label("下载器", downloader), label("种子链接", url)], feedback, async () => {
              await write(path, "POST", { downloader_id: Number(downloader.value), url: url.value.trim() });
              url.value = "";
              await refresh();
            }, "已提交外部下载。"));
          } else formHost.replaceChildren(hint("当前账户没有可用的外部下载器。"));
          loaded = true;
        } catch (_) { if (current()) formHost.replaceChildren(hint("无法读取可用下载器。"), button("重试下载器列表", "subtle", load)); }
        finally { loading = false; }
      }
      panel.addEventListener("toggle", () => { if (panel.open) return Promise.all([load(), refresh()]); });
      return { panel, refresh };
    }

    function scraperConfig(saved, feedback, reload) {
      const path = "/api/settings/scraper";
      const enabled = check("enabled", "启用元数据刮削", saved.enabled);
      const key = secret("tmdb_key", "TMDB API Key", saved.has_tmdb_key), proxy = secret("proxy", "刮削代理地址", saved.has_proxy);
      const endpoints = [["tmdb_url", "TMDB API（更换地址后需重新填写密钥）"], ["image_url", "海报服务"], ["bangumi_url", "Bangumi"], ["douban_url", "豆瓣"]].map(([name, text]) => ({ name, text, field: input(name, saved[name], { type: "url" }) }));
      return submitForm("保存刮削设置", [enabled.label, ...key.nodes, ...proxy.nodes, ...endpoints.map(({ text, field }) => label(`${text} 地址`, field))], feedback, async () => {
        const urls = Object.fromEntries(endpoints.map(({ name, field }) => [name, field.value.trim()]));
        await write(path, "PUT", { enabled: Boolean(enabled.field.checked), ...urls, ...key.values(), ...proxy.values() });
        key.reset(); proxy.reset(); await reload();
      }, "刮削设置已保存。");
    }

    function serverConfig(saved, feedback, reload) {
      const enabled = check("enabled", "整理后刷新媒体库", saved.enabled);
      const kind = select("kind", [["emby", "Emby"], ["jellyfin", "Jellyfin"]], saved.kind || "emby");
      const url = input("url", saved.url, { type: "url" });
      const key = secret("api_key", "媒体服务器 API Key", saved.has_api_key);
      return submitForm("保存媒体服务器", [enabled.label, label("服务器类型", kind), label("服务器地址（更换服务器后需重新填写密钥）", url), ...key.nodes], feedback, async () => {
        await write("/api/settings/media-server", "PUT", { enabled: Boolean(enabled.field.checked), kind: kind.value, url: url.value.trim(), ...key.values() });
        key.reset(); await reload();
      }, "媒体服务器设置已保存。");
    }

    function organizerConfig(saved, feedback) {
      const enabled = check("auto_organize", "自动整理入库文件", saved.auto_organize);
      const mode = select("mode", [["hardlink", "硬链接"], ["move", "移动"]], saved.mode);
      const movie = input("movie_template", saved.movie_template, { required: true });
      const episode = input("episode_template", saved.episode_template, { required: true });
      return submitForm("保存整理设置", [enabled.label, label("整理方式", mode), label("电影路径模板", movie), label("剧集路径模板", episode)], feedback, () => write("/api/settings/organizer", "PUT", { auto_organize: Boolean(enabled.field.checked), mode: mode.value, movie_template: movie.value, episode_template: episode.value }), "整理设置已保存。");
    }

    function history(rows, feedback, reload) {
      const states = { pending: "整理中", completed: "已完成", rolling_back: "回滚中", rolled_back: "已回滚" };
      const modes = { hardlink: "硬链接", copy: "复制", move: "移动" };
      return make("div", {}, [button("刷新整理记录", "subtle small-button", reload), !rows.length ? hint("暂无整理记录。") : null, ...rows.map((row) => make("article", { class: "media-record" }, [
        heading(row.target_path), make("p", { text: `来源 ${row.source_path}` }), hint([modes[row.mode] || row.mode, states[row.status] || row.status, row.created_at && new Date(row.created_at * 1000).toLocaleString()].filter(Boolean).join(" · ")),
        row.status === "completed" ? action("回滚整理", feedback, async () => { await write(`/api/organizer/${encodeURIComponent(row.id)}/rollback`, "POST", {}); await reload(); }, "整理已回滚。", `回滚“${row.target_path}”的整理操作？`) : null,
      ]))]);
    }

    function downloaderConfig(clients, feedback, reload) {
      const path = "/api/downloaders";
      function editor(client = {}) {
        const provider = select("provider", providers, client.provider);
        const name = input("name", client.name, { required: true });
        const endpoint = input("endpoint", client.endpoint, { type: "url", required: true });
        const username = secret("username", "下载器用户名", client.has_credentials);
        const password = secret("password", "下载器密码", client.has_credentials);
        const members = check("members_allowed", "允许家庭成员提交任务", client.members_allowed);
        const remote = input("remote_root", client.remote_root), local = input("local_root", client.local_root);
        const edit = client.id != null;
        const target = edit ? `${path}/${encodeURIComponent(client.id)}` : path;
        return make("details", { class: "media-editor" }, [make("summary", { text: edit ? client.name : "新增下载器" }),
          submitForm(edit ? "保存下载器" : "创建下载器", [label("名称", name), label("类型", provider), label("连接地址（Transmission 使用完整 RPC 地址）", endpoint), ...username.nodes, ...password.nodes, members.label, label("下载器文件根目录", remote), label("本地映射根目录", local)], feedback, async () => {
            await write(target, edit ? "PUT" : "POST", { name: name.value.trim(), provider: provider.value, endpoint: endpoint.value.trim(), ...username.values(), ...password.values(), members_allowed: Boolean(members.field.checked), remote_root: remote.value.trim(), local_root: local.value.trim() });
            username.reset(); password.reset(); await reload();
          }, "下载器已保存。"),
          edit ? action("删除下载器", feedback, async () => { await write(target, "DELETE"); await reload(); }, "下载器已删除。", `删除下载器“${client.name}”？`) : null,
        ]);
      }
      return make("div", {}, [...clients.map(editor), editor()]);
    }

    async function settings() {
      if (!admin() || !current()) return [];
      async function panel(title, path, build) {
        const content = make("div"), feedback = status();
        const node = make("section", { class: "panel automation-panel" }, [make("h2", { text: title }), content, feedback]);
        let revision = 0;
        async function load() {
          if (!admin() || !current()) return;
          const request = ++revision;
          try {
            const value = await api(path);
            if (current() && revision === request) content.replaceChildren(build(value, feedback, load));
          } catch (_) {
            if (current() && revision === request) content.replaceChildren(hint(`无法读取${title}。`), button("重试", "subtle", load));
          }
        }
        await load(); return node;
      }
      return Promise.all([
        panel("元数据刮削", "/api/settings/scraper", scraperConfig), panel("Emby / Jellyfin 联动", "/api/settings/media-server", serverConfig),
        panel("文件整理", "/api/settings/organizer", organizerConfig), panel("媒体整理记录", "/api/organizer/history", history), panel("外部下载器", "/api/downloaders", downloaderConfig),
      ]);
    }

    return { detail, rules, wishlist, tasks, settings };
  };
})();
