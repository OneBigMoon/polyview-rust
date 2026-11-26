import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access, mkdtemp, readFile, realpath, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { homedir, tmpdir } from 'node:os';
import { delimiter, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const timeout = 15_000;
const pageChecks = {
  online: ['影视发现', '.media-grid .media-card'],
  cloud: ['网盘搜片', 'input[name="cloud_query"]', '[aria-label="热门榜单"] h2'],
  calendar: ['放送日历', '[aria-label="放送星期"] button[aria-pressed="true"]'],
  library: ['你的媒体库', '.hero h2, .media-grid, .library-stack'],
  live: ['电视直播', '#live-guide .guide-channel-play', '#live-guide .epg-program'],
  favorites: ['我的收藏', '.favorites-section .saved-card'],
  series: ['追剧清单', '.series-list'],
  downloads: ['下载任务', '.download-queue .filters'],
  settings: ['账户与设置', '.settings-panel .panel h2'],
};
const tabChecks = {
  '账户安全': ['修改密码', 'input[name="current"]'],
  '家庭成员': ['家庭成员', '.member-settings-row'],
  '在线影视源': ['在线影视源', 'form'],
  '解析接口': ['解析接口', 'fieldset legend'],
  '网盘与资源频道': ['资源频道订阅管理', '.cloud-settings form'],
  '媒体自动化': ['元数据刮削', '.automation-panel form'],
  '直播与节目单': ['电视节目单', 'form textarea'],
  '通知与日历': ['通知', 'input[name="api_url"]'],
  '备份与工具': ['配置导出 / 导入', 'input[type="file"]'],
  '影视仓订阅': ['影视仓 / TVBox 订阅', '.tvbox-configs input[readonly]'],
};
const sidebar = '.sidebar [data-view]';
const settingsTabs = '#content > div > .settings-tabs > button';

async function executable(file) {
  try { await access(file, constants.X_OK); return true; }
  catch (error) { if (error.code === 'ENOENT' || error.code === 'EACCES') return false; throw error; }
}

async function localPlaywright() {
  const loaders = [createRequire(import.meta.url)];
  // Global CLI installations may be on PATH without being in Node's module search path.
  for (const directory of (process.env.PATH || '').split(delimiter).filter(Boolean)) {
    const cli = join(directory, 'playwright');
    if (await executable(cli)) loaders.push(createRequire(await realpath(cli)));
  }
  for (const load of loaders) {
    for (const name of ['playwright', 'playwright-core', '@playwright/test']) {
      let entry;
      try { entry = load.resolve(name); }
      catch (error) { if (error.code === 'MODULE_NOT_FOUND') continue; throw error; }
      return load(entry).chromium;
    }
  }
}

async function chromePath(chromium) {
  if (process.env.CHROME_BIN) {
    assert(await executable(process.env.CHROME_BIN), 'CHROME_BIN is not executable');
    return process.env.CHROME_BIN;
  }
  const candidates = [chromium?.executablePath()];
  for (const apps of ['/Applications', join(homedir(), 'Applications')]) {
    for (const app of ['Google Chrome', 'Chromium', 'Google Chrome for Testing', 'Google Chrome Canary']) {
      candidates.push(join(apps, `${app}.app`, 'Contents', 'MacOS', app));
    }
  }
  for (const directory of (process.env.PATH || '').split(delimiter).filter(Boolean)) {
    for (const binary of ['chromium', 'chromium-browser', 'google-chrome', 'google-chrome-stable', 'chrome', 'chrome-headless-shell']) {
      candidates.push(join(directory, binary));
    }
  }
  candidates.push('/opt/google/chrome/chrome', '/snap/bin/chromium');
  for (const file of candidates) if (file && await executable(file)) return file;
}

// The page-level CDP session has the same send/on interface as Playwright's CDPSession.
async function localSession(url) {
  assert(typeof WebSocket === 'function', 'Local CDP requires Node with built-in WebSocket support (Node 22+)');
  const socket = new WebSocket(url);
  const pending = new Map(), events = new Map();
  let sequence = 0;
  socket.addEventListener('close', () => {
    for (const request of pending.values()) request.reject(new Error('Chrome closed the CDP connection'));
    pending.clear();
  });
  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(data);
    if (!message.id) { events.get(message.method)?.(message.params); return; }
    const request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id);
    if (message.error) request.reject(new Error(`${request.method}: ${message.error.message}`));
    else request.resolve(message.result);
  });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { socket.close(); reject(new Error('Timed out opening local CDP')); }, timeout);
    socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
    socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Could not connect to local CDP')); }, { once: true });
  });
  return {
    on(method, handler) { events.set(method, handler); },
    send(method, params = {}) {
      const id = ++sequence;
      return new Promise((resolve, reject) => {
        if (socket.readyState !== WebSocket.OPEN) { reject(new Error('Local CDP disconnected')); return; }
        const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)); }, timeout);
        pending.set(id, { method, resolve: (value) => { clearTimeout(timer); resolve(value); },
          reject: (error) => { clearTimeout(timer); reject(error); } });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    close() { socket.close(); },
  };
}

async function withLocalChrome(binary, run) {
  const profile = await mkdtemp(join(tmpdir(), 'polyview-browser-'));
  const chrome = spawn(binary, ['--headless', '--disable-background-networking', '--no-first-run', '--no-default-browser-check',
    ...(process.getuid?.() === 0 ? ['--no-sandbox'] : []), '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0',
    `--user-data-dir=${profile}`, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '', closed = false, session;
  chrome.stderr.on('data', (data) => { stderr = (stderr + data).slice(-3000); });
  chrome.on('error', (error) => { stderr += error.message; });
  const exit = new Promise((resolve) => chrome.once('close', () => { closed = true; resolve(); }));
  try {
    const deadline = Date.now() + timeout;
    while (!closed && Date.now() < deadline) {
      let port;
      try { [port] = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).split('\n'); }
      catch (error) { if (error.code !== 'ENOENT') throw error; }
      if (/^\d+$/.test(port || '')) {
        const response = await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(timeout) });
        assert(response.ok, `Local CDP target list returned ${response.status}`);
        const target = (await response.json()).find((item) => item.type === 'page' && item.url === 'about:blank');
        if (target) { session = await localSession(target.webSocketDebuggerUrl); break; }
      }
      await delay(50);
    }
    assert(session, `Chrome did not create a local page target. ${stderr}`);
    await run(session);
  } finally {
    try { session?.close(); } finally {
      if (!closed) chrome.kill('SIGTERM');
      const kill = setTimeout(() => chrome.kill('SIGKILL'), 2500);
      try { await exit; } finally { clearTimeout(kill); await rm(profile, { recursive: true, force: true, maxRetries: 3 }); }
    }
  }
}

async function exercise(cdp, { baseUrl, username, password }) {
  let step = 'startup';
  const errors = [], pending = new Set();
  let lastActivity = Date.now();
  function report(kind, message) { errors.push(`${step} ${kind}: ${message}`); }
  function checkErrors() { assert.equal(errors.length, 0, errors.join('\n')); }
  cdp.on('Runtime.consoleAPICalled', ({ type, args }) => {
    if (type === 'error' || type === 'assert') report('console', args.map((arg) => arg.description ?? arg.value ?? arg.type).join(' '));
  });
  cdp.on('Runtime.exceptionThrown', ({ exceptionDetails: details }) => report('pageerror', details.exception?.description || details.text));
  cdp.on('Log.entryAdded', ({ entry }) => { if (entry.level === 'error') report(entry.source, entry.text); });
  cdp.on('Runtime.bindingCalled', ({ name, payload }) => { if (name === 'polyviewSmokeCsp') report('CSP', payload); });
  cdp.on('Network.requestWillBeSent', ({ requestId, request }) => {
    if (request.url.startsWith('http:') || request.url.startsWith('https:')) {
      pending.add(requestId); lastActivity = Date.now();
    }
  });
  const finished = ({ requestId }) => { if (pending.delete(requestId)) lastActivity = Date.now(); };
  cdp.on('Network.loadingFinished', finished);
  cdp.on('Network.loadingFailed', (event) => {
    finished(event);
    if (!event.canceled) report('request failed', event.errorText);
  });
  cdp.on('Network.responseReceived', ({ response }) => {
    if (response.status >= 400) report('HTTP', `${response.status} ${new URL(response.url).pathname}`);
  });
  for (const domain of ['Page', 'Runtime', 'Network', 'Log']) await cdp.send(`${domain}.enable`);
  await cdp.send('Runtime.addBinding', { name: 'polyviewSmokeCsp' });
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source:
    "document.addEventListener('securitypolicyviolation', event => polyviewSmokeCsp(event.effectiveDirective + ' ' + event.blockedURI));" });
  // Count the real point draws to distinguish a paused renderer from a hidden animation.
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: `(() => {
    const prototype = window.WebGL2RenderingContext?.prototype;
    if (!prototype) return;
    const draw = prototype.drawArrays;
    window.polyviewSmokeDraws = 0;
    prototype.drawArrays = function(...args) {
      const result = Reflect.apply(draw, this, args);
      if (this.canvas.id === 'bg-canvas') window.polyviewSmokeDraws++;
      return result;
    };
  })()` });

  async function evaluate(fn, ...args) {
    const { result, exceptionDetails } = await cdp.send('Runtime.evaluate', {
      expression: `(${fn.toString()})(...${JSON.stringify(args)})`, returnByValue: true, awaitPromise: true,
      allowUnsafeEvalBlockedByCSP: false,
    });
    assert(!exceptionDetails, `${step}: ${exceptionDetails?.exception?.description || exceptionDetails?.text}`);
    return result.value;
  }
  async function until(description, read) {
    const deadline = Date.now() + timeout;
    do {
      checkErrors();
      const result = await read();
      if (result) return result;
      await delay(50);
    } while (Date.now() < deadline);
    throw new Error(`${step}: timed out waiting for ${description}`);
  }
  async function click(selector, text) {
    const point = await until(`clickable ${text || selector}`, () => evaluate((selector, text) => {
      const element = Array.from(document.querySelectorAll(selector)).find((el) => !text || el.textContent.trim() === text);
      if (!element || element.disabled || !element.getClientRects().length) return;
      element.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' });
      const box = element.getBoundingClientRect();
      const x = box.x + box.width / 2, y = box.y + box.height / 2;
      if (element.contains(document.elementFromPoint(x, y))) return { x, y };
      element.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
    }, selector, text));
    for (const type of ['mousePressed', 'mouseReleased']) {
      await cdp.send('Input.dispatchMouseEvent', { type, ...point, button: 'left', clickCount: 1 });
    }
  }
  async function press(key, code, virtualKey) {
    for (const type of ['rawKeyDown', 'keyUp']) {
      await cdp.send('Input.dispatchKeyEvent', { type, key, code, windowsVirtualKeyCode: virtualKey });
    }
  }
  async function settle() {
    await until('local requests to finish', () => pending.size === 0 && Date.now() - lastActivity >= 100);
    await evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true)))));
    checkErrors();
  }
  async function rendered(page, tab) {
    await until('completed page render', () => evaluate(() => document.querySelector('#content')?.getAttribute('aria-busy') === 'false'));
    await settle();
    const [title, ...selectors] = tab ? tabChecks[tab] : pageChecks[page];
    const problems = await evaluate((page, tab, title, selectors, settingsTabs) => {
      const main = document.querySelector('#content');
      const visible = (el) => el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden';
      const problems = [];
      const view = document.querySelector('.sidebar [aria-current="page"]')?.dataset.view;
      if (view !== page) problems.push(`active page ${view}`);
      if (document.body.dataset.view !== page) problems.push(`background view ${document.body.dataset.view}`);
      if (document.body.classList.contains('nav-open')) problems.push('sidebar is still open');
      const root = tab ? main.querySelector('.settings-panel') : main;
      const headings = root ? Array.from(root.querySelectorAll(tab ? 'h2' : 'h1')).filter(visible).map((el) => el.textContent.trim()) : [];
      if (!headings.includes(title)) problems.push(`expected ${title}, found ${headings.join(', ') || 'no headings'}`);
      if (tab && !Array.from(document.querySelectorAll(settingsTabs)).some((el) => el.getAttribute('aria-pressed') === 'true' && el.textContent.trim() === tab)) problems.push(`inactive tab ${tab}`);
      for (const selector of selectors) {
        if (!root || !Array.from(root.querySelectorAll(selector)).some(visible)) problems.push(`missing visible ${selector}`);
      }
      const toast = document.querySelector('#toast.error');
      if (toast && visible(toast)) problems.push(toast.textContent.trim());
      for (const el of main.querySelectorAll('.form-error, [role="alert"], .source-notice, .hint')) {
        const text = el.textContent.trim();
        if (visible(el) && text && (!el.matches('.hint') || /暂不可用：|无法读取|加载失败/.test(text))) problems.push(text);
      }
      return problems;
    }, page, tab, title, selectors, settingsTabs);
    assert(!problems.length, `${step}: ${problems.join('; ')}`);
  }
  async function checkLayout(width, height, theme) {
    const layout = await evaluate(() => ({ width: innerWidth, height: innerHeight, content: document.documentElement.scrollWidth,
      theme: document.documentElement.dataset.theme }));
    assert.equal(layout.theme, theme, `${step}: expected ${theme} theme`);
    assert.equal(layout.width, width, `${step}: viewport width`);
    assert.equal(layout.height, height, `${step}: viewport height`);
    assert(layout.content <= width, `${step}: ${theme} document width ${layout.content} exceeds ${width}`);
    checkErrors();
  }
  async function checkThemes(width, height) {
    for (const theme of ['dark', 'light']) {
      await click('#theme-toggle');
      await settle();
      await checkLayout(width, height, theme);
    }
  }
  const stillBackdrop = () => evaluate(async () => {
    const frame = () => new Promise(requestAnimationFrame);
    await frame();
    const draws = window.polyviewSmokeDraws;
    await frame(); await frame();
    return draws === window.polyviewSmokeDraws;
  });
  async function checkDisplay(width, height) {
    const backdrop = await until('mounted cinema canvas or CSS fallback', () => evaluate(() => {
      const canvas = document.getElementById('bg-canvas');
      if (!(canvas instanceof HTMLCanvasElement) || canvas.getAttribute('aria-hidden') !== 'true') return;
      if (canvas.hidden && !canvas.getContext('webgl2')) return 'CSS fallback';
      if (!canvas.hidden && canvas.dataset.engine && window.polyviewSmokeDraws > 0) return 'WebGL';
    }));
    const baseFont = await evaluate(() => parseFloat(getComputedStyle(document.documentElement).fontSize));
    await click('#tv-mode-toggle');
    await until('larger TV typography', () => evaluate(size => document.documentElement.dataset.tv === '1' &&
      parseFloat(getComputedStyle(document.documentElement).fontSize) >= size * 1.15, baseFont));
    await press('Tab', 'Tab', 9);
    assert(await evaluate(() => document.activeElement.matches(':focus-visible') &&
      parseFloat(getComputedStyle(document.activeElement).outlineWidth) >= 3), 'Missing TV keyboard focus ring');
    await checkLayout(width, height, 'light');
    for (const mode of ['soft', 'static']) {
      await click('#bg-mode-toggle');
      await until(`${mode} motion setting`, () => evaluate(mode => document.documentElement.dataset.bgMode === mode, mode));
    }
    if (backdrop === 'WebGL') assert(await stillBackdrop(), 'Static motion kept drawing');
    await cdp.send('Page.reload');
    await until('persisted display controls on discovery', () => evaluate(() =>
      document.body.dataset.view === 'online' && document.getElementById('tv-mode-toggle')?.getAttribute('aria-pressed') === 'true' &&
      document.documentElement.dataset.bgMode === 'static'));
    await rendered('online');
    await click('#tv-mode-toggle');
    await click('#bg-mode-toggle');
    await until('standard layout and motion', () => evaluate(size => document.documentElement.dataset.bgMode === 'standard' &&
      parseFloat(getComputedStyle(document.documentElement).fontSize) === size, baseFont));
    if (backdrop === 'WebGL') {
      await cdp.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] });
      await until('reduced motion frame to settle', stillBackdrop);
      await cdp.send('Emulation.setEmulatedMedia', { features: [] });
      const previous = await evaluate(() => window.polyviewSmokeDraws);
      await until('background animation to resume', () => evaluate(n => window.polyviewSmokeDraws > n, previous));
    }
    return backdrop;
  }
  async function checkLive(width) {
    await click('#content button', '显示全部频道（含 1 个无节目单）');
    await rendered('live');
    const program = await evaluate(() => {
      const button = document.querySelector('.epg-program.current');
      return button && [button.parentElement.localName, button.getBoundingClientRect().width, button.getBoundingClientRect().height];
    });
    assert(program && program[0] === 'foreignObject' && program[1] > 0 && program[2] > 0, 'Current program has no timeline area');
    await click('.guide-channel-play[aria-label="播放 Smoke News"]');
    const playing = title => until(`${title} decoding`, () => evaluate(title => {
      const video = document.getElementById('video-player');
      return document.getElementById('player-title').textContent === title && video.videoWidth === 32 && video.videoHeight === 24 &&
        video.readyState >= 2 && !video.paused && document.body.dataset.playing === '1' && document.getElementById('bg-canvas').hidden;
    }, title));
    await playing('Smoke News');
    assert(await stillBackdrop(), 'Background continued drawing during live playback');
    assert(await evaluate(width => {
      const video = document.querySelector('#video-player').getBoundingClientRect();
      const guide = document.querySelector('#player-guide');
      const rect = guide.getBoundingClientRect();
      return !guide.hidden && guide.textContent.includes('Local News Hour') && (width === 390 ? rect.top >= video.bottom - 1 : rect.left >= video.right - 1);
    }, width), 'Guide did not align with the decoded video');
    const source = await evaluate(() => document.querySelector('#video-player').currentSrc);
    await press('ArrowRight', 'ArrowRight', 39);
    await until('second news line', () => evaluate(source => document.querySelector('#player-source').textContent.includes('2/2') &&
      document.querySelector('#video-player').currentSrc !== source, source));
    await playing('Smoke News');
    await press('ArrowLeft', 'ArrowLeft', 37);
    await until('first news line', () => evaluate(source => document.querySelector('#video-player').currentSrc === source, source));
    await press('m', 'KeyM', 77);
    await until('all channels in drawer', () => evaluate(() => !document.querySelector('#player-channel-drawer').hidden &&
      document.querySelectorAll('.player-channel-row').length === 2 && document.querySelector('.player-channel-row[aria-current] strong')?.textContent === 'Smoke News'));
    await click('#player-channel-groups button', '★ 收藏');
    await until('only the favorite station', () => evaluate(() => document.querySelectorAll('.player-channel-row').length === 1 &&
      document.querySelector('.player-channel-row strong')?.textContent === 'Smoke News'));
    await press('Escape', 'Escape', 27);
    const direction = await evaluate(() => document.querySelector('#player-next').hidden ? 'ArrowUp' : 'ArrowDown');
    await press(direction, direction, direction === 'ArrowUp' ? 38 : 40);
    await playing('Smoke Culture');
    await press('Enter', 'Enter', 13);
    await until('drawer opened with Enter', () => evaluate(() => !document.querySelector('#player-channel-drawer').hidden));
    await press('Escape', 'Escape', 27);
    await click('#player-close');
    await until('player dialog to close', () => evaluate(() => !document.querySelector('#player-dialog').open));
    await until('background to resume after playback', () => evaluate(() => document.body.dataset.playing === '0' &&
      (!document.querySelector('#bg-canvas').dataset.engine || !document.querySelector('#bg-canvas').hidden)));
  }
  async function checkQr() {
    for (const mode of ['standard', 'base58']) {
      if (mode === 'base58') await click('.tvbox-qr [data-mode="base58"]');
      await until(`${mode} QR canvas`, () => evaluate(mode => document.querySelector(`.tvbox-qr [data-mode="${mode}"]`)?.getAttribute('aria-pressed') === 'true' &&
        document.querySelector('.tvbox-qr canvas:not([hidden])')?.width > 0, mode));
      const matches = await evaluate(async mode => {
        const { qr, url } = await (await fetch(`/api/tvbox/qr?mode=${mode}`)).json();
        const link = document.querySelectorAll('.tvbox-configs input')[mode === 'standard' ? 0 : 1];
        const canvas = document.querySelector('.tvbox-qr canvas');
        const scale = canvas.width / (qr.length + 8);
        if (!Number.isInteger(scale) || scale < 1 || link.value !== url || canvas.width !== canvas.height) return false;
        const rgba = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
        for (let y = 0; y < qr.length + 8; y++) for (let x = 0; x < qr.length + 8; x++) {
          const offset = 4 * ((y * scale + Math.floor(scale / 2)) * canvas.width + x * scale + Math.floor(scale / 2));
          if (rgba[offset] !== (qr[y - 4]?.[x - 4] === '1' ? 0 : 255) || rgba[offset + 3] !== 255) return false;
        }
        return true;
      }, mode);
      assert(matches, `${mode} QR pixels differ from the active subscription matrix or its quiet zone`);
    }
  }
  try {
    for (const [width, height] of [[1440, 900], [390, 844]]) {
      await cdp.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: width === 390 });
      step = `${width}×${height} startup`;
      const navigation = await cdp.send('Page.navigate', { url: baseUrl });
      assert(!navigation.errorText, navigation.errorText);
      if (width === 1440) {
        await until('login form', () => evaluate(() => document.querySelector('#auth-form[data-setup="0"]')?.getClientRects().length));
        for (const [selector, text] of [['#auth-username', username], ['#auth-password', password]]) {
          await click(selector);
          await cdp.send('Input.insertText', { text });
        }
        await click('#auth-submit');
      }
      await until('authenticated admin shell', () => evaluate((name) => !document.querySelector('#app-shell')?.hidden &&
        document.querySelector('#profile-name')?.textContent === name && document.querySelector('#profile-role')?.textContent === '管理员', username));
      await rendered('online');
      const pages = await evaluate((selector) => Array.from(document.querySelectorAll(selector), (el) => el.dataset.view), sidebar);
      assert.deepEqual([...pages].sort(), Object.keys(pageChecks).sort(), `${step}: sidebar inventory`);
      assert.deepEqual(await evaluate(() => [...document.querySelectorAll('.nav-scroll .nav-label')].map(el => el.textContent)), ['探索与观看', '我的片库', '系统']);
      const mobile = await evaluate(() => [...document.querySelectorAll('.mobile-tabs button')].filter(el => el.getClientRects().length).map(el => el.textContent.trim()));
      assert.deepEqual(mobile, width === 390 ? ['发现', '直播', '媒体', '网盘', '更多'] : []);
      const backdrop = await checkDisplay(width, height);
      for (const page of pages) {
        step = `${width}×${height} ${page}`;
        if (width === 390 && ['online', 'live', 'library', 'cloud'].includes(page)) await click(`.mobile-tab[data-view="${page}"]`);
        else {
          if (width === 390) await click('#mobile-more');
          await click(`${sidebar}[data-view="${page}"]`);
        }
        await rendered(page);
        await checkThemes(width, height);
        if (page === 'live') await checkLive(width);
        if (page !== 'settings') continue;
        const tabs = await evaluate((selector) => Array.from(document.querySelectorAll(selector), (el) => el.textContent.trim()), settingsTabs);
        assert.deepEqual([...tabs].sort(), Object.keys(tabChecks).sort(), `${step}: admin tab inventory`);
        for (const tab of tabs) {
          step = `${width}×${height} settings / ${tab}`;
          await click(settingsTabs, tab);
          await rendered(page, tab);
          await checkThemes(width, height);
          if (tab === '影视仓订阅') await checkQr();
        }
      }
      console.log(`Browser ${width}×${height}: ${pages.length} pages, ${Object.keys(tabChecks).length} admin tabs, both themes, ${backdrop}, TV persistence, decoded live switching and both QR canvases`);
    }
    await settle();
  } catch (error) {
    throw new Error(`${step}: ${error.message}`, { cause: error });
  }
}

export async function runBrowserSmoke({ baseUrl, username, password }) {
  const origin = new URL(baseUrl);
  assert(['localhost', '127.0.0.1', '[::1]'].includes(origin.hostname) && origin.protocol === 'http:', 'Browser smoke requires a loopback HTTP service');
  const chromium = await localPlaywright();
  const binary = await chromePath(chromium);
  const run = (session) => exercise(session, { baseUrl: origin.href, username, password });
  if (chromium) {
    let browser;
    try {
      browser = await chromium.launch({ ...(binary && { executablePath: binary }), headless: true });
    } catch (error) {
      // A Playwright package can be installed without its optional browser download.
      if (binary || !/Executable doesn't exist at/.test(error.message)) throw error;
    }
    if (browser) {
      try {
        const page = await browser.newPage();
        await run(await page.context().newCDPSession(page));
      } finally { await browser.close(); }
      return { skipped: false };
    }
  }
  if (binary) { await withLocalChrome(binary, run); return { skipped: false }; }
  const reason = 'No installed Playwright Chromium or local Chrome/Chromium executable found';
  if (process.env.POLYVIEW_REQUIRE_BROWSER === '1') throw new Error(reason);
  return { skipped: true, reason };
}
