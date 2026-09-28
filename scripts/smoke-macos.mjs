#!/usr/bin/env node

// Guided real-Chrome M1 smoke run. Native dialogs and Chrome permission prompts
// are answered by a person, or with --auto through UI scripting; all host and
// vault state is kept in a temp home. --headless runs the same flow with
// nothing on screen: headless Chrome, a scripted-dialogs host build, and
// Chrome access granted ahead of time instead of through Chrome's prompt.
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import {
  access, chmod, cp, lstat, mkdir, mkdtemp, open, readFile,
  readdir, realpath, rename, unlink, writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline/promises";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { APP_NAME, BINARY_NAME, NATIVE_HOST_NAME } from "../extension/brand.ts";

const REPO = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const DIST = path.join(REPO, "extension", "dist");
const DEFAULT_HOST = path.join(REPO, "target", "debug", BINARY_NAME);
// A separate target directory keeps the scripted build away from the host a
// developer registers for everyday use.
const SCRIPTED_HOST = path.join(REPO, "target", "scripted-dialogs", "debug", BINARY_NAME);
const SCRIPTED_BUILD = `cargo build --locked -p ${BINARY_NAME} --features scripted-dialogs --target-dir target/scripted-dialogs`;
const SCRIPTED_DIALOGS_ENV = `${BINARY_NAME.toUpperCase()}_SCRIPTED_DIALOGS`;
const DEFAULT_CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const DEV_DESCRIPTION = `${APP_NAME} development native host`;
const EXTENSION_ID = /^[a-p]{32}$/;
const EXTENSION_ID_ALPHABET = "abcdefghijklmnop";

function usage() {
  console.log(`Usage: node scripts/smoke-macos.mjs [--host /absolute/path/to/${BINARY_NAME}] [--chrome /absolute/path/to/Google Chrome] [--default-port] [--auto | --headless]`);
  console.log("  --default-port  Serve the fixture on port 80 and enter the site as http://127.0.0.1:80.");
  console.log("  --auto          Answer the prompts through macOS UI scripting; the terminal needs Accessibility access.");
  console.log(`  --headless      Show nothing on screen: headless Chrome and scripted dialogs. Build the host with:\n                  ${SCRIPTED_BUILD}`);
}

function argumentsForRun(args) {
  let host = null;
  let chrome = DEFAULT_CHROME;
  let defaultPort = false;
  let auto = false;
  let headless = false;
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--help") {
      usage();
      return null;
    }
    if (args[index] === "--default-port") {
      defaultPort = true;
      continue;
    }
    if (args[index] === "--auto") {
      auto = true;
      continue;
    }
    if (args[index] === "--headless") {
      headless = true;
      continue;
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
  if (auto && headless) throw new Error("Choose either --auto or --headless");
  return { host: host ?? (headless ? SCRIPTED_HOST : DEFAULT_HOST), chrome, defaultPort, auto, headless };
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

// --auto answers the prompts through macOS UI scripting, which needs the
// terminal to hold Accessibility access. The host's folder picker does not
// answer Accessibility queries, so it is driven by keyboard; its Yes/No
// alert is drawn by UserNotificationCenter, which does.
async function osascript(script, language = "AppleScript") {
  return commandOutput("/usr/bin/osascript", ["-l", language, "-e", script]);
}

async function requireAccessibility() {
  const enabled = await osascript('tell application "System Events" to get UI elements enabled');
  requireCondition(enabled === "true" && await osascript(
    'tell application "System Events" to get name of first process whose frontmost is true',
  ).then(() => true, () => false),
  "--auto needs Accessibility access for this terminal: System Settings > Privacy & Security > Accessibility");
}

async function onScreenWindows(pid) {
  const listed = await osascript(`ObjC.import("CoreGraphics");
    JSON.stringify(ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)))
      .filter((window) => window.kCGWindowOwnerPID === ${Number(pid)}).length)`, "JavaScript");
  return Number(listed);
}

async function pickerPid() {
  const pids = await commandOutput("/usr/bin/pgrep", ["-f", `${BINARY_NAME} __dialog-pick-folder`]).catch(() => "");
  const list = pids.split("\n").filter(Boolean);
  requireCondition(list.length === 1, list.length ? "More than one folder picker is open" : "The folder picker is not open yet");
  requireCondition(await onScreenWindows(list[0]) > 0, "The folder picker window is not on screen yet");
  return Number(list[0]);
}

// Keystrokes go to the frontmost process. System Events cannot report the
// unbundled picker process as frontmost, so activate it and wait.
async function keysToPicker(pid, keys) {
  await osascript(`tell application "System Events"
    set frontmost of (first process whose unix id is ${pid}) to true
    delay 1
    ${keys}
  end tell`);
}

async function cancelPicker() {
  const pid = await waitFor(pickerPid, { timeout: 20_000 });
  await keysToPicker(pid, "key code 53");
}

async function choosePickerFolder(folder) {
  const pid = await waitFor(pickerPid, { timeout: 20_000 });
  const closed = async () => requireCondition(
    !(await commandOutput("/bin/ps", ["-p", String(pid)]).then(() => true, () => false)), "The folder picker is still open");
  // Early keystrokes can be lost while the picker takes focus; if it is still
  // open, repeat the whole sequence, which is safe from either picker state.
  for (let attempt = 1; ; attempt += 1) {
    await keysToPicker(pid, `keystroke "g" using {command down, shift down}
      delay 1.5
      keystroke "a" using {command down}
      keystroke ${JSON.stringify(folder)}
      delay 1
      key code 36
      delay 1.5
      key code 36`);
    try {
      return await waitFor(closed, { timeout: 5_000 });
    } catch (error) {
      if (attempt === 3) throw error;
    }
  }
}

async function answerAlert(button) {
  await waitFor(() => osascript(`tell application "System Events"
    click button ${JSON.stringify(button)} of (first window of process "UserNotificationCenter" whose subrole is "AXSystemDialog")
  end tell`), { timeout: 20_000 });
}

// Chrome's permission prompt is a sheet on the settings window.
async function allowInChrome(press, pid) {
  await waitFor(() => commandOutput(press, [String(pid), "Allow"]).catch((error) => {
    throw new Error(error.message.includes("exited 2") ? "Chrome's Allow button is not showing" : error.message);
  }), { timeout: 20_000 });
}

// Spawn a short-lived host process for one framed request, independent of
// whatever host process is serving the live panel or settings connection.
// The host's config-file lock (§7) serializes these against each other.
function hostRequest(wrapper, request) {
  const body = Buffer.from(JSON.stringify(request));
  const length = Buffer.alloc(4);
  length.writeUInt32LE(body.length);
  return new Promise((resolve, reject) => {
    const child = spawn(wrapper, ["serve"], { stdio: ["pipe", "pipe", "pipe"] });
    const output = [];
    const errors = [];
    let settled = false;
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      finish(new Error(`Native host ${request.type} timed out`));
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
        if (code !== 0) throw new Error(`Native host ${request.type} failed: ${Buffer.concat(errors).toString("utf8").trim()}`);
        const frame = Buffer.concat(output);
        if (frame.length < 4 || frame.readUInt32LE(0) !== frame.length - 4) {
          throw new Error("Native host returned an invalid response frame");
        }
        const reply = JSON.parse(frame.subarray(4).toString("utf8"));
        if (reply.protocol_version !== 3) {
          throw new Error(`Native host returned ${reply.type ?? "an unknown response"}`);
        }
        finish(null, reply);
      } catch (error) {
        finish(error);
      }
    });
    child.stdin.end(Buffer.concat([length, body]));
  });
}

async function hostConfig(wrapper) {
  const reply = await hostRequest(wrapper, {
    type: "get_config", protocol_version: 3, request_id: randomUUID(),
  });
  if (reply.type !== "config_result") {
    throw new Error(`Native host returned ${reply.type ?? "an unknown response"} instead of config_result`);
  }
  return reply;
}

// Simulates a second Brauser window saving this page's note first, so the
// panel's own next save is refused as stale (§4.4, §5.3).
async function hostSaveNote(wrapper, url, title, body, expectedRevision) {
  const reply = await hostRequest(wrapper, {
    type: "save_note", protocol_version: 3, request_id: randomUUID(),
    url, title, body, expected_revision: expectedRevision,
  });
  if (reply.type !== "note_saved") {
    throw new Error(`Native host returned ${reply.type ?? "an unknown response"} instead of note_saved`);
  }
  return reply;
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
      const port = chrome.runtime.connectNative("${NATIVE_HOST_NAME}");
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
      port.postMessage({type: "hello", protocol_version: 3, request_id: "smoke-preflight"});
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
    const title = pathname === "/" ? `${APP_NAME} smoke fixture` : `${APP_NAME} smoke ${pathname}`;
    response.end(`<!doctype html><html><head><title>${title}</title></head><body><h1>${title}</h1><p>Run ${caseId}</p><ul><li><a href="/allowed/page?case=${caseId}">Allowed page</a></li><li><a href="/blocked/page?case=${caseId}">Blocked page</a></li><li><a href="/allowed/after-removal?case=${caseId}">After removal</a></li></ul></body></html>`);
  });
  return server;
}

async function startServer(server, defaultPort) {
  if (defaultPort) {
    // macOS lets an unprivileged process bind port 80 only on the wildcard
    // address, so refuse every connection that is not from this machine.
    server.on("connection", (socket) => {
      if (!["127.0.0.1", "::ffff:127.0.0.1"].includes(socket.remoteAddress ?? "")) socket.destroy();
    });
  }
  await new Promise((resolve, reject) => {
    server.once("error", (error) => reject(defaultPort && error.code === "EADDRINUSE"
      ? new Error("Port 80 is already in use; stop that server or run without --default-port")
      : error));
    server.listen(defaultPort ? 80 : 0, defaultPort ? "0.0.0.0" : "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Cannot determine fixture port");
  return defaultPort ? "http://127.0.0.1" : `http://127.0.0.1:${address.port}`;
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
  const build = options.headless ? SCRIPTED_BUILD : `cargo build --locked -p ${BINARY_NAME}`;
  await access(options.host, constants.X_OK).catch(() => {
    throw new Error(`Build the native host first: ${build} (${options.host} is unavailable)`);
  });
  // A host without the seam would open real dialogs, so check before any runs.
  const scriptedHost = (await commandOutput(options.host, ["--version"])).endsWith("(scripted dialogs)");
  if (options.headless) {
    requireCondition(scriptedHost, `--headless needs a scripted-dialogs host: ${SCRIPTED_BUILD}`);
  } else {
    requireCondition(!scriptedHost, "This host answers dialogs from files; use --headless or a normal build");
  }
  await access(options.chrome, constants.X_OK);
  if (options.auto) await requireAccessibility();
  await access(path.join(DIST, "manifest.json"), constants.R_OK).catch(() => {
    throw new Error(`Build the extension first: npm run build:extension (${DIST} is unavailable)`);
  });

  const runDir = await mkdtemp(path.join(os.tmpdir(), `${BINARY_NAME}-smoke-macos-`));
  const testHome = path.join(runDir, "home");
  const profile = path.join(runDir, "chrome-profile");
  const notes = path.join(runDir, "notes");
  const wrapper = path.join(runDir, `${BINARY_NAME}-host-wrapper`);
  const press = path.join(runDir, "smoke-macos-press");
  const windowCheck = path.join(runDir, "smoke-macos-windows");
  const dialogs = path.join(runDir, "dialogs");
  // Headless Chrome loads a copy whose first load carries webNavigation; see
  // preGrantChromeAccess.
  const extensionCopy = path.join(runDir, "extension");
  const manifest = path.join(profile, "NativeMessagingHosts", `${NATIVE_HOST_NAME}.json`);
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

  // Only Chrome's permission prompt and the host's native dialogs need a
  // person, or with --auto, the given automation.
  async function userAction(instruction, check, automate) {
    if ((options.auto || options.headless) && automate) {
      console.log(`  ${options.headless ? "SCRIPTED" : "AUTO"}: ${instruction}`);
      await automate();
      return waitFor(check, { timeout: 30_000, interval: 500, signal: abort.signal });
    }
    if (options.headless) {
      // Nobody can see a headless browser, so never wait for a person.
      throw Object.assign(new Error(`--headless cannot do this step: ${instruction}`), { fatal: true });
    }
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
      await userAction(`Click the ${APP_NAME} toolbar button to open the side panel.`,
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
      await userAction(`Close the ${APP_NAME} side panel.`, closed);
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

  // Chrome skips its prompt when it already holds the requested access, so a
  // missing prompt is reported, and the step's own checks decide the result.
  async function chromePrompt() {
    if (options.headless) {
      console.log("  No Chrome prompt in --headless: its access was granted before the run.");
      return;
    }
    try {
      await allowInChrome(press, chrome.pid);
      console.log("  Clicked Allow in Chrome's prompt.");
    } catch (error) {
      const status = await settingsStatus().catch(() => "(settings page unavailable)");
      console.log(`  No Chrome prompt was clicked: ${error.message}. Settings page says: ${status}`);
    }
  }

  // --headless: every Chrome and host process must stay invisible, with no
  // on-screen window (menu-bar items included) and no Dock presence.
  async function nothingOnScreen() {
    const listed = await Promise.all([
      commandOutput("/usr/bin/pgrep", ["-g", String(chrome.pid)]).catch(() => ""),
      commandOutput("/usr/bin/pgrep", ["-f", options.host]).catch(() => ""),
    ]);
    const pids = [...new Set(listed.join("\n").split("\n").filter(Boolean))];
    const seen = JSON.parse(await commandOutput(windowCheck, pids));
    requireCondition(seen.windows === 0 && seen.apps === 0,
      `A test process is visible: ${seen.windows} on-screen windows, ${seen.apps} Dock apps ` +
      `(${JSON.stringify(seen.visible)})`);
  }

  // --headless: the host's dialog child records each dialog in shown.jsonl
  // and waits for an answer file. Answer only the dialog the step expects.
  let dialogsShown = 0;
  async function scriptedDialog(dialog, reply) {
    const shown = await waitFor(async () => {
      const lines = (await readFile(path.join(dialogs, "shown.jsonl"), "utf8").catch(() => ""))
        .split("\n").filter(Boolean);
      requireCondition(lines.length > dialogsShown, `The ${dialog} dialog has not opened`);
      return JSON.parse(lines[dialogsShown]);
    }, { timeout: 20_000, signal: abort.signal });
    dialogsShown += 1;
    requireCondition(shown.dialog === dialog, `Expected the ${dialog} dialog; the host opened ${shown.dialog}`);
    await nothingOnScreen();
    await atomicWrite(path.join(dialogs, "answer"), `${JSON.stringify({ dialog, reply })}\n`);
    await waitFor(async () => requireCondition(!(await maybeLstat(path.join(dialogs, "answer"))),
      `The host did not take the ${dialog} answer`), { signal: abort.signal });
    return shown.text;
  }

  const dismissPicker = () => options.headless ? scriptedDialog("pick-folder", "canceled") : cancelPicker();
  const pickFolder = (folder) => options.headless
    ? scriptedDialog("pick-folder", `picked:${folder}`) : choosePickerFolder(folder);
  async function answerConfirmation(button, site) {
    if (!options.headless) return answerAlert(button);
    const text = await scriptedDialog("confirm", button === "Yes" ? "confirmed" : "canceled");
    // The host writes this text from the policy change itself.
    requireCondition(text.includes(`Allow visit logging for ${JSON.stringify(site)}`) &&
      text.includes("Turn on automatic visit logging."),
    `The confirmation does not describe the change: ${JSON.stringify(text)}`);
  }

  // --headless: Chrome's permission prompt is browser UI that no command-line
  // switch or DevTools method can answer, so grant the access ahead of time
  // through Chrome's own mechanisms. webNavigation stays active when an
  // extension version that required it is replaced by one that makes it
  // optional, and chrome://extensions grants a site as a user would under
  // Site access. permissions.request then resolves without a prompt, and
  // permissions.remove still revokes both.
  async function loadExtension(directory) {
    if (!options.headless) return cdp.send("Extensions.loadUnpacked", { path: directory });
    const manifestFile = path.join(directory, "manifest.json");
    const shipped = await readFile(manifestFile, "utf8");
    const earlier = JSON.parse(shipped);
    earlier.permissions = [...earlier.permissions, "webNavigation"];
    await writeFile(manifestFile, JSON.stringify(earlier, null, 2));
    await cdp.send("Extensions.loadUnpacked", { path: directory });
    await writeFile(manifestFile, shipped);
    return cdp.send("Extensions.loadUnpacked", { path: directory });
  }

  async function preGrantSite(pattern) {
    const { targetId } = await cdp.send("Target.createTarget", { url: "chrome://extensions", background: true });
    try {
      await cdp.withTarget(targetId, async (page) => {
        await waitFor(() => page.evaluate("typeof chrome.developerPrivate?.addHostPermission === 'function' || Promise.reject(new Error('loading'))"));
        await page.evaluate(`chrome.developerPrivate.addHostPermission(${JSON.stringify(extensionId)}, ${JSON.stringify(pattern)})`);
      });
    } finally {
      await cdp.send("Target.closeTarget", { targetId }).catch(() => undefined);
    }
  }

  async function typeNote(text) {
    await waitFor(() => onPanel(async (page) =>
      requireCondition(await page.enabled("#note-body"), "The note editor is disabled")));
    await onPanel((page) => page.fill("#note-body", text));
  }

  async function waitForNoteStatus(expected) {
    return waitFor(() => onPanel(async (page) => {
      const shown = await page.text("#note-status");
      requireCondition(shown === expected, `Panel note status: ${shown || "(none yet)"}`);
    }), { timeout: 20_000 });
  }

  function fieldValue(selector) {
    return onPanel((page) => page.evaluate(`document.getElementById(${JSON.stringify(selector)}).value`));
  }

  try {
    await Promise.all([mkdir(testHome), mkdir(notes), mkdir(profile)]);
    if (options.auto) {
      await commandOutput("/usr/bin/swiftc", ["-O", path.join(REPO, "scripts", "smoke-macos-press.swift"), "-o", press]);
    }
    let scriptedEnv = "";
    if (options.headless) {
      await mkdir(dialogs, { mode: 0o700 });
      scriptedEnv = `export ${SCRIPTED_DIALOGS_ENV}=${shellQuote(dialogs)}\n`;
      await commandOutput("/usr/bin/swiftc", ["-O", path.join(REPO, "scripts", "smoke-macos-windows.swift"), "-o", windowCheck]);
    }
    await writeFile(wrapper, `#!/bin/sh\nexport HOME=${shellQuote(testHome)}\n${scriptedEnv}exec ${shellQuote(options.host)} "$@"\n`, { mode: 0o700 });
    await chmod(wrapper, 0o700);
    const configPath = await commandOutput(wrapper, ["--config-path"]);
    const actualHome = await realpath(testHome);
    requireCondition(path.isAbsolute(configPath) &&
      (inside(testHome, configPath) || inside(actualHome, configPath)),
    `Host config would escape the temporary home: ${configPath}`);
    if (options.headless) await cp(DIST, extensionCopy, { recursive: true });
    const dist = await realpath(options.headless ? extensionCopy : DIST);
    extensionId = unpackedExtensionId(dist);
    const hostManifest = Buffer.from(`${JSON.stringify({
      name: NATIVE_HOST_NAME,
      description: DEV_DESCRIPTION,
      path: wrapper,
      type: "stdio",
      allowed_origins: [`chrome-extension://${extensionId}/`],
    }, null, 2)}\n`);
    await atomicWrite(manifest, hostManifest);
    installedManifest = hostManifest;
    console.log(`Predicted unpacked extension ID: ${extensionId}`);
    console.log(`Isolated-profile native host registered for ${extensionId} before Chrome launch.`);

    const origin = await startServer(server, options.defaultPort);
    // The URL typed into settings; the default-port run spells out :80.
    const siteInput = options.defaultPort ? `${origin}:80` : origin;
    const allowed = `${origin}/allowed/page?case=${caseId}`;
    const blocked = `${origin}/blocked/page?case=${caseId}`;
    const afterRemoval = `${origin}/allowed/after-removal?case=${caseId}`;
    // The panel titles a page note from the tab, so this is the fixture page title.
    const title = `${APP_NAME} smoke /allowed/page`;
    const firstBody = `First note ${caseId}`;
    await writeFile(path.join(runDir, "run-info.json"), `${JSON.stringify({
      origin, allowed, blocked, afterRemoval, notes, profile, testHome, configPath,
      manifest, host: options.host, chrome: options.chrome, siteInput,
    }, null, 2)}\n`);

    console.log(`\nSmoke artifacts: ${runDir}`);
    console.log(`Fixture: ${origin}${options.defaultPort ? " (port 80, entered as " + siteInput + ")" : ""}`);
    console.log(`Notes folder: ${notes}`);
    console.log(`Extension folder: ${dist}`);
    console.log(`Loading the unpacked extension into the isolated ${options.headless ? "headless " : ""}Chrome profile.`);
    chrome = spawn(options.chrome, [
      ...(options.headless ? ["--headless=new"] : []),
      `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check",
      "--remote-debugging-pipe", "--enable-unsafe-extension-debugging",
      // Headless Chrome accepts one start page; it opens chrome://extensions later.
      ...(options.headless ? [] : ["chrome://extensions"]), `${origin}/`,
    ], { detached: true, stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    await new Promise((resolve, reject) => {
      chrome.once("spawn", resolve);
      chrome.once("error", reject);
    });

    cdp = new Cdp(chrome);
    const loaded = await loadExtension(dist);
    const loadedId = loaded?.id;
    requireCondition(loadedId === extensionId,
      `Chrome loaded extension ID ${loadedId ?? "none"}; expected ${extensionId}`);
    await waitFor(async () => {
      const discovered = await extensionIdsInProfile(profile, dist);
      requireCondition(discovered.length === 1 && discovered[0] === extensionId,
        `Expected ${extensionId} for ${dist} in this isolated Chrome profile; found ${discovered.join(", ") || "none"}`);
    });
    console.log(`Loaded and confirmed isolated Chrome profile extension ID ${extensionId}.`);
    await nativeHostPreflight(cdp, extensionId);
    console.log("Chrome-to-native-host hello passed.");
    console.log(`The runner drives ${APP_NAME}'s pages itself. Answer only the prompts it names; press Ctrl+C to stop.`);

    // The grant the settings page requests: always an explicit port, never
    // Chrome's any-port form, which the run checks is not granted.
    const pattern = `${origin}${options.defaultPort ? ":80" : ""}/*`;
    const anyPortPattern = `${new URL(origin).protocol}//${new URL(origin).hostname}/*`;
    if (options.headless) {
      await preGrantSite(pattern);
      await nothingOnScreen();
      console.log(`Granted ${pattern} and webNavigation ahead of time; nothing is on screen.`);
    }
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
        if (status?.startsWith("Folder selected")) {
          throw Object.assign(new Error("A folder was chosen, but this step needs Cancel. Rerun the smoke test."), { fatal: true });
        }
        requireCondition(status?.startsWith("Canceled"), `Settings page says: ${status}`);
        await onSettings(async (page) => {
          requireCondition(await page.text("#folder-path") === "None selected", "A folder is shown as selected");
          requireCondition(await page.enabled("#choose-folder"), "Picker is still open");
        });
      }, dismissPicker);
      const reply = await hostConfig(wrapper);
      requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
        "The isolated host configuration changed after picker cancellation");
      requireCondition(!(await maybeLstat(configPath)), "A config file appeared after picker cancellation");
    });

    await step("Canceled native consent", async () => {
      // The clipboard is the user's; only a person choosing by hand needs it.
      if (!options.headless) await commandOutput("/bin/sh", ["-c", `printf %s ${shellQuote(notes)} | pbcopy`]);
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
        }), () => pickFolder(notes));
      console.log(`  Using notes folder ${notesRoot}`);
      await onSettings(async (page) => {
        await page.fill("#site-url", siteInput);
        await page.fill("#site-path", "/allowed");
      });
      await waitFor(() => onSettings(async (page) =>
        requireCondition(await page.enabled("#enable-site"), "Enable this site is disabled")));
      await inSettings((page) => page.click("#enable-site"));
      await userAction(`In Chrome's prompt, click Allow. Then click No in ${APP_NAME}'s confirmation dialog.`, async () => {
        const status = await settingsStatus();
        if (status?.includes("Chrome access was declined")) {
          throw Object.assign(new Error(`Chrome access was declined; this step needs Allow in Chrome and No in ${APP_NAME}`), { fatal: true });
        }
        requireCondition(status?.includes("Canceled"), `Settings page says: ${status}`);
        await onSettings(async (page) => requireCondition(await page.enabled("#enable-site"), "Setup is still running"));
      }, async () => {
        await chromePrompt();
        await answerConfirmation("No", `${origin}/allowed`);
      });
      const reply = await hostConfig(wrapper);
      requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
        "Capture was saved even though native consent was declined");
      const grants = await chromeGrants(pattern);
      // Headless Chrome already held webNavigation, so the declined change
      // correctly keeps it; site removal later must revoke it.
      requireCondition(!grants.origin && grants.api === options.headless,
        "Chrome access granted for the declined change was not removed");
    });

    await step("Confirmed setup", async () => {
      await inSettings(async (page) => {
        // Clear the previous step's result so only a new failure stops the run.
        await page.evaluate('document.getElementById("status").textContent = ""');
        await page.click("#enable-site");
      });
      await userAction(`If Chrome prompts, click Allow; it may re-grant the access it just removed without asking. Then click Yes in ${APP_NAME}'s confirmation dialog.`, async () => {
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
      }, async () => {
        await chromePrompt();
        await answerConfirmation("Yes", `${origin}/allowed`);
      });
      // A first navigation grant makes the settings page restart the extension,
      // which closes every extension page.
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
      const granted = await chromeGrants(pattern);
      requireCondition(granted.origin && granted.api, `Chrome did not grant ${pattern} and webNavigation`);
      requireCondition(!(await chromeGrants(anyPortPattern)).origin,
        `Chrome granted every port (${anyPortPattern}), not just ${pattern}`);
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

    await step("Note autosave writes the whole note about a second after typing stops", async () => {
      await navigate(origin, allowed);
      await cdp.send("Target.activateTarget", { targetId: (await fixtureTarget(origin)).targetId });
      await openPanel();
      await typeNote(firstBody);
      // Autosave debounces ~1s after typing stops (§5.3); 20s covers that plus
      // the native round trip with room to spare.
      await waitForNoteStatus("Saved");
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 1, "Expected exactly one page note");
      requireCondition(files[0].text.includes(title) && files[0].text.includes(firstBody) &&
        files[0].text.includes(allowed), "Page note content does not match the fixture page");
    });

    await step("The note persists and reloads across navigating the page away and back", async () => {
      const appended = `${firstBody} plus an edit before leaving the page ${caseId}`;
      await typeNote(appended);
      await waitForNoteStatus("Saved");
      // The panel follows the active tab (§5.7): leaving and returning to
      // the page must flush any edit and load the saved note again.
      await navigate(origin, blocked);
      await navigate(origin, allowed);
      await waitForNoteStatus("Saved");
      const shown = await fieldValue("note-body");
      requireCondition(shown === appended, `Note editor shows: ${shown}`);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 1 && files[0].text.includes(appended),
        "Navigating away and back did not preserve the saved note");
    });

    let externalBody;
    await step("A refused stale save shows the current note and keeps the unsaved text", async () => {
      const loaded = await hostRequest(wrapper, {
        type: "load_note", protocol_version: 3, request_id: randomUUID(), url: allowed,
      });
      requireCondition(loaded.type === "note_loaded" && loaded.exists,
        `Expected the note saved above to load; got ${loaded.type}`);

      // Simulate a second Brauser window saving this page's note first, so
      // the open panel's own revision becomes stale (§4.4).
      externalBody = `External edit ${caseId}`;
      await hostSaveNote(wrapper, allowed, title, externalBody, loaded.revision);

      const unsavedLocalEdit = `${firstBody} plus an edit that cannot be saved ${caseId}`;
      await typeNote(unsavedLocalEdit);
      await waitFor(() => onPanel(async (page) => {
        const hidden = await page.hidden("#note-conflict");
        requireCondition(hidden === false, "The conflict copy-out box did not appear");
      }), { timeout: 20_000 });
      const [unsaved, shown] = await Promise.all([fieldValue("note-unsaved"), fieldValue("note-body")]);
      requireCondition(unsaved === unsavedLocalEdit, `Unsaved copy-out shows: ${unsaved}`);
      requireCondition(shown === externalBody, `Note editor shows: ${shown}`);
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 1 && files[0].text.includes(externalBody) &&
        !files[0].text.includes(unsavedLocalEdit), "The refused save must not have changed the note file");
    });

    await step("Editing again after a conflict saves against the now-current revision", async () => {
      const recovered = `${externalBody} plus the recovered edit ${caseId}`;
      await typeNote(recovered);
      await waitForNoteStatus("Saved");
      const files = await markdownFiles(path.join(notesRoot, "pages"));
      requireCondition(files.length === 1 && files[0].text.includes(recovered),
        "The recovered edit was not saved after the conflict");
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

    if (options.headless) {
      await step("Nothing reached the screen", async () => {
        const pids = (await commandOutput("/usr/bin/pgrep", ["-f", `${options.host} __dialog-`]).catch(() => ""))
          .split("\n").filter(Boolean);
        requireCondition(pids.length === 0, "A dialog process is still running");
        requireCondition(dialogsShown === 4, `Expected four scripted dialogs; answered ${dialogsShown}`);
        requireCondition(!(await maybeLstat(path.join(dialogs, "answer"))), "An unused scripted answer remains");
        await nothingOnScreen();
      });
    } else if (options.auto) {
      // The busy cursor came from a host process left holding a dialog
      // window, so check that no dialog process or host window remains.
      await step("No host dialog left behind", async () => {
        const pids = (await commandOutput("/usr/bin/pgrep", ["-f", options.host]).catch(() => ""))
          .split("\n").filter(Boolean);
        const commands = await Promise.all(pids.map((pid) =>
          commandOutput("/bin/ps", ["-o", "command=", "-p", pid]).catch(() => "")));
        requireCondition(!commands.some((command) => command.includes("__dialog-")), "A native dialog process is still running");
        const windows = await Promise.all(pids.map(onScreenWindows));
        requireCondition(windows.every((count) => count === 0), "A host process still has a window on screen");
      });
    } else {
      const cursor = await ask(`\nDid a spinning busy cursor stay on screen after any ${APP_NAME} dialog closed? Type no or yes: `);
      requireCondition(cursor.toLowerCase() === "no", "A busy cursor persisted after a native dialog");
    }

    console.log(`\nPASS: ${options.headless ? "headless" : "guided"} macOS Chrome M1 smoke run completed.`);
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
