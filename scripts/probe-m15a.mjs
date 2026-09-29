#!/usr/bin/env node

// M1.5a step 6.1 probe runner (DESIGN.md §12.2). It finds out what real
// Chrome delivers to a worker with Brauser's permissions, so steps 6.3 and 6.4
// build on observed facts. Everything runs in headless Chrome (--headless=new)
// on a fresh temporary profile, over the DevTools pipe; nothing touches the
// screen, keyboard focus, clipboard, or an everyday Chrome profile. Chrome is
// killed when the run ends. The CDP, fixture-server, and cleanup code is copied
// from scripts/smoke-macos.mjs rather than imported, because that script runs
// its whole flow on import.
//
//   node scripts/probe-m15a.mjs [--only <name>[,<name>]] [--json <file>] [--keep-profile]
//
// Each scripts/probes/*.mjs module exports { name, title, async run(ctx) } and
// returns findings: [{ claim, expected, observed, matches, inconclusive?, note?, ...extra }].
// inconclusive: true marks a finding whose observation matched but cannot
// settle the claim; it prints as "inconclusive" and does not fail the run.
// Extra fields such as an event slice go into the JSON report as they are.
// The runner prints a table and exits 1 when a finding does not match, a probe
// throws, or the harness fails. Before each probe it closes every tab except
// the home and control tabs and clears the recorder log.
//
// The recorder (scripts/probes/recorder-extension) logs every webNavigation,
// tabs, windows, idle, and runtime event it receives. Its records are
// { seq, bootId, at, kind, details, extra }. kind is the event name, such as
// "onCommitted" or "tabs.onReplaced", and details is the payload Chrome gave.
// The worker's own records use kind "worker.boot". extra.sourceMapped on
// onCreatedNavigationTarget is the recorder's tab map entry for sourceTabId at
// that moment.
//
// ctx API
//   Browser and run state (the getters follow a relaunch):
//     ctx.cdp                      current Cdp: send(method, params?, sessionId?, timeout?),
//                                  targets(), events, onEvent(listener) -> unsubscribe
//     ctx.chrome                   current Chrome child process
//     ctx.chromeVersion            Browser.getVersion reply
//     ctx.runDir, ctx.profile      temp run directory and its chrome-profile
//     ctx.extensionId              recorder ID (the same after a relaunch)
//     ctx.homeTargetId             a plain tab the runner keeps open (about:blank at start)
//     ctx.controlTargetId          the recorder's control.html tab
//     ctx.signal                   AbortSignal raised by SIGINT/SIGTERM
//   Fixture (node:http on 127.0.0.1; every response is Cache-Control: no-store):
//     ctx.origin                   http://127.0.0.1:<port>, granted to the recorder
//     ctx.otherOrigin              http://localhost:<port>, the same server, not granted
//     ctx.urls.home                <origin>/
//     ctx.urls.list                link page; see fixtureServer for element IDs
//     ctx.urls.listTo(path)        the link page with every link and window.open aimed at path
//     ctx.urls.board               board page that pushStates ?selectedIssue=KEY-1, then KEY-2
//     ctx.urls.page(n)             <origin>/page?n=<n>, a plain page titled "Page <n>"
//     ctx.urls.browse(key)         <origin>/browse/<key>, an artifact-like issue page
//     ctx.urls.slow(ms, n)         <origin>/slow?d=<ms>&n=<n>, answered after ms
//     ctx.urls.other(path)         path on the ungranted localhost origin
//     ctx.fixture.hold(match?)     park matching requests (default: all) until release()
//     ctx.fixture.release()        answer every parked request and stop holding; returns the count
//     ctx.fixture.heldCount()      requests parked now
//     ctx.fixture.requests         [{ at, method, url }] for every request received
//   Tabs and pages (CDP targets):
//     ctx.openTab(url, { background = true, newWindow = false, wait = true }) -> targetId
//     ctx.pageTargets()            -> page target infos
//     ctx.findTarget(urlOrPredicate, { timeout = 8000 }) -> target info; waits for it
//     ctx.activate(targetId)       Target.activateTarget
//     ctx.closeTab(targetId)       close, then wait until the target is gone
//     ctx.attach(targetId)         -> PageDriver; call page.detach() when done
//     ctx.withPage(targetId, async (page) => ...) attach for one action, then detach
//     ctx.navigate(targetId, url, { transitionType = "typed", wait = true })
//     PageDriver: sessionId, send(method, params?), evaluate(expr, { userGesture, timeout }),
//       point(selector) -> {x, y}, click(selector, { button = "left", modifiers = 0 }),
//       navigate(url, { transitionType, wait }), waitForLoad(url?, { timeout }), detach()
//   Recorder:
//     ctx.inWorker(expr, { timeout = 4000, retries = 3, wait = 12000 }) evaluate in the worker
//                                  (probe.* helpers are in recorder-extension/worker.js); detaches after
//     ctx.control(expr, { userGesture = false }) evaluate in the control tab
//     ctx.tabs()                   chrome.tabs.query({}) summaries (url/title only on granted origins)
//     ctx.tabIdFor(url, { timeout = 8000 }) -> tab id whose url equals url
//     ctx.readLog({ since, kinds, bootId }) -> records; flushes the worker if it runs
//     ctx.clearLog()
//     ctx.waitForEvent(predicate, { timeout = 8000, since }) -> the first matching record
//     ctx.brief(record)            one-line summary of a record
//     ctx.workerTarget()           -> the recorder service_worker target, or null
//     ctx.waitForWorker({ timeout = 12000 }) -> target
//     ctx.workerBootId()           -> bootId of the running worker
//     ctx.stopWorker()             -> { bootId, method }; bootId is the stopped worker's
//     ctx.waitForRestart(bootId, { timeout = 12000 }) -> a new worker's bootId
//     ctx.wakeWorker()             runtime.sendMessage from the control tab (not logged) -> bootId
//     ctx.grantOrigin(pattern)     developerPrivate.addHostPermission plus permissions.request
//     ctx.loadExtension(dir)       copy dir into runDir and load it unpacked -> { id, dir }
//     ctx.extensionsInfo()         developerPrivate.getExtensionsInfo summaries
//   Browser lifecycle:
//     ctx.relaunch({ restoreSession = false, args = [], beforeLaunch }) close Chrome gracefully
//                                  (so the session is saved), run beforeLaunch, relaunch on the
//                                  same profile, and reload the recorder, which
//                                  Extensions.loadUnpacked loads for one browser session only.
//                                  Other loaded extensions must be reloaded by the probe.
//                                  -> { pageTargets } as seen before the reload
//   Helpers: ctx.delay(ms), ctx.waitFor(check, { timeout, interval }),
//     ctx.requireCondition(condition, message), ctx.log(...parts)
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { access, cp, mkdir, mkdtemp, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath, pathToFileURL } from "node:url";

const REPO = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const PROBES = path.join(REPO, "scripts", "probes");
const RECORDER = path.join(PROBES, "recorder-extension");
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const EXTENSION_ID = /^[a-p]{32}$/;
const EXTENSION_ID_ALPHABET = "abcdefghijklmnop";
// Runs first when selected, so a broken harness fails before any real probe.
const FIRST_PROBE = "self-check";

function usage() {
  console.log("Usage: node scripts/probe-m15a.mjs [--only <name>[,<name>]] [--json <file>] [--keep-profile]");
  console.log("  --only          Run only these probes (module names in scripts/probes/).");
  console.log("  --json          Also write the report to this file.");
  console.log("  --keep-profile  Keep the temporary Chrome profile after the run.");
}

function argumentsForRun(args) {
  const options = { only: null, json: null, keepProfile: false };
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--help" || arg === "-h") {
      usage();
      return null;
    }
    if (arg === "--keep-profile") options.keepProfile = true;
    else if (arg === "--only" || arg === "--json") {
      const value = args[index + 1];
      if (!value || value.startsWith("--")) throw new Error(`${arg} needs a value`);
      index += 1;
      if (arg === "--only") options.only = value.split(",").map((name) => name.trim()).filter(Boolean);
      else options.json = path.resolve(value);
    } else {
      throw new Error(`Unknown argument ${arg}`);
    }
  }
  return options;
}

// One reader for Chrome's DevTools pipe, copied from smoke-macos.mjs. Replies
// are matched by id. Unlike the smoke's reader, events are kept too, in a
// bounded buffer, because probes compare Page.* and Preload.* evidence.
class Cdp {
  constructor(chrome) {
    this.chrome = chrome;
    this.outgoing = chrome.stdio[3];
    this.incoming = chrome.stdio[4];
    if (!this.outgoing || !this.incoming) throw new Error("Chrome DevTools pipe is unavailable");
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = Buffer.alloc(0);
    this.events = [];
    this.listeners = new Set();
    this.incoming.on("data", (chunk) => this.receive(chunk));
    const fail = (error) => {
      for (const { reject } of this.pending.values()) reject(error);
      this.pending.clear();
    };
    this.incoming.on("error", fail);
    chrome.once("exit", (code) => fail(new Error(`Chrome exited (${code})`)));
  }

  receive(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    for (;;) {
      const end = this.buffer.indexOf(0);
      if (end < 0) return;
      const text = this.buffer.subarray(0, end).toString("utf8");
      this.buffer = this.buffer.subarray(end + 1);
      let message;
      try { message = JSON.parse(text); } catch { continue; }
      if (message.id === undefined) {
        this.event(message);
        continue;
      }
      const waiter = this.pending.get(message.id);
      if (!waiter) continue;
      this.pending.delete(message.id);
      if (message.error) waiter.reject(new Error(`Chrome ${waiter.method} failed: ${message.error.message ?? JSON.stringify(message.error)}`));
      else waiter.resolve(message.result);
    }
  }

  event(message) {
    const entry = { at: Date.now(), method: message.method, params: message.params, sessionId: message.sessionId };
    this.events.push(entry);
    if (this.events.length > 20_000) this.events.splice(0, 5_000);
    for (const listener of this.listeners) {
      try { listener(entry); } catch { /* A probe's listener must not break the reader. */ }
    }
  }

  onEvent(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  send(method, params = {}, sessionId = undefined, timeout = 15_000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Chrome ${method} timed out`));
      }, timeout);
      this.pending.set(id, {
        method,
        resolve: (value) => { clearTimeout(timer); resolve(value); },
        reject: (error) => { clearTimeout(timer); reject(error); },
      });
      this.outgoing.write(`${JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) })}\0`,
        (error) => { if (error) this.pending.get(id)?.reject(error); });
    });
  }

  async targets() {
    return (await this.send("Target.getTargets")).targetInfos;
  }

  async attach(targetId) {
    const { sessionId } = await this.send("Target.attachToTarget", { targetId, flatten: true });
    return new PageDriver(this, sessionId);
  }

  /** Attach to a target for one operation, then detach. */
  async withTarget(targetId, action) {
    const page = await this.attach(targetId);
    try {
      return await action(page);
    } finally {
      await page.detach();
    }
  }
}

// Drives a fixture page, an extension page, or (evaluate only) a worker.
// Clicks are real input events, so Chrome treats them as user gestures just
// as it would a mouse click.
class PageDriver {
  constructor(cdp, sessionId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
  }

  send(method, params = {}, timeout = undefined) {
    return this.cdp.send(method, params, this.sessionId, timeout);
  }

  async evaluate(expression, { userGesture = false, timeout = 15_000 } = {}) {
    const reply = await this.send("Runtime.evaluate", {
      expression, awaitPromise: true, returnByValue: true, userGesture,
    }, timeout);
    if (reply.exceptionDetails) {
      throw new Error(reply.exceptionDetails.exception?.description ?? reply.exceptionDetails.text);
    }
    return reply.result?.value;
  }

  point(selector) {
    return this.evaluate(`(() => {
      const element = document.querySelector(${JSON.stringify(selector)});
      if (!element) throw new Error("Missing ${selector.replaceAll('"', "'")}");
      element.scrollIntoView({ block: "center" });
      const box = element.getBoundingClientRect();
      return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    })()`);
  }

  // A middle click needs buttons: 4 on the press; modifiers: 4 is Meta.
  async click(selector, { button = "left", modifiers = 0 } = {}) {
    const { x, y } = await this.point(selector);
    const buttons = { left: 1, right: 2, middle: 4 }[button] ?? 0;
    await this.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button, buttons, modifiers, clickCount: 1 });
    await this.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button, buttons: 0, modifiers, clickCount: 1 });
  }

  async navigate(url, { transitionType = "typed", wait = true } = {}) {
    const reply = await this.send("Page.navigate", { url, transitionType });
    if (reply.errorText) throw new Error(`Navigation to ${url} failed: ${reply.errorText}`);
    if (wait) await this.waitForLoad(url);
    return reply;
  }

  // Polls, because the page's context is replaced while it navigates.
  waitForLoad(url = null, { timeout = 12_000 } = {}) {
    const wanted = url === null ? null : new URL(url).href;
    return waitFor(async () => {
      const state = await this.evaluate("({ href: location.href, ready: document.readyState })", { timeout: 2_000 });
      requireCondition(state?.ready === "complete" && (wanted === null || state.href === wanted),
        `Page is at ${state?.href} (${state?.ready}); waiting for ${wanted ?? "load"}`);
      return state;
    }, { timeout, interval: 150 });
  }

  detach() {
    return this.cdp.send("Target.detachFromTarget", { sessionId: this.sessionId }).catch(() => undefined);
  }
}

function requireCondition(condition, message) {
  if (!condition) throw new Error(message);
}

async function waitFor(check, { timeout = 12_000, interval = 400, signal } = {}) {
  const deadline = Date.now() + timeout;
  let lastError;
  for (;;) {
    if (signal?.aborted) throw signal.reason;
    try { return await check(); } catch (error) {
      if (error.fatal) throw error;
      lastError = error;
    }
    if (Date.now() >= deadline) throw lastError;
    await delay(interval);
  }
}

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (character) => `&#${character.charCodeAt(0)};`);
}

function htmlPage(title, body) {
  return `<!doctype html><html><head><meta charset="utf-8"><title>${escapeHtml(title)}</title></head><body><h1>${escapeHtml(title)}</h1>${body}</body></html>`;
}

// The probes' pages. no-store on every response makes a restored or
// back-navigated tab ask the server again instead of the HTTP cache, which
// the hold gate relies on.
//   /                 index linking to the other pages
//   /list[?to=path]   #link plain link, #blank target=_blank, #noopener target=_blank rel=noopener,
//                     #cross target=_blank to the localhost origin, #slow link to /slow,
//                     #open window.open(dest), #popup window.open(dest, "", "popup,..."),
//                     #form / #submit a GET form to /page?n=form. With ?to=, every link
//                     and window.open (except #cross and #slow) goes to that path.
//   /board            #key1 / #key2 pushState ?selectedIssue=KEY-1 / KEY-2, #sequence both
//                     100 ms apart, #replace replaceState ?selectedIssue=KEY-3. The title
//                     settles 50 ms after each change, as an SPA's usually does.
//                     window.board.select(key, { replace }) does the same without a click.
//   /page?n=          plain page titled "Page <n>"
//   /browse/<KEY>     artifact-like issue page titled "[KEY] Fixture issue"
//   /slow?d=ms&n=     /page?n= answered after d ms (at most 30 s)
function fixtureServer(state) {
  const held = [];
  const server = createServer(async (request, response) => {
    const url = new URL(request.url, "http://127.0.0.1");
    state.requests.push({ at: Date.now(), method: request.method, url: `${request.headers.host ?? ""}${request.url}` });
    if (state.holding && state.holding(url)) {
      await new Promise((resolve) => held.push(resolve));
    }
    if (url.pathname === "/slow") await delay(Math.min(Number(url.searchParams.get("d") ?? 2000) || 0, 30_000));
    response.setHeader("Content-Type", "text/html; charset=utf-8");
    response.setHeader("Cache-Control", "no-store");
    const n = url.searchParams.get("n") ?? "";
    if (url.pathname === "/") {
      response.end(htmlPage("Probe fixture", `<ul><li><a href="/list">List</a></li><li><a href="/board">Board</a></li><li><a href="/page?n=1">Page 1</a></li><li><a href="/browse/KEY-1">KEY-1</a></li></ul>`));
    } else if (url.pathname === "/list") {
      const to = url.searchParams.get("to");
      const dest = (name) => to ?? `/page?n=${name}`;
      const cross = `${state.otherOrigin}/page?n=cross`;
      response.end(htmlPage("List", `
        <p><a id="link" href="${escapeHtml(dest("link"))}">plain link</a></p>
        <p><a id="blank" target="_blank" href="${escapeHtml(dest("blank"))}">target=_blank</a></p>
        <p><a id="noopener" target="_blank" rel="noopener" href="${escapeHtml(dest("noopener"))}">target=_blank rel=noopener</a></p>
        <p><a id="cross" target="_blank" href="${escapeHtml(cross)}">cross-origin target=_blank</a></p>
        <p><a id="slow" href="/slow?d=2500&amp;n=slow">slow link</a></p>
        <p><button id="open" onclick="window.open(${escapeHtml(JSON.stringify(dest("open")))})">window.open</button></p>
        <p><button id="popup" onclick="window.open(${escapeHtml(JSON.stringify(dest("popup")))}, '', 'popup,width=400,height=300')">window.open popup</button></p>
        <form id="form" action="/page"><input name="n" value="form"><button id="submit">submit</button></form>`));
    } else if (url.pathname === "/board") {
      response.end(htmlPage("Board", `
        <p><button id="key1" onclick="board.select('KEY-1')">KEY-1</button>
        <button id="key2" onclick="board.select('KEY-2')">KEY-2</button>
        <button id="sequence" onclick="board.select('KEY-1'); setTimeout(() => board.select('KEY-2'), 100)">KEY-1 then KEY-2</button>
        <button id="replace" onclick="board.select('KEY-3', { replace: true })">replace KEY-3</button></p>
        <script>
          window.board = {
            select(key, { replace = false } = {}) {
              history[replace ? "replaceState" : "pushState"]({ key }, "", "?selectedIssue=" + encodeURIComponent(key));
              setTimeout(() => { document.title = "[" + key + "] Board issue"; }, 50);
            },
          };
        </script>`));
    } else if (url.pathname === "/page" || url.pathname === "/slow") {
      response.end(htmlPage(`Page ${n}`, `<p><a id="list" href="/list">List</a></p>`));
    } else if (/^\/browse\/[A-Z][A-Z0-9]*-\d+$/.test(url.pathname)) {
      const key = url.pathname.slice("/browse/".length);
      response.end(htmlPage(`[${key}] Fixture issue`, `<p><a id="list" href="/list">List</a></p>`));
    } else {
      response.writeHead(404);
      response.end("Not found");
    }
  });
  const fixture = {
    get requests() { return state.requests; },
    hold(match = () => true) { state.holding = match; },
    release() {
      state.holding = null;
      const parked = held.splice(0);
      for (const resolve of parked) resolve();
      return parked.length;
    },
    heldCount: () => held.length,
  };
  return { server, fixture };
}

async function startServer(server) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Cannot determine fixture port");
  return address.port;
}

async function stopServer(server) {
  if (!server?.listening) return;
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}

async function stopChrome(chrome) {
  if (!chrome?.pid) return;
  // Chrome was started in its own process group. Never signal other profiles.
  try { process.kill(-chrome.pid, "SIGTERM"); } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
  await delay(1500);
  try { process.kill(-chrome.pid, "SIGKILL"); } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

function unpackedExtensionId(directory) {
  const digest = createHash("sha256").update(directory).digest();
  return [...digest.subarray(0, 16)].map((byte) =>
    `${EXTENSION_ID_ALPHABET[byte >> 4]}${EXTENSION_ID_ALPHABET[byte & 0x0f]}`).join("");
}

// Always headless, always this run's own profile. Headless Chrome takes one
// start page; a session restore passes none.
async function launchChrome(profile, args, startUrl) {
  const chrome = spawn(CHROME, [
    "--headless=new", `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check",
    "--remote-debugging-pipe", "--enable-unsafe-extension-debugging", ...args,
    ...(startUrl ? [startUrl] : []),
  ], { detached: true, stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
  await new Promise((resolve, reject) => {
    chrome.once("spawn", resolve);
    chrome.once("error", reject);
  });
  return chrome;
}

const WORKER_LOG = "probe.flush().then(() => chrome.storage.local.get('log')).then((r) => r.log ?? [])";

class ProbeContext {
  constructor({ runDir, profile, port, fixture, signal }) {
    this.runDir = runDir;
    this.profile = profile;
    this.signal = signal;
    this.origin = `http://127.0.0.1:${port}`;
    this.otherOrigin = `http://localhost:${port}`;
    this.fixture = fixture;
    const origin = this.origin;
    this.urls = {
      home: `${origin}/`,
      list: `${origin}/list`,
      listTo: (target) => `${origin}/list?to=${encodeURIComponent(target)}`,
      board: `${origin}/board`,
      page: (n) => `${origin}/page?n=${encodeURIComponent(n)}`,
      browse: (key) => `${origin}/browse/${key}`,
      slow: (ms, n = "slow") => `${origin}/slow?d=${ms}&n=${encodeURIComponent(n)}`,
      other: (pathname) => `${this.otherOrigin}${pathname}`,
    };
    this.chrome = null;
    this.cdp = null;
    this.chromeVersion = null;
    this.extensionId = null;
    this.recorderDir = null;
    this.homeTargetId = null;
    this.controlTargetId = null;
    this.delay = delay;
    this.requireCondition = requireCondition;
    this.waitFor = (check, options = {}) => waitFor(check, { signal: this.signal, ...options });
  }

  log(...parts) {
    console.log("   ", ...parts);
  }

  async launch({ args = [], startUrl = "about:blank" } = {}) {
    this.chrome = await launchChrome(this.profile, args, startUrl);
    this.cdp = new Cdp(this.chrome);
    this.chromeVersion = await this.cdp.send("Browser.getVersion");
    const restored = await this.pageTargets();
    await this.loadRecorder();
    this.homeTargetId = (await this.pageTargets()).find((target) => !this.isControl(target))?.targetId
      ?? await this.openTab("about:blank");
    return { pageTargets: restored };
  }

  // Loaded before any fixture page opens, so the recorder's map sees each
  // source page commit.
  async loadRecorder() {
    this.recorderDir ??= await this.copyExtension(RECORDER);
    const { id } = await this.cdp.send("Extensions.loadUnpacked", { path: this.recorderDir });
    const predicted = unpackedExtensionId(this.recorderDir);
    requireCondition(EXTENSION_ID.test(id ?? "") && id === predicted,
      `Chrome loaded recorder ID ${id ?? "none"}; expected ${predicted}`);
    requireCondition(this.extensionId === null || this.extensionId === id, "Recorder ID changed after relaunch");
    this.extensionId = id;
    await this.waitForWorker();
    this.controlTargetId = await this.openTab(this.controlUrl(), { wait: false });
    await this.withPage(this.controlTargetId, (page) => this.waitFor(() =>
      page.evaluate("typeof chrome.tabs?.query === 'function' || Promise.reject(new Error('loading'))", { timeout: 2_000 }),
    { interval: 200 }));
  }

  controlUrl() {
    return `chrome-extension://${this.extensionId}/control.html`;
  }

  isControl(target) {
    return this.extensionId !== null && target.url.startsWith(`chrome-extension://${this.extensionId}/`);
  }

  // Chrome loads a copy in the run directory, so it never writes into the
  // repository, and the copy's path fixes the unpacked ID for the whole run.
  async copyExtension(directory) {
    const copy = path.join(this.runDir, "extensions", path.basename(directory));
    await rm(copy, { recursive: true, force: true });
    await cp(directory, copy, { recursive: true });
    return realpath(copy);
  }

  /** Copy an unpacked extension into the run directory and load it. */
  async loadExtension(directory) {
    const dir = await this.copyExtension(directory);
    const { id } = await this.cdp.send("Extensions.loadUnpacked", { path: dir });
    return { id, dir };
  }

  async openTab(url, { background = true, newWindow = false, wait = true } = {}) {
    const { targetId } = await this.cdp.send("Target.createTarget", { url, background, newWindow });
    if (wait && /^https?:/.test(url)) await this.withPage(targetId, (page) => page.waitForLoad(url));
    return targetId;
  }

  async pageTargets() {
    return (await this.cdp.targets()).filter((target) => target.type === "page");
  }

  findTarget(match, { timeout = 8_000 } = {}) {
    const test = typeof match === "function" ? match : (target) => target.url === match;
    return this.waitFor(async () => {
      const found = (await this.pageTargets()).find(test);
      requireCondition(found, `No page target matches ${typeof match === "function" ? "the predicate" : match}`);
      return found;
    }, { timeout, interval: 200 });
  }

  activate(targetId) {
    return this.cdp.send("Target.activateTarget", { targetId });
  }

  async closeTab(targetId) {
    await this.cdp.send("Target.closeTarget", { targetId }).catch(() => undefined);
    await this.waitFor(async () => requireCondition(
      !(await this.cdp.targets()).some((target) => target.targetId === targetId),
      `Tab ${targetId} is still closing`), { timeout: 5_000, interval: 150 });
  }

  attach(targetId) {
    return this.cdp.attach(targetId);
  }

  withPage(targetId, action) {
    return this.cdp.withTarget(targetId, action);
  }

  navigate(targetId, url, options = {}) {
    return this.withPage(targetId, (page) => page.navigate(url, options));
  }

  async workerTarget() {
    const url = `chrome-extension://${this.extensionId}/worker.js`;
    return (await this.cdp.targets()).find((target) => target.type === "service_worker" && target.url === url) ?? null;
  }

  waitForWorker({ timeout = 12_000 } = {}) {
    return this.waitFor(async () => {
      const target = await this.workerTarget();
      requireCondition(target, "The recorder worker is not running");
      return target;
    }, { timeout, interval: 200 });
  }

  // The first evaluate on a freshly attached worker sometimes never answers,
  // so each attempt has a short timeout. Detaching matters: a worker with
  // DevTools attached never idles out.
  async inWorker(expression, { timeout = 4_000, retries = 3, wait = 12_000 } = {}) {
    let lastError;
    for (let attempt = 0; attempt <= retries; attempt += 1) {
      const target = await this.waitForWorker({ timeout: wait });
      let page;
      try {
        page = await this.cdp.attach(target.targetId);
        return await page.evaluate(expression, { timeout });
      } catch (error) {
        lastError = error;
        if (!/timed out|No target|not found|detached/i.test(error.message)) throw error;
      } finally {
        await page?.detach();
      }
    }
    throw lastError;
  }

  control(expression, { userGesture = false } = {}) {
    return this.withPage(this.controlTargetId, (page) => page.evaluate(expression, { userGesture }));
  }

  tabs() {
    return this.control(`chrome.tabs.query({}).then((tabs) => tabs.map((tab) => ({
      id: tab.id, windowId: tab.windowId, index: tab.index, active: tab.active, discarded: tab.discarded,
      status: tab.status, url: tab.url ?? null, title: tab.title ?? null })))`);
  }

  tabIdFor(url, { timeout = 8_000 } = {}) {
    const wanted = new URL(url).href;
    return this.waitFor(async () => {
      const tab = (await this.tabs()).find((entry) => entry.url === wanted);
      requireCondition(tab, `No tab shows ${wanted}`);
      return tab.id;
    }, { timeout, interval: 200 });
  }

  // A running worker is flushed first, so every record it has taken is in
  // the log. A stopped worker has nothing pending, and reading through the
  // control tab does not wake it.
  async readLog({ since = null, kinds = null, bootId = null } = {}) {
    let log = null;
    if (await this.workerTarget()) log = await this.inWorker(WORKER_LOG, { wait: 1_000 }).catch(() => null);
    log ??= await this.control("chrome.storage.local.get('log').then((r) => r.log ?? [])");
    return log.filter((entry) => (since === null || entry.at >= since)
      && (kinds === null || kinds.includes(entry.kind))
      && (bootId === null || entry.bootId === bootId));
  }

  async clearLog() {
    if (await this.workerTarget()) {
      const cleared = await this.inWorker("probe.clearLog().then(() => true)", { wait: 1_000 }).catch(() => false);
      if (cleared) return;
    }
    await this.control("chrome.storage.local.set({ log: [] })");
  }

  waitForEvent(predicate, { timeout = 8_000, since = null } = {}) {
    return this.waitFor(async () => {
      const found = (await this.readLog({ since })).find(predicate);
      requireCondition(found, "The recorder has no matching event yet");
      return found;
    }, { timeout, interval: 300 });
  }

  brief(entry) {
    const { details = {}, extra = {} } = entry;
    const parts = [entry.kind, `boot=${entry.bootId?.slice(0, 8)}`, `seq=${entry.seq}`];
    for (const key of ["tabId", "sourceTabId", "frameId", "url", "transitionType", "transitionQualifiers", "documentLifecycle", "replacedTabId", "addedTabId", "removedTabId", "windowId", "state"]) {
      if (details[key] !== undefined) parts.push(`${key}=${typeof details[key] === "object" ? JSON.stringify(details[key]) : details[key]}`);
    }
    if (details.change) parts.push(`change=${JSON.stringify(details.change)}`);
    if (extra.sourceMapped !== undefined) parts.push(`sourceMapped=${extra.sourceMapped}`);
    return parts.join(" ");
  }

  workerBootId() {
    return this.inWorker("probe.bootId");
  }

  // ServiceWorker.stopAllWorkers stops every worker in the profile, including
  // any extension Enterprise policy force-installs. Target.closeTarget on the
  // worker is the fallback.
  async stopWorker() {
    const bootId = await this.workerBootId();
    const target = await this.workerTarget();
    const gone = (timeout) => this.waitFor(async () => requireCondition(
      !(await this.workerTarget()), "The recorder worker is still running"), { timeout, interval: 150 });
    await this.withPage(this.controlTargetId, async (page) => {
      await page.send("ServiceWorker.enable");
      await page.send("ServiceWorker.stopAllWorkers");
      await page.send("ServiceWorker.disable").catch(() => undefined);
    });
    try {
      await gone(4_000);
      return { bootId, method: "ServiceWorker.stopAllWorkers" };
    } catch {
      await this.cdp.send("Target.closeTarget", { targetId: target.targetId }).catch(() => undefined);
      await gone(4_000);
      return { bootId, method: "Target.closeTarget" };
    }
  }

  waitForRestart(bootId, { timeout = 12_000 } = {}) {
    return this.waitFor(async () => {
      const current = await this.inWorker("probe.bootId", { wait: 500, retries: 1 });
      requireCondition(current !== bootId, "The recorder worker has not restarted");
      return current;
    }, { timeout, interval: 300 });
  }

  async wakeWorker() {
    const reply = await this.control("chrome.runtime.sendMessage({ probe: 'wake' })");
    return reply?.bootId ?? null;
  }

  async withExtensionsPage(action) {
    const targetId = await this.openTab("chrome://extensions", { wait: false });
    try {
      return await this.withPage(targetId, async (page) => {
        await this.waitFor(() => page.evaluate("typeof chrome.developerPrivate?.getExtensionsInfo === 'function' || Promise.reject(new Error('loading'))", { timeout: 2_000 }), { interval: 200 });
        return action(page);
      });
    } finally {
      await this.closeTab(targetId).catch(() => undefined);
    }
  }

  // Grants the site as a user would under Site access, then asks through
  // permissions.request, which then resolves without a prompt.
  async grantOrigin(pattern) {
    await this.withExtensionsPage((page) => page.evaluate(
      `chrome.developerPrivate.addHostPermission(${JSON.stringify(this.extensionId)}, ${JSON.stringify(pattern)})`));
    return this.control(`chrome.permissions.request({ origins: [${JSON.stringify(pattern)}] })`, { userGesture: true });
  }

  extensionsInfo() {
    return this.withExtensionsPage((page) => page.evaluate(`chrome.developerPrivate.getExtensionsInfo({ includeDisabled: true, includeTerminated: true })
      .then((list) => list.map((entry) => ({ id: entry.id, name: entry.name, version: entry.version, location: entry.location,
        state: entry.state, permissions: entry.permissions })))`));
  }

  // The control tab is closed first so it is not part of the saved session;
  // the recorder is unloaded until loadRecorder runs again.
  async relaunch({ restoreSession = false, args = [], beforeLaunch = null } = {}) {
    await this.closeTab(this.controlTargetId).catch(() => undefined);
    const chrome = this.chrome;
    const exited = chrome.exitCode !== null ? Promise.resolve() : new Promise((resolve) => chrome.once("exit", resolve));
    await this.cdp.send("Browser.close").catch(() => undefined);
    const closed = await Promise.race([exited.then(() => true), delay(10_000).then(() => false)]);
    if (!closed) await stopChrome(chrome);
    this.chrome = null;
    this.cdp = null;
    this.homeTargetId = null;
    this.controlTargetId = null;
    if (beforeLaunch) await beforeLaunch();
    return this.launch({
      args: [...(restoreSession ? ["--restore-last-session"] : []), ...args],
      startUrl: restoreSession ? null : "about:blank",
    });
  }

  // Between probes: keep the home and control tabs, close the rest, and
  // start the next probe with an empty log.
  async reset() {
    const targets = await this.pageTargets();
    if (!targets.some((target) => target.targetId === this.homeTargetId)) this.homeTargetId = await this.openTab("about:blank");
    if (!targets.some((target) => target.targetId === this.controlTargetId)) {
      this.controlTargetId = await this.openTab(this.controlUrl(), { wait: false });
      await this.withPage(this.controlTargetId, (page) => this.waitFor(() =>
        page.evaluate("typeof chrome.tabs?.query === 'function' || Promise.reject(new Error('loading'))", { timeout: 2_000 }),
      { interval: 200 }));
    }
    for (const target of await this.pageTargets()) {
      if (target.targetId !== this.homeTargetId && target.targetId !== this.controlTargetId) await this.closeTab(target.targetId).catch(() => undefined);
    }
    this.fixture.release();
    await delay(300);
    await this.clearLog();
  }
}

async function loadProbes(only) {
  const names = (await readdir(PROBES, { withFileTypes: true }))
    .filter((entry) => entry.isFile() && entry.name.endsWith(".mjs"))
    .map((entry) => entry.name.slice(0, -".mjs".length))
    .sort((a, b) => (a === FIRST_PROBE ? -1 : b === FIRST_PROBE ? 1 : a.localeCompare(b)));
  const unknown = (only ?? []).filter((name) => !names.includes(name));
  if (unknown.length) throw new Error(`Unknown probe ${unknown.join(", ")}; available: ${names.join(", ")}`);
  const probes = [];
  for (const file of names.filter((name) => !only || only.includes(name))) {
    const probe = await import(pathToFileURL(path.join(PROBES, `${file}.mjs`)).href);
    requireCondition(typeof probe.name === "string" && typeof probe.run === "function",
      `scripts/probes/${file}.mjs must export name, title, and run(ctx)`);
    probes.push(probe);
  }
  return probes;
}

function shorten(value, limit = 70) {
  const text = typeof value === "string" ? value : JSON.stringify(value) ?? String(value);
  return text.length > limit ? `${text.slice(0, limit - 3)}...` : text;
}

const status = (finding) => (!finding.matches ? "MISMATCH" : finding.inconclusive ? "inconclusive" : "ok");

function printTable(results) {
  const rows = [["", "probe", "claim", "expected", "observed"]];
  for (const result of results) {
    if (result.error) rows.push(["THREW", result.name, result.error, "", ""]);
    for (const finding of result.findings) {
      rows.push([status(finding), result.name, shorten(finding.claim, 60),
        shorten(finding.expected, 40), shorten(finding.observed, 60)]);
    }
  }
  const widths = rows[0].map((_, column) => Math.max(...rows.map((row) => row[column].length)));
  console.log("");
  for (const [index, row] of rows.entries()) {
    console.log(row.map((cell, column) => cell.padEnd(widths[column])).join("  ").trimEnd());
    if (index === 0) console.log(widths.map((width) => "-".repeat(width)).join("  "));
  }
  for (const result of results) {
    for (const finding of result.findings.filter((entry) => entry.note)) console.log(`note (${result.name}): ${finding.note}`);
  }
}

async function run() {
  const options = argumentsForRun(process.argv.slice(2));
  if (!options) return;
  if (process.platform !== "darwin") throw new Error("This probe runner supports macOS only");
  await access(CHROME, constants.X_OK);
  const probes = await loadProbes(options.only);

  const runDir = await mkdtemp(path.join(os.tmpdir(), "brauser-probe-m15a-"));
  const profile = path.join(runDir, "chrome-profile");
  await mkdir(profile);
  const state = { requests: [], holding: null, otherOrigin: null };
  const { server, fixture } = fixtureServer(state);
  const abort = new AbortController();
  const interrupt = () => abort.abort(new Error("Probe run interrupted"));
  process.on("SIGINT", interrupt);
  process.on("SIGTERM", interrupt);
  let ctx = null;
  let failed = false;
  const results = [];
  const report = { startedAt: new Date().toISOString(), runDir, chrome: null, extensionId: null, extensions: null, probes: results };

  try {
    const port = await startServer(server);
    ctx = new ProbeContext({ runDir, profile, port, fixture, signal: abort.signal });
    state.otherOrigin = ctx.otherOrigin;
    console.log(`Run directory: ${runDir}`);
    console.log(`Fixture: ${ctx.origin} (ungranted twin ${ctx.otherOrigin})`);
    await ctx.launch();
    report.chrome = ctx.chromeVersion;
    report.extensionId = ctx.extensionId;
    console.log(`Chrome: ${ctx.chromeVersion.product}; recorder ${ctx.extensionId}`);
    // Enterprise policy can force-install extensions into every fresh profile,
    // so record what is loaded next to the results.
    report.extensions = await ctx.extensionsInfo().catch((error) => ({ error: error.message }));

    for (const probe of probes) {
      if (abort.signal.aborted) throw abort.signal.reason;
      console.log(`\n${probe.name}: ${probe.title ?? ""}`);
      const result = { name: probe.name, title: probe.title ?? "", startedAt: new Date().toISOString(), durationMs: 0, findings: [], error: null };
      results.push(result);
      const started = Date.now();
      try {
        await ctx.reset();
        const findings = await probe.run(ctx);
        requireCondition(Array.isArray(findings), `${probe.name} did not return an array of findings`);
        result.findings = findings;
      } catch (error) {
        result.error = error.stack ?? error.message;
        console.error(`   ${probe.name} threw: ${error.message}`);
      }
      result.durationMs = Date.now() - started;
      for (const finding of result.findings) {
        console.log(`   ${status(finding).padEnd(8)} ${finding.claim}`);
      }
    }
  } catch (error) {
    failed = true;
    report.harnessError = error.stack ?? error.message;
    console.error(`\nProbe harness failed: ${error.message}`);
  } finally {
    process.off("SIGINT", interrupt);
    process.off("SIGTERM", interrupt);
    try { await stopChrome(ctx?.chrome); } catch (error) {
      failed = true;
      console.error(`Could not stop the headless Chrome process group: ${error.message}`);
    }
    try { await stopServer(server); } catch (error) {
      failed = true;
      console.error(`Could not stop the fixture server: ${error.message}`);
    }
    if (!options.keepProfile) await rm(profile, { recursive: true, force: true }).catch(() => undefined);
  }

  printTable(results);
  const mismatches = results.flatMap((result) => result.findings.filter((finding) => !finding.matches));
  const threw = results.filter((result) => result.error);
  const inconclusive = results.flatMap((result) => result.findings.filter((finding) => finding.matches && finding.inconclusive));
  report.finishedAt = new Date().toISOString();
  report.summary = { probes: results.length, findings: results.reduce((sum, result) => sum + result.findings.length, 0), mismatches: mismatches.length, inconclusive: inconclusive.length, threw: threw.length };
  const text = `${JSON.stringify(report, null, 2)}\n`;
  await writeFile(path.join(runDir, "report.json"), text);
  if (options.json) await writeFile(options.json, text);
  console.log(`\nReport: ${path.join(runDir, "report.json")}${options.json ? ` and ${options.json}` : ""}`);
  console.log(`${report.summary.findings} findings, ${mismatches.length} mismatched, ${inconclusive.length} inconclusive, ${threw.length} probes threw${failed ? ", harness failed" : ""}.`);
  if (failed || mismatches.length || threw.length) process.exitCode = 1;
}

await run().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
