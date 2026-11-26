#!/usr/bin/env node
// Build with cargo build --locked --release, then run this file. POLYVIEW_BIN
// selects another build. POLYVIEW_REQUIRE_BROWSER=1 makes browser coverage mandatory.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual, stripVTControlCharacters } from "node:util";
import { runBrowserSmoke } from "./e2e-browser.mjs";

const title = "冒烟测试剧集";
const mediaFile = "Smoke Library.mp4";
// A 32×24 VP8 WebM, three ten-second frames of solid color. Browser smoke needs no encoder.
const liveVideo = Buffer.from(
  "GkXfo59ChoEBQveBAULygQRC84EIQoKEd2VibUKHgQJChYECGFOAZwH/////////EU2bdKtNu4tTq4QVSalmU6yBoU" +
  "27i1OrhBZUrmtTrIHGTbuMU6uEElTDZ1OsggEU7AEAAAAAAABoAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
  "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
  "AAAAAAAAAVSalmoCrXsYMPQkBNgIRMYXZmV0GETGF2ZkSJiEDdTAAAAAAAFlSua8muAQAAAAAAAEDXgQFzxYgAAAAA" +
  "AAAAAZyBACK1nIN1bmSIgQCGhVZfVlA4g4EBI+ODhQJUC+QA4JCwgSC6gRiagQJVsIRVuYEBElTDZ6xzc6ljwItjxY" +
  "gAAAAAAAAAAWfImEWjh0VOQ09ERVJEh4tMYXZjIGxpYnZweB9DtnW654EAo7WBAACAcAIAnQEqIAAYAAJHCIWFiJmE" +
  "iAyCAnTyxmZi2AD++0KD/0JP/+CT//gk/dPxhNJHYB9DtnW154InEKOvgQAAABECAAkQEAAYB2AQP/LmmYCL4gD+/4" +
  "3k//2eX/7PL/9nl/9nl/9x2Buu8QAfQ7Z1s+eCTiCjrYEAAADRAQAJEBAAGAAeV/QMAEEOAP7/jeT//Z5f/s8v/2eX" +
  "/2eX/3HYG67xAA==",
  "base64",
);
const binary = process.env.POLYVIEW_BIN ? resolve(process.env.POLYVIEW_BIN)
  : fileURLToPath(new URL("../target/release/polyview", import.meta.url));

function mixedPlaylist(origin) {
  return ["#EXTM3U", '#EXTINF:-1 tvg-id="smoke.news" group-title="Smoke",Smoke News',
    `${origin}/news.webm`, '#EXTINF:-1 tvg-id="smoke.news" group-title="Smoke",Smoke News',
    `${origin}/news.webm?line=2`, "Smoke,#genre#", `Smoke Culture,${origin}/culture.webm`, ""].join("\n");
}

function xmltvTime(ms) {
  return new Date(ms).toISOString().slice(0, 19).replace(/[-:T]/g, "") + " +0000";
}

function makeFixture() {
  // Also act as a rejecting proxy for any accidental non-loopback provider request.
  const unexpected = new Set();
  const server = createServer((req, res) => {
    const origin = `http://127.0.0.1:${server.address().port}`;
    const url = new URL(req.url, origin);
    const send = (type, body) => res.writeHead(200, { "Content-Type": type }).end(body);
    const json = value => send("application/json", JSON.stringify(value));
    if (url.origin === origin) {
      switch (url.pathname) {
        case "/cms-a":
        case "/cms-b": {
          const id = url.pathname === "/cms-a" ? 41 : 42;
          const row = { vod_id: id, vod_name: title, vod_year: "2026", type_id: 2,
            type_name: "国产剧", vod_play_from: "Direct", vod_play_url: `第一集$${origin}/1.mp4#第二集$${origin}/2.mp4` };
          return json({ code: 1, page: 1, pagecount: 1, total: 1, class: [{ type_id: 2, type_name: "国产剧" }],
            list: !url.searchParams.has("ids") || url.searchParams.get("ids") === String(id) ? [row] : [] });
        }
        case "/1.mp4":
        case "/2.mp4": return send("video/mp4", `Fixture episode ${url.pathname[1]}`);
        case "/channels.m3u": return send("audio/x-mpegurl", mixedPlaylist(origin));
        case "/news.webm":
        case "/culture.webm": return send("video/webm", liveVideo);
        case "/guide.xml": {
          const now = Date.now();
          return send("application/xml", `<tv><channel id="smoke.news"><display-name>Smoke News</display-name></channel>` +
            `<programme channel="smoke.news" start="${xmltvTime(now - 3600000)}" stop="${xmltvTime(now + 3600000)}">` +
            `<title>Local News Hour</title></programme></tv>`);
        }
        case "/calendar": return json([{ weekday: { id: 1 }, items: [{ id: 41, name: title }] }]);
        case "/movies": return json({ subjects: [{ title: "Fixture Film" }] });
        case "/series": return json({ subjects: [{ title }] });
      }
    }
    unexpected.add(url.origin === origin ? url.pathname : "external HTTP request");
    res.writeHead(404).end();
  });
  server.on("connect", (_req, socket) => {
    unexpected.add("external HTTPS request");
    socket.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
  });
  return { server, unexpected };
}

function startService(directory, proxy) {
  const child = spawn(binary, [], {
    cwd: directory,
    // Do not inherit the operator's account, media, legacy import or provider settings.
    env: { PATH: process.env.PATH, HOME: directory, TMPDIR: directory, RUST_LOG: "polyview=info",
      POLYVIEW_BIND: "127.0.0.1:0", POLYVIEW_DATA_DIR: join(directory, "data"),
      POLYVIEW_MEDIA_DIR: join(directory, "media"), HTTP_PROXY: proxy, HTTPS_PROXY: proxy, NO_PROXY: "127.0.0.1" },
    stdio: ["ignore", "pipe", "ignore"],
  });
  const closed = new Promise(resolve => child.once("close", (code, signal) => resolve(signal || code)));
  const lines = createInterface({ input: child.stdout });
  const ready = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Timed out waiting for PolyView startup")), 20000);
    child.once("error", error => reject(new Error(`Could not start PolyView (${error.code})`)));
    lines.on("line", line => {
      const text = stripVTControlCharacters(line);
      if (!text.includes("PolyView is ready")) return;
      const address = text.match(/\b127\.0\.0\.1:(\d+)\b/);
      if (address) {
        clearTimeout(timer);
        resolve(`http://${address[0]}`);
      }
    });
    closed.then(status => {
      clearTimeout(timer);
      reject(new Error(`PolyView exited during startup (${status})`));
    });
  });
  async function stop() {
    child.kill("SIGTERM");
    const timer = setTimeout(() => child.kill("SIGKILL"), 3000);
    try { await closed; } finally { clearTimeout(timer); }
  }
  return { ready, stop };
}

function client(baseUrl) {
  let session = "";
  async function request(method, path, { body, status = 200, auth = true, csrf = true } = {}) {
    const url = new URL(path, baseUrl);
    assert.equal(url.origin, baseUrl, "Unexpected off-site API or feed URL");
    const headers = {};
    if (auth && session) headers.Cookie = session;
    if (csrf && method !== "GET") headers["X-PolyView-Request"] = "1";
    if (body !== undefined) headers["Content-Type"] = "application/json";
    const response = await fetch(url, { method, headers, redirect: "error", body: JSON.stringify(body), signal: AbortSignal.timeout(20000) });
    // Feed URLs contain member access tokens. Never include them or response bodies in diagnostics.
    const label = url.pathname.startsWith("/tvbox/") ? "TVBox feed" : url.pathname;
    assert.equal(response.status, status, `${method} ${label} returned ${response.status}, expected ${status}`);
    return response;
  }
  async function json(method, path, options) {
    const response = await request(method, path, options);
    try { return await response.json(); } catch { throw new Error("Expected a JSON response from PolyView"); }
  }
  function useSession(response) {
    const cookie = response.headers.get("set-cookie") || "";
    assert.ok(/^polyview_session=[^;\s]+;/.test(cookie) && /; HttpOnly/i.test(cookie), "Expected an HTTP-only session cookie");
    session = cookie.split(";", 1)[0];
  }
  return { request, json, useSession };
}

async function authenticate(api, credentials) {
  assert.deepEqual(await api.json("GET", "/healthz"), { status: "ok" });
  assert.deepEqual(await api.json("GET", "/api/auth/status"), { needs_setup: true, user: null });
  await api.json("GET", "/api/settings/export", { status: 401 });
  const setup = await api.request("POST", "/api/auth/setup", { body: credentials });
  assert.equal((await setup.json()).role, "admin");
  api.useSession(setup);
  await api.request("POST", "/api/auth/logout");
  const login = await api.request("POST", "/api/auth/login", { auth: false, body: credentials });
  api.useSession(login);
  const { user, needs_setup } = await api.json("GET", "/api/auth/status");
  assert.ok(!needs_setup && user?.username === credentials.username && user.role === "admin", "Login did not establish an administrator session");
  const body = { name: "Rejected", url: "http://127.0.0.1/rejected" };
  await api.json("POST", "/api/sources", { body, auth: false, status: 401 });
  await api.json("POST", "/api/sources", { body, csrf: false, status: 403 });
  assert.deepEqual(await api.json("GET", "/api/sources"), []);
}

async function configureProviders(api, origin) {
  const backup = await api.json("GET", "/api/settings/export");
  backup.app_settings.calendar = { enabled: true, api_url: `${origin}/calendar` };
  backup.app_settings.douban_movie_url = `${origin}/movies`;
  backup.app_settings.douban_series_url = `${origin}/series`;
  backup.epg_config.url = `${origin}/guide.xml`;
  assert.deepEqual(await api.json("POST", "/api/settings/import", { body: backup }), { ok: true });
  const recommendations = await api.json("GET", "/api/catalog/recommend");
  assert.deepEqual(recommendations.groups.map(group => group.items[0].title), ["Fixture Film", title]);
  const calendar = await api.json("GET", "/api/calendar");
  assert.equal(calendar.days[0].items[0].title_cn, title);
}

async function catalogAndProgress(api, origin) {
  const sourceIds = [];
  for (const suffix of ["a", "b"]) {
    const source = await api.json("POST", "/api/sources", { status: 201, body: { name: `Smoke ${suffix}`, url: `${origin}/cms-${suffix}` } });
    assert.ok(source.enabled && Number.isInteger(source.id), "Expected an enabled CMS source");
    sourceIds.push(source.id);
  }
  const discovery = await api.json("GET", "/api/catalog/discover?page=1");
  assert.deepEqual(discovery.errors, []);
  assert.deepEqual(discovery.results.map(row => row.source_id).sort((a, b) => a - b), sourceIds);
  assert.ok(discovery.results.every(row => row.title === title), "Discovery did not aggregate the shared title");
  const source = sourceIds[0];
  const detail = await api.json("GET", `/api/catalog/${source}/detail?id=41`);
  assert.equal(detail.title, title);
  assert.equal(detail.source_id, source);
  const episodes = detail.groups[0].episodes;
  assert.deepEqual(episodes.map(item => item.url), [`${origin}/1.mp4`, `${origin}/2.mp4`]);
  const favorite = { source_id: source, media_id: detail.id, title };
  const progress = { ...favorite, episode_index: 1, episode_name: episodes[1].name, remote_url: episodes[1].url, position: 42, duration: 120 };
  assert.deepEqual(await api.json("PUT", "/api/progress", { body: progress }), { saved: true });
  const rows = await api.json("GET", `/api/progress?source_id=${source}&media_id=41`);
  assert.equal(rows.length, 1);
  for (const [key, value] of Object.entries(progress)) assert.equal(rows[0][key], value, `Saved progress field ${key}`);
  assert.deepEqual(await api.json("POST", "/api/favorites", { body: favorite }), { favorite: true });
  return source;
}

async function liveAndFavorites(api, origin, sourceId) {
  const imported = await api.json("POST", "/api/channels/import", { body: { playlist: mixedPlaylist(origin) } });
  assert.deepEqual(imported, { imported: 3, new_channels: 2 });
  const subscription = await api.json("POST", "/api/live-sources", { status: 201, body: { name: "Smoke TV", url: `${origin}/channels.m3u` } });
  assert.equal(subscription.enabled, true);
  assert.deepEqual(await api.json("POST", "/api/live-sources/refresh"), { refreshed: 1, failed: 0 });
  const probe = await api.json("POST", "/api/channels/probe", { body: { group: "Smoke" } });
  assert.deepEqual(probe, { checked: 3, healthy: 3, failed: 0 });
  const channels = await api.json("GET", "/api/channels");
  assert.deepEqual(channels.map(item => item.name).sort(), ["Smoke Culture", "Smoke News"]);
  assert.deepEqual(channels.map(channel => channel.sources.length).sort(), [1, 2]);
  assert.ok(channels.every(channel => channel.sources.every(line => line.last_ok_at > 0)), "Expected probe results for all three lines");
  const news = channels.find(item => item.name === "Smoke News");
  const refresh = await api.json("POST", "/api/epg/refresh");
  assert.ok(refresh.matched_channels === 1 && refresh.programs === 1 && !refresh.errors.length, "Expected a fresh XMLTV match");
  const guide = await api.json("GET", "/api/epg");
  assert.ok(guide.programs.some(p => p.title === "Local News Hour" && p.channel_id === news.id), "Missing program on its imported channel");
  const local = (await api.json("GET", "/api/library")).find(item => item.relative_path === mediaFile);
  assert.ok(local, "Startup scan missed the temporary media file");
  for (const path of [`/api/library/${local.id}/favorite`, `/api/channels/${news.id}/favorite`]) {
    assert.deepEqual(await api.json("PUT", path, { body: { favorite: true } }), { favorite: true });
  }
  const favorites = await api.json("GET", "/api/favorites");
  assert.deepEqual(favorites.catalog.map(item => [item.source_id, item.media_id]), [[sourceId, "41"]]);
  assert.deepEqual(favorites.channels.map(item => item.id), [news.id]);
  assert.deepEqual(favorites.local.map(item => item.id), [local.id]);
}

function decodeBase58(encoded) {
  const alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  assert.ok(encoded.length > 0, "Empty Base58 export");
  let value = 0n;
  for (const char of encoded) {
    const digit = alphabet.indexOf(char);
    assert.ok(digit >= 0, "Invalid Base58 character");
    value = value * 58n + BigInt(digit);
  }
  const bytes = [];
  while (value > 0n) {
    bytes.push(Number(value % 256n));
    value /= 256n;
  }
  for (const char of encoded) {
    if (char !== "1") break;
    bytes.push(0);
  }
  return Buffer.from(bytes.reverse());
}

async function tvbox(api, baseUrl) {
  const feed = await api.json("GET", "/api/tvbox");
  const standard = await api.request("GET", feed.config_path, { auth: false });
  const bytes = Buffer.from(await standard.arrayBuffer());
  let config;
  try { config = JSON.parse(bytes.toString("utf8")); }
  catch { throw new Error("Standard TVBox export was not valid JSON"); }
  assert.deepEqual(config.sites.map(site => site.name).sort(), ["PolyView 媒体库", "Smoke a", "Smoke b"]);
  assert.ok(config.lives[0].url === baseUrl + feed.live_path, "Incorrect TVBox live feed URL");
  const encoded = await api.request("GET", feed.config_b58_path, { auth: false });
  assert.ok(decodeBase58(await encoded.text()).equals(bytes), "Base58 export differs from the standard configuration");
  const playlist = await (await api.request("GET", feed.live_path, { auth: false })).text();
  assert.ok(playlist.startsWith("#EXTM3U") && playlist.includes("Smoke News") && playlist.includes("Smoke Culture"), "TVBox playlist omitted imported channels");
}

async function backupRestore(api, origin) {
  const backup = await api.json("GET", "/api/settings/export");
  assert.equal(backup.version, 1);
  assert.deepEqual(backup.catalog_sources.map(item => item.url).sort(), [`${origin}/cms-a`, `${origin}/cms-b`]);
  assert.deepEqual(backup.live_channels.map(item => item.name).sort(), ["Smoke Culture", "Smoke News"]);
  assert.equal(backup.live_sources[0].url, `${origin}/channels.m3u`);
  assert.equal(backup.epg_config.url, `${origin}/guide.xml`);
  const calendar = backup.app_settings.calendar;
  const changed = await api.json("PUT", "/api/settings/calendar", { body: { ...calendar, enabled: false } });
  assert.equal(changed.enabled, false);
  assert.deepEqual(await api.json("POST", "/api/settings/import", { body: backup }), { ok: true });
  assert.deepEqual(await api.json("GET", "/api/settings/calendar"), calendar);
  const restored = await api.json("GET", "/api/settings/export");
  delete restored.exported_at;
  delete backup.exported_at;
  assert.ok(isDeepStrictEqual(restored, backup), "Restored site configuration differs from its export");
}

async function main() {
  try { await access(binary, constants.X_OK); } catch { throw new Error("Missing executable PolyView build. Run cargo build --locked --release or set POLYVIEW_BIN."); }
  const directory = await mkdtemp(join(tmpdir(), "polyview-e2e-"));
  const { server, unexpected } = makeFixture();
  let service;
  let interrupted = false;
  const interrupt = () => { interrupted = true; service?.stop(); };
  process.once("SIGINT", interrupt);
  process.once("SIGTERM", interrupt);
  try {
    await mkdir(join(directory, "media"));
    await writeFile(join(directory, "media", mediaFile), "smoke library fixture");
    await new Promise((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
    const origin = `http://127.0.0.1:${server.address().port}`;
    assert.ok(!interrupted, "Smoke run interrupted");
    service = startService(directory, origin);
    const baseUrl = await service.ready;
    const api = client(baseUrl);
    const credentials = { username: "smoke_admin", password: randomBytes(24).toString("hex") };
    await authenticate(api, credentials);
    await configureProviders(api, origin);
    const sourceId = await catalogAndProgress(api, origin);
    await liveAndFavorites(api, origin, sourceId);
    await tvbox(api, baseUrl);
    await backupRestore(api, origin);
    const browser = await runBrowserSmoke({ baseUrl, ...credentials });
    console.log(browser.skipped ? `Browser smoke SKIPPED (${browser.reason})` : "Browser smoke passed");
    const logout = await api.request("POST", "/api/auth/logout");
    assert.ok(/Max-Age=0/i.test(logout.headers.get("set-cookie") || ""), "Logout did not expire the session cookie");
    // Deliberately replay the old cookie to verify server-side revocation.
    await api.json("GET", "/api/favorites", { status: 401 });
    assert.deepEqual(await api.json("GET", "/api/auth/status"), { needs_setup: false, user: null });
    assert.equal(unexpected.size, 0, "Unexpected outbound request or unhandled fixture route");
    assert.ok(!interrupted, "Smoke run interrupted");
  } finally {
    try {
      await service?.stop();
    } finally {
      if (server.listening) {
        const closed = new Promise(resolve => server.close(resolve));
        server.closeAllConnections();
        await closed;
      }
      await rm(directory, { recursive: true, force: true });
      process.off("SIGINT", interrupt);
      process.off("SIGTERM", interrupt);
    }
  }
  console.log("HTTP smoke passed (authentication, catalog, progress, favorites, live, EPG, TVBox, backup/restore, logout)");
}

main().catch(error => {
  console.error(`Smoke failed: ${error.message}`);
  process.exitCode = 1;
});
