#!/usr/bin/env node

// Guided real-Chrome M1 smoke run. Native dialogs and Chrome permission prompts
// deliberately remain manual; all host and vault state is kept in a temp home.
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import {
  access, chmod, lstat, mkdir, mkdtemp, open, readFile,
  readdir, realpath, rename, unlink, writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline/promises";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const REPO = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const DIST = path.join(REPO, "extension", "dist");
const DEFAULT_HOST = path.join(REPO, "target", "debug", "rauser");
const DEFAULT_CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const DEV_DESCRIPTION = "Rauser development native host";
const EXTENSION_ID = /^[a-p]{32}$/;
const EXTENSION_ID_ALPHABET = "abcdefghijklmnop";

function usage() {
  console.log("Usage: node scripts/smoke-macos.mjs [--host /absolute/path/to/rauser] [--chrome /absolute/path/to/Google Chrome]");
}

function argumentsForRun(args) {
  let host = DEFAULT_HOST;
  let chrome = DEFAULT_CHROME;
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--help") {
      usage();
      return null;
    }
    if ((args[index] === "--host" || args[index] === "--chrome") && args[index + 1]) {
      if (!path.isAbsolute(args[index + 1])) throw new Error(`${args[index]} needs an absolute path`);
      if (args[index] === "--host") host = args[index + 1];
      else chrome = args[index + 1];
      index += 1;
      continue;
    }
    throw new Error(`Unknown argument: ${args[index]}`);
  }
  return { host, chrome };
}

function shellQuote(value) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function inside(parent, child) {
  const relative = path.relative(parent, child);
  return relative === "" || (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}

async function maybeLstat(filename) {
  try {
    return await lstat(filename);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

async function atomicWrite(filename, contents, mode = 0o600) {
  const directory = path.dirname(filename);
  await mkdir(directory, { recursive: true });
  const temporary = path.join(directory, `.${path.basename(filename)}.${randomUUID()}.tmp`);
  const file = await open(temporary, "wx", mode);
  try {
    await file.writeFile(contents);
    await file.chmod(mode);
    await file.sync();
  } finally {
    await file.close();
  }
  try {
    await rename(temporary, filename);
  } catch (error) {
    await unlink(temporary).catch(() => undefined);
    throw error;
  }
}

async function commandOutput(executable, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code === 0) resolve(stdout.trim());
      else reject(new Error(`${executable} exited ${code}: ${stderr.trim()}`));
    });
  });
}

async function hostConfig(wrapper) {
  const request = Buffer.from(JSON.stringify({
    type: "get_config", protocol_version: 2, request_id: randomUUID(),
  }));
  const length = Buffer.alloc(4);
  length.writeUInt32LE(request.length);
  return new Promise((resolve, reject) => {
    const child = spawn(wrapper, ["serve"], { stdio: ["pipe", "pipe", "pipe"] });
    const output = [];
    const errors = [];
    let settled = false;
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      finish(new Error("Native host get_config timed out"));
    }, 10_000);
    function finish(error, value) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (error) reject(error);
      else resolve(value);
    }
    child.stdout.on("data", (chunk) => { output.push(chunk); });
    child.stderr.on("data", (chunk) => { errors.push(chunk); });
    child.on("error", (error) => finish(error));
    child.on("close", (code) => {
      try {
        if (code !== 0) throw new Error(`Native host get_config failed: ${Buffer.concat(errors).toString("utf8").trim()}`);
        const frame = Buffer.concat(output);
        if (frame.length < 4 || frame.readUInt32LE(0) !== frame.length - 4) {
          throw new Error("Native host returned an invalid response frame");
        }
        const reply = JSON.parse(frame.subarray(4).toString("utf8"));
        if (reply.type !== "config_result" || reply.protocol_version !== 2) {
          throw new Error(`Native host returned ${reply.type ?? "an unknown response"}`);
        }
        finish(null, reply);
      } catch (error) {
        finish(error);
      }
    });
    child.stdin.end(Buffer.concat([length, request]));
  });
}

// One reader for Chrome's DevTools pipe. Replies are matched by id; events
// are ignored because every wait below polls page or host state instead.
class Cdp {
  constructor(chrome) {
    this.chrome = chrome;
    this.outgoing = chrome.stdio[3];
    this.incoming = chrome.stdio[4];
    if (!this.outgoing || !this.incoming) throw new Error("Chrome DevTools pipe is unavailable");
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = Buffer.alloc(0);
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
      const waiter = message.id === undefined ? null : this.pending.get(message.id);
      if (!waiter) continue;
      this.pending.delete(message.id);
      if (message.error) waiter.reject(new Error(`Chrome ${waiter.method} failed: ${message.error.message ?? JSON.stringify(message.error)}`));
      else waiter.resolve(message.result);
    }
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

  async targetFor(url) {
    return (await this.targets()).find((target) => target.url === url || target.url.startsWith(`${url}?`)) ?? null;
  }

  /** Attach to a target for one operation, then detach. */
  async withTarget(targetId, action) {
    const { sessionId } = await this.send("Target.attachToTarget", { targetId, flatten: true });
    try {
      return await action(new PageDriver(this, sessionId));
    } finally {
      await this.send("Target.detachFromTarget", { sessionId }).catch(() => undefined);
    }
  }
}

// Drives an extension or fixture page. Clicks are real input events, so
// Chrome treats them as user gestures just as it would a mouse click.
class PageDriver {
  constructor(cdp, sessionId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
  }

  async evaluate(expression, { userGesture = false } = {}) {
    const reply = await this.cdp.send("Runtime.evaluate", {
      expression, awaitPromise: true, returnByValue: true, userGesture,
    }, this.sessionId);
    if (reply.exceptionDetails) {
      throw new Error(reply.exceptionDetails.exception?.description ?? reply.exceptionDetails.text);
    }
    return reply.result?.value;
  }

  async click(selector) {
    const point = await this.evaluate(`(() => {
      const element = document.querySelector(${JSON.stringify(selector)});
      if (!element) throw new Error("Missing ${selector.replaceAll('"', "'")}");
      if (element.disabled) throw new Error("${selector.replaceAll('"', "'")} is disabled");
      element.scrollIntoView({ block: "center" });
      const box = element.getBoundingClientRect();
      return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    })()`);
    for (const type of ["mousePressed", "mouseReleased"]) {
      await this.cdp.send("Input.dispatchMouseEvent", {
        type, x: point.x, y: point.y, button: "left", clickCount: 1,
      }, this.sessionId);
    }
  }

  async fill(selector, value) {
    await this.evaluate(`(() => {
      const element = document.querySelector(${JSON.stringify(selector)});
      if (!element) throw new Error("Missing ${selector.replaceAll('"', "'")}");
      element.value = ${JSON.stringify(value)};
      element.dispatchEvent(new Event("input", { bubbles: true }));
    })()`);
  }

  text(selector) {
    return this.evaluate(`document.querySelector(${JSON.stringify(selector)})?.textContent ?? null`);
  }

  enabled(selector) {
    return this.evaluate(`(() => {
      const element = document.querySelector(${JSON.stringify(selector)});
      return element !== null && !element.disabled;
    })()`);
  }

  hidden(selector) {
    return this.evaluate(`document.querySelector(${JSON.stringify(selector)})?.hidden ?? null`);
  }
}

async function nativeHostPreflight(cdp, extensionId) {
  const target = await cdp.send("Target.createTarget", {
    url: `chrome-extension://${extensionId}/panel.html`, background: true,
  });
  try {
    const expression = `new Promise(resolve => {
      const port = chrome.runtime.connectNative("com.rauser.browser");
      const timer = setTimeout(() => resolve({ok: false, error: "native host timed out"}), 5000);
      port.onMessage.addListener(message => {
        clearTimeout(timer);
        resolve({ok: message.type === "hello_result", type: message.type});
        port.disconnect();
      });
      port.onDisconnect.addListener(() => {
        clearTimeout(timer);
        resolve({ok: false, error: chrome.runtime.lastError?.message ?? "disconnected"});
      });
      port.postMessage({type: "hello", protocol_version: 2, request_id: "smoke-preflight"});
    })`;
    await cdp.withTarget(target.targetId, async (page) => {
      let lastError = "extension page did not load";
      for (let attempt = 0; attempt < 20; attempt += 1) {
        let reply;
        try {
          reply = await page.evaluate(expression);
        } catch (error) {
          lastError = error.message;
          if (!/connectNative/.test(lastError)) break;
          await delay(250);
          continue;
        }
        if (reply?.ok === true) return;
        lastError = reply?.error ?? `unexpected host response ${reply?.type ?? "none"}`;
        break;
      }
      throw new Error(`Chrome could not connect to the native host: ${lastError}`);
    });
  } finally {
    await cdp.send("Target.closeTarget", { targetId: target.targetId }).catch(() => undefined);
    // This tab has the side panel's URL; later steps must not mistake it for the panel.
    await waitFor(async () => requireCondition(
      !(await cdp.targets()).some((entry) => entry.targetId === target.targetId),
      "Preflight tab is still closing")).catch(() => undefined);
  }
}

function fixtureServer(caseId) {
  const server = createServer((request, response) => {
    const pathname = new URL(request.url, "http://127.0.0.1").pathname;
    const known = new Set(["/", "/allowed/page", "/blocked/page", "/allowed/after-removal"]);
    response.setHeader("Content-Type", "text/html; charset=utf-8");
    response.setHeader("Cache-Control", "no-store");
    if (!known.has(pathname)) {
      response.writeHead(404);
      response.end("Not found");
      return;
    }
    const title = pathname === "/" ? "Rauser smoke fixture" : `Rauser smoke ${pathname}`;
    response.end(`<!doctype html><html><head><title>${title}</title></head><body><h1>${title}</h1><p>Run ${caseId}</p><ul><li><a href="/allowed/page?case=${caseId}">Allowed page</a></li><li><a href="/blocked/page?case=${caseId}">Blocked page</a></li><li><a href="/allowed/after-removal?case=${caseId}">After removal</a></li></ul></body></html>`);
  });
  return server;
}

async function startServer(server) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Cannot determine fixture port");
  return `http://127.0.0.1:${address.port}`;
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

async function extensionIdsInProfile(profile, dist) {
  const ids = new Set();
  for (const entry of await readdir(profile, { withFileTypes: true })) {
    if (!entry.isDirectory() || (entry.name !== "Default" && !entry.name.startsWith("Profile "))) continue;
    for (const filename of ["Preferences", "Secure Preferences"]) {
      let preferences;
      try {
        preferences = JSON.parse(await readFile(path.join(profile, entry.name, filename), "utf8"));
      } catch {
        continue;
      }
      for (const [id, settings] of Object.entries(preferences.extensions?.settings ?? {})) {
        if (!EXTENSION_ID.test(id) || typeof settings?.path !== "string" || !path.isAbsolute(settings.path)) continue;
        if (await realpath(settings.path).catch(() => null) === dist) ids.add(id);
      }
    }
  }
  return [...ids];
}

async function markdownFiles(directory) {
  const found = [];
  async function visit(current) {
    let entries;
    try { entries = await readdir(current, { withFileTypes: true }); } catch (error) {
      if (error.code === "ENOENT") return;
      throw error;
    }
    for (const entry of entries) {
      const filename = path.join(current, entry.name);
      if (entry.isDirectory()) await visit(filename);
      else if (entry.isFile() && entry.name.endsWith(".md")) {
        found.push({ path: filename, text: await readFile(filename, "utf8") });
      }
    }
  }
  await visit(directory);
  return found;
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

async function run() {
  const options = argumentsForRun(process.argv.slice(2));
  if (!options) return;
  if (process.platform !== "darwin") throw new Error("This smoke runner supports macOS only");
  await access(options.host, constants.X_OK).catch(() => {
    throw new Error(`Build the native host first: cargo build --locked -p rauser (${options.host} is unavailable)`);
  });
  await access(options.chrome, constants.X_OK);
  await access(path.join(DIST, "manifest.json"), constants.R_OK).catch(() => {
    throw new Error(`Build the extension first: npm run build:extension (${DIST} is unavailable)`);
  });

  const runDir = await mkdtemp(path.join(os.tmpdir(), "rauser-smoke-macos-"));
  const testHome = path.join(runDir, "home");
  const profile = path.join(runDir, "chrome-profile");
  const notes = path.join(runDir, "notes");
  const wrapper = path.join(runDir, "rauser-host-wrapper");
  const manifest = path.join(profile, "NativeMessagingHosts", "com.rauser.browser.json");
  const caseId = randomUUID().slice(0, 8);
  const server = fixtureServer(caseId);
  const abort = new AbortController();
  const prompt = createInterface({ input: process.stdin, output: process.stdout });
  let chrome = null;
  let installedManifest = null;
  let failed = false;
  const interrupt = () => abort.abort(new Error("Smoke run interrupted"));
  process.on("SIGINT", interrupt);
  process.on("SIGTERM", interrupt);

  async function ask(message) {
    const answer = (await prompt.question(message, { signal: abort.signal })).trim();
    if (answer.toLowerCase() === "q") throw new Error("Smoke run stopped");
    return answer;
  }

  let cdp = null;
  let extensionId = null;
  const panelUrl = () => `chrome-extension://${extensionId}/panel.html`;
  const settingsUrl = () => `chrome-extension://${extensionId}/options.html`;

  async function step(title, action) {
    console.log(`\n${title}`);
    await action();
    console.log(`PASS: ${title}`);
  }

  // Only Chrome's permission prompt and Rauser's native dialogs need a person.
  async function userAction(instruction, check) {
    console.log(`  ACTION: ${instruction}`);
    // Say what is still missing, so a stuck wait is diagnosable.
    let reported = null;
    let since = Date.now();
    return waitFor(async () => {
      try {
        return await check();
      } catch (error) {
        if (error.message !== reported && Date.now() - since > 5_000) {
          console.log(`  Waiting: ${error.message}`);
          reported = error.message;
          since = Date.now();
        }
        throw error;
      }
    }, { timeout: 60 * 60_000, interval: 500, signal: abort.signal });
  }

  async function onPage(url, action) {
    const target = await cdp.targetFor(url);
    requireCondition(target, `${url.endsWith("panel.html") ? "The side panel" : "The settings page"} is not open`);
    return cdp.withTarget(target.targetId, action);
  }

  const onPanel = (action) => onPage(panelUrl(), action);
  const onSettings = (action) => onPage(settingsUrl(), action);

  async function inSettings(action) {
    const target = await cdp.targetFor(settingsUrl());
    requireCondition(target, "The settings page is not open");
    // Input events reach only a visible tab.
    await cdp.send("Target.activateTarget", { targetId: target.targetId });
    return cdp.withTarget(target.targetId, action);
  }

  async function openPanel() {
    if (await cdp.targetFor(panelUrl())) return;
    // Opening the side panel needs a user gesture in an extension page.
    // Evaluate with a gesture in the settings page, or a temporary tab.
    let source = await cdp.targetFor(settingsUrl());
    let temporary = null;
    if (!source) {
      temporary = await cdp.send("Target.createTarget", { url: panelUrl(), background: true });
      source = { targetId: temporary.targetId };
    }
    try {
      await cdp.withTarget(source.targetId, async (page) => {
        await waitFor(() => page.evaluate("typeof chrome.sidePanel?.open === 'function' || Promise.reject(new Error('loading'))"));
        const windowId = await page.evaluate("chrome.tabs.getCurrent().then((tab) => tab.windowId)");
        await page.evaluate(`chrome.sidePanel.open({ windowId: ${Number(windowId)} })`, { userGesture: true });
      });
    } catch (error) {
      console.log(`  Could not open the side panel automatically (${error.message}).`);
    } finally {
      if (temporary) {
        await cdp.send("Target.closeTarget", { targetId: temporary.targetId }).catch(() => undefined);
        await waitFor(async () => requireCondition(
          !(await cdp.targets()).some((entry) => entry.targetId === temporary.targetId),
          "Temporary tab is still closing")).catch(() => undefined);
      }
    }
    try {
      await waitFor(async () => requireCondition(await cdp.targetFor(panelUrl()), "Side panel did not open"),
        { timeout: 4_000 });
    } catch {
      await userAction("Click the Rauser toolbar button to open the side panel.",
        async () => requireCondition(await cdp.targetFor(panelUrl()), "Side panel is not open yet"));
    }
  }

  async function closePanel() {
    const target = await cdp.targetFor(panelUrl());
    if (!target) return;
    await cdp.withTarget(target.targetId, (page) => page.evaluate("window.close()")).catch(() => undefined);
    const closed = async () => requireCondition(!(await cdp.targetFor(panelUrl())), "Side panel is still open");
    try {
      await waitFor(closed, { timeout: 4_000 });
    } catch {
      await userAction("Close the Rauser side panel.", closed);
    }
  }

  async function openSettingsTab() {
    if (await cdp.targetFor(settingsUrl())) return;
    await cdp.send("Target.createTarget", { url: settingsUrl() });
  }

  async function settingsConnected() {
    await waitFor(() => onSettings(async (page) =>
      requireCondition(await page.enabled("#choose-folder"), "Settings page is not connected to the host")));
  }

  async function fixtureTarget(origin) {
    const target = (await cdp.targets()).find((entry) => entry.type === "page" && entry.url.startsWith(`${origin}/`));
    requireCondition(target, "The fixture tab is not open");
    return target;
  }

  async function navigate(origin, url) {
    const target = await fixtureTarget(origin);
    await cdp.withTarget(target.targetId, async (page) => {
      await cdp.send("Page.navigate", { url }, page.sessionId);
      await waitFor(async () => requireCondition(
        await page.evaluate(`location.href === ${JSON.stringify(url)} && document.readyState === "complete"`),
        `Fixture did not finish loading ${url}`));
    });
    // The worker fills a visit's title from the tab's title update.
    await delay(1_500);
  }

  async function pendingVisitsCleared() {
    await waitFor(() => onPanel(async (page) => {
      const summary = await page.text("#queue-summary");
      requireCondition(summary === "0 pending visits.", `Panel shows: ${summary}`);
    }), { timeout: 20_000 });
  }

  async function settingsStatus() {
    return onSettings((page) => page.text("#status"));
  }

  async function chromeGrants(pattern) {
    return onSettings((page) => page.evaluate(`Promise.all([
      chrome.permissions.contains({ origins: [${JSON.stringify(pattern)}] }),
      chrome.permissions.contains({ permissions: ["webNavigation"] }),
    ]).then(([origin, api]) => ({ origin, api }))`));
  }

  async function createNote(expected, title, body) {
    await waitFor(() => onPanel(async (page) =>
      requireCondition(await page.enabled("#create-note"), "Create page note is disabled")));
    await onPanel(async (page) => {
      await page.fill("#note-title", title);
      await page.fill("#note-body", body);
      await page.click("#create-note");
    });
    return waitFor(() => onPanel(async (page) => {
      const result = await page.text("#note-result");
      requireCondition(await page.enabled("#create-note") && result?.startsWith(expected),
        `Panel note result: ${result || "(none yet)"}`);
      return result;
    }), { timeout: 20_000 });
  }

  try {
    await Promise.all([mkdir(testHome), mkdir(notes), mkdir(profile)]);
    await writeFile(wrapper, `#!/bin/sh\nexport HOME=${shellQuote(testHome)}\nexec ${shellQuote(options.host)} "$@"\n`, { mode: 0o700 });
    await chmod(wrapper, 0o700);
    const configPath = await commandOutput(wrapper, ["--config-path"]);
    const actualHome = await realpath(testHome);
    requireCondition(path.isAbsolute(configPath) &&
      (inside(testHome, configPath) || inside(actualHome, configPath)),
    `Host config would escape the temporary home: ${configPath}`);
    const dist = await realpath(DIST);
    extensionId = unpackedExtensionId(dist);
    const hostManifest = Buffer.from(`${JSON.stringify({
      name: "com.rauser.browser",
      description: DEV_DESCRIPTION,
      path: wrapper,
      type: "stdio",
      allowed_origins: [`chrome-extension://${extensionId}/`],
    }, null, 2)}\n`);
    await atomicWrite(manifest, hostManifest);
    installedManifest = hostManifest;
    console.log(`Predicted unpacked extension ID: ${extensionId}`);
    console.log(`Isolated-profile native host registered for ${extensionId} before Chrome launch.`);

    const origin = await startServer(server);
    const allowed = `${origin}/allowed/page?case=${caseId}`;
    const blocked = `${origin}/blocked/page?case=${caseId}`;
    const afterRemoval = `${origin}/allowed/after-removal?case=${caseId}`;
    const title = `Rauser smoke ${caseId}`;
    const firstBody = `First note ${caseId}`;
    const changedBody = `Review note ${caseId}`;
    await writeFile(path.join(runDir, "run-info.json"), `${JSON.stringify({
      origin, allowed, blocked, afterRemoval, notes, profile, testHome, configPath,
      manifest, host: options.host, chrome: options.chrome,
    }, null, 2)}\n`);

    console.log(`\nSmoke artifacts: ${runDir}`);
    console.log(`Fixture: ${origin}`);
    console.log(`Notes folder: ${notes}`);
    console.log(`Extension folder: ${DIST}`);
    console.log("Loading the unpacked extension into the isolated Chrome profile.");
    chrome = spawn(options.chrome, [
      `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check",
      "--remote-debugging-pipe", "--enable-unsafe-extension-debugging",
      "chrome://extensions", `${origin}/`,
    ], { detached: true, stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    await new Promise((resolve, reject) => {
      chrome.once("spawn", resolve);
      chrome.once("error", reject);
    });

    cdp = new Cdp(chrome);
    const loaded = await cdp.send("Extensions.loadUnpacked", { path: dist });
    const loadedId = loaded?.id;
    requireCondition(loadedId === extensionId,
      `Chrome loaded extension ID ${loadedId ?? "none"}; expected ${extensionId}`);
    await waitFor(async () => {
      const discovered = await extensionIdsInProfile(profile, dist);
      requireCondition(discovered.length === 1 && discovered[0] === extensionId,
        `Expected ${extensionId} for ${DIST} in this isolated Chrome profile; found ${discovered.join(", ") || "none"}`);
    });
    console.log(`Loaded and confirmed isolated Chrome profile extension ID ${extensionId}.`);
    await nativeHostPreflight(cdp, extensionId);
    console.log("Chrome-to-native-host hello passed.");
    console.log("The runner drives Rauser's pages itself. Answer only the prompts it names; press Ctrl+C to stop.");

    const pattern = `${origin}/*`;
    // Any empty folder will do; the suggested one is only a convenience.
    // Later checks count files, so the chosen folder must start empty.
    let notesRoot = null;
    const sameFolder = async (value) => value && notesRoot !== null &&
      await realpath(value).catch(() => null) === notesRoot;

    await step("First-run setup warning", async () => {
      await openPanel();
      await waitFor(() => onPanel(async (page) => {
        requireCondition(await page.hidden("#setup-warning") === false, "Setup warning is hidden");
        const reason = await page.text("#setup-reason");
        requireCondition(reason === "No notes folder is chosen.", `Setup warning says: ${reason}`);
      }));
      const reply = await hostConfig(wrapper);
      requireCondition(reply.config.storage === null && !reply.config.capture_enabled,
        "The isolated host configuration is not in its first-run state");
      await onPanel((page) => page.click("#open-settings"));
      await waitFor(async () => requireCondition(await cdp.targetFor(settingsUrl()), "The gear did not open settings"));
      await settingsConnected();
    });

    await step("First-run folder cancellation", async () => {
      await inSettings((page) => page.click("#choose-folder"));
      await userAction("In the macOS folder picker, click Cancel.", async () => {
        const status = await settingsStatus();
        requireCondition(status?.startsWith("Canceled"), `Settings page says: ${status}`);
        await onSettings(async (page) => {
          requireCondition(await page.text("#folder-path") === "None selected", "A folder is shown as selected");
          requireCondition(await page.enabled("#choose-folder"), "Picker is still open");
        });
      });
      const reply = await hostConfig(wrapper);
      requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
        "The isolated host configuration changed after picker cancellation");
      requireCondition(!(await maybeLstat(configPath)), "A config file appeared after picker cancellation");
    });

    await step("Canceled native consent", async () => {
      await commandOutput("/bin/sh", ["-c", `printf %s ${shellQuote(notes)} | pbcopy`]);
      await inSettings((page) => page.click("#choose-folder"));
      await userAction(`In the folder picker, choose any empty folder. The suggested one's path is on the clipboard: press Cmd+Shift+G, clear the box, paste, press Return, then click Open.\n  Path: ${notes}`,
        () => onSettings(async (page) => {
          const shown = await page.text("#folder-path");
          requireCondition(shown && shown !== "None selected", "No folder is selected yet");
          const chosen = await realpath(shown);
          const entries = (await readdir(chosen)).filter((name) => name !== ".DS_Store");
          if (entries.length) {
            throw Object.assign(new Error(`${chosen} is not empty; rerun and choose an empty folder`), { fatal: true });
          }
          notesRoot = chosen;
        }));
      console.log(`  Using notes folder ${notesRoot}`);
      await onSettings(async (page) => {
        await page.fill("#site-url", origin);
        await page.fill("#site-path", "/allowed");
      });
      await waitFor(() => onSettings(async (page) =>
        requireCondition(await page.enabled("#enable-site"), "Enable this site is disabled")));
      await inSettings((page) => page.click("#enable-site"));
      await userAction("In Chrome's prompt, click Allow. Then click No in Rauser's confirmation dialog.", async () => {
        const status = await settingsStatus();
        if (status?.includes("Chrome access was declined")) {
          throw Object.assign(new Error("Chrome access was declined; this step needs Allow in Chrome and No in Rauser"), { fatal: true });
        }
        requireCondition(status?.includes("Canceled"), `Settings page says: ${status}`);
        await onSettings(async (page) => requireCondition(await page.enabled("#enable-site"), "Setup is still running"));
      });
      const reply = await hostConfig(wrapper);
      requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
        "Capture was saved even though native consent was declined");
      const grants = await chromeGrants(pattern);
      requireCondition(!grants.origin && !grants.api, "Chrome access granted for the declined change was not removed");
    });

    await step("Confirmed setup", async () => {
      await inSettings(async (page) => {
        // Clear the previous step's result so only a new failure stops the run.
        await page.evaluate('document.getElementById("status").textContent = ""');
        await page.click("#enable-site");
      });
      await userAction("In Chrome's prompt, click Allow. Then click Yes in Rauser's confirmation dialog.", async () => {
        const status = await settingsStatus().catch(() => null);
        if (status?.startsWith("Setup failed")) {
          throw Object.assign(new Error(`${status} Rerun the smoke test; a folder selection expires after five minutes.`), { fatal: true });
        }
        const reply = await hostConfig(wrapper);
        const selectedRoot = reply.config.storage?.root;
        requireCondition(await sameFolder(selectedRoot),
          `Expected ${notesRoot}; host has ${selectedRoot ?? "none"}`);
        requireCondition(reply.config.capture_enabled === true, "Capture is not enabled in the host");
        requireCondition(reply.config.sites.length === 1 &&
          reply.config.sites[0].origin === origin && reply.config.sites[0].path_prefix === "/allowed",
        "Host site rule does not match the fixture origin and /allowed prefix");
      });
      // A first navigation grant makes the settings page restart the extension,
      // which closes every Rauser page.
      const outcome = await waitFor(async () => {
        const target = await cdp.targetFor(settingsUrl());
        if (!target) return "restarted";
        const status = await settingsStatus().catch(() => null);
        if (status?.startsWith("Capture enabled for")) return "enabled";
        if (status?.startsWith("Chrome is restarting")) throw new Error("Waiting for the extension restart");
        throw new Error(`Settings page says: ${status}`);
      }, { timeout: 20_000 });
      if (outcome === "restarted") {
        console.log("  The extension restarted to activate navigation access; reopening its pages.");
        await delay(1_000);
        await openSettingsTab();
        await settingsConnected();
      }
      await openPanel();
      await waitFor(() => onPanel(async (page) =>
        requireCondition(await page.hidden("#setup-warning") === true, "Panel still shows the setup warning")),
      { timeout: 20_000 });
    });

    const logRows = async () => {
      const files = await markdownFiles(path.join(notesRoot, "log"));
      return { files, rows: files.flatMap((file) => file.text.split("\n").filter((line) => line.startsWith("- "))) };
    };

    await step("Allowed and blocked visit replay", async () => {
      await closePanel();
      await navigate(origin, allowed);
      await navigate(origin, blocked);
      await openPanel();
      await pendingVisitsCleared();
      await waitFor(async () => {
        const { files, rows } = await logRows();
        requireCondition(rows.filter((line) => line.includes(`<${allowed}>`)).length === 1,
          "Expected exactly one allowed visit in the daily log");
        requireCondition(!files.some((file) => file.text.includes(blocked)),
          "Blocked-path visit appeared in the daily log");
      });
    });

    await step("Panel reopen does not duplicate the visit", async () => {
      await closePanel();
      await openPanel();
      await pendingVisitsCleared();
      await delay(1_000);
      const { rows } = await logRows();
      requireCondition(rows.filter((line) => line.includes(`<${allowed}>`)).length === 1,
        "Allowed visit was duplicated or removed");
    });

    let originalNote;
    await step("Page note creation", async () => {
      await navigate(origin, allowed);
      await cdp.send("Target.activateTarget", { targetId: (await fixtureTarget(origin)).targetId });
      await createNote("Page note created", title, firstBody);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      const base = files.filter((file) => !file.path.includes(".rauser-review-"));
      requireCondition(base.length === 1 && files.length === 1, "Expected one page note and no review draft");
      requireCondition(base[0].text.includes(title) && base[0].text.includes(firstBody) &&
        base[0].text.includes(allowed), "Page note content does not match the fixture request");
      originalNote = base[0];
    });

    await step("Identical page note is idempotent", async () => {
      await createNote("Page note already exists", title, firstBody);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 1 && files[0].path === originalNote.path &&
        files[0].text === originalNote.text, "Identical retry changed the note or created a file");
    });

    let reviewNote;
    let reviewResult;
    await step("Changed page note creates one review draft", async () => {
      reviewResult = await createNote("Page note needs review", title, changedBody);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      const review = files.filter((file) => file.path.includes(".rauser-review-"));
      const base = files.find((file) => file.path === originalNote.path);
      requireCondition(files.length === 2 && review.length === 1, "Expected one original note and one sibling review draft");
      requireCondition(base?.text === originalNote.text, "Original page note was changed");
      requireCondition(review[0].text.includes(changedBody) &&
        review[0].text.includes("review_of:") && review[0].text.includes("proposal_id:"),
      "Review draft does not contain the proposal and ownership metadata");
      reviewNote = review[0];
    });

    await step("Review draft retry is idempotent", async () => {
      const result = await createNote("Page note needs review", title, changedBody);
      requireCondition(result === reviewResult, `Retry reported a different result: ${result}`);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 2 &&
        files.some((file) => file.path === originalNote.path && file.text === originalNote.text) &&
        files.some((file) => file.path === reviewNote.path && file.text === reviewNote.text),
      "Review retry changed an existing file or created another draft");
    });

    await step("Site removal", async () => {
      await inSettings((page) => page.click(`button[aria-label="Remove ${origin}/allowed"]`));
      await waitFor(async () => {
        const reply = await hostConfig(wrapper);
        requireCondition(!reply.config.capture_enabled && reply.config.sites.length === 0,
          "Host still has capture enabled or the site rule");
      }, { timeout: 20_000, interval: 1_000 });
      await waitFor(async () => {
        const grants = await chromeGrants(pattern);
        requireCondition(!grants.origin && !grants.api, `Chrome still grants ${grants.origin ? origin : "webNavigation"}`);
      });
      await waitFor(() => onPanel(async (page) => {
        requireCondition(await page.hidden("#setup-warning") === false, "Panel does not show the setup warning");
        const reason = await page.text("#setup-reason");
        requireCondition(reason === "No sites are enabled for capture.", `Setup warning says: ${reason}`);
      }), { timeout: 20_000 });
    });

    await step("No capture after removal", async () => {
      await closePanel();
      await navigate(origin, afterRemoval);
      await openPanel();
      await pendingVisitsCleared();
      const { files } = await logRows();
      requireCondition(!files.some((file) => file.text.includes(afterRemoval)),
        "A visit was logged after site removal");
    });

    const cursor = await ask("\nDid a spinning busy cursor stay on screen after any Rauser dialog closed? Type no or yes: ");
    requireCondition(cursor.toLowerCase() === "no", "A busy cursor persisted after a native dialog");

    console.log("\nPASS: guided macOS Chrome M1 smoke run completed.");
  } catch (error) {
    failed = true;
    console.error(`\nSmoke run stopped: ${error.message}`);
  } finally {
    prompt.close();
    process.off("SIGINT", interrupt);
    process.off("SIGTERM", interrupt);
    try { await stopChrome(chrome); } catch (error) {
      failed = true;
      console.error(`Could not stop the isolated Chrome process group: ${error.message}`);
    }
    if (installedManifest) {
      try {
        const current = await readFile(manifest).catch((error) => {
          if (error.code === "ENOENT") return null;
          throw error;
        });
        if (!current || !current.equals(installedManifest)) {
          if (current) await writeFile(path.join(runDir, "manifest-found-during-cleanup.json"), current);
          failed = true;
          console.error("Isolated-profile host manifest changed during the run; leaving its current state untouched.");
          console.error(`Inspect ${manifest} manually.`);
        } else {
          await unlink(manifest);
          console.log("Removed the isolated-profile native-host manifest.");
        }
      } catch (error) {
        failed = true;
        console.error(`Could not remove the isolated-profile native-host manifest: ${error.message}`);
      }
    }
    try { await stopServer(server); } catch (error) {
      failed = true;
      console.error(`Could not stop the fixture server: ${error.message}`);
    }
    console.log(`Smoke artifacts preserved: ${runDir}`);
    if (failed) process.exitCode = 1;
  }
}

await run().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
