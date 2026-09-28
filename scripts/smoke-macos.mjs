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

async function cdpCall(chrome, id, method, params = {}, sessionId) {
  const outgoing = chrome.stdio[3];
  const incoming = chrome.stdio[4];
  if (!outgoing || !incoming) throw new Error("Chrome DevTools pipe is unavailable");
  return new Promise((resolve, reject) => {
    let pending = Buffer.alloc(0);
    let settled = false;
    const timer = setTimeout(() => finish(new Error(`Chrome ${method} timed out`)), 15_000);
    function finish(error, result) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      incoming.off("data", onData);
      incoming.off("error", onError);
      chrome.off("exit", onExit);
      if (error) reject(error);
      else resolve(result);
    }
    function onError(error) { finish(error); }
    function onExit(code) { finish(new Error(`Chrome exited during ${method} (${code})`)); }
    function onData(chunk) {
      pending = Buffer.concat([pending, chunk]);
      for (;;) {
        const end = pending.indexOf(0);
        if (end < 0) return;
        let message;
        try { message = JSON.parse(pending.subarray(0, end).toString("utf8")); }
        catch (error) { finish(new Error(`Invalid Chrome DevTools response: ${error.message}`)); return; }
        pending = pending.subarray(end + 1);
        if (message.id !== id) continue;
        if (message.error) finish(new Error(`Chrome ${method} failed: ${message.error.message ?? JSON.stringify(message.error)}`));
        else finish(null, message.result);
        return;
      }
    }
    incoming.on("data", onData);
    incoming.on("error", onError);
    chrome.once("exit", onExit);
    outgoing.write(`${JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) })}\0`,
      (error) => { if (error) finish(error); });
  });
}

async function nativeHostPreflight(chrome, extensionId) {
  const target = await cdpCall(chrome, 2, "Target.createTarget", {
    url: `chrome-extension://${extensionId}/panel.html`, background: true,
  });
  try {
    const attached = await cdpCall(chrome, 3, "Target.attachToTarget", {
      targetId: target.targetId, flatten: true,
    });
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
    let lastError = "extension page did not load";
    for (let attempt = 0; attempt < 20; attempt += 1) {
      const reply = await cdpCall(chrome, 4 + attempt, "Runtime.evaluate", {
        expression, awaitPromise: true, returnByValue: true,
      }, attached.sessionId);
      if (!reply.exceptionDetails && reply.result?.value?.ok === true) return;
      lastError = reply.result?.value?.error ?? reply.exceptionDetails?.text ??
        `unexpected host response ${reply.result?.value?.type ?? "none"}`;
      if (!reply.exceptionDetails || !/connectNative/.test(lastError)) break;
      await delay(250);
    }
    throw new Error(`Chrome could not connect to the native host: ${lastError}`);
  } finally {
    await cdpCall(chrome, 30, "Target.closeTarget", { targetId: target.targetId }).catch(() => undefined);
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

async function waitFor(check) {
  let lastError;
  for (let attempt = 0; attempt < 30; attempt += 1) {
    try { return await check(); } catch (error) { lastError = error; }
    await delay(400);
  }
  throw lastError;
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

  async function checkpoint(title, instructions, check) {
    console.log(`\n${title}\n${instructions}`);
    for (;;) {
      await ask("Press Enter to check, or q to stop: ");
      try {
        await check();
        console.log(`PASS: ${title}`);
        return;
      } catch (error) {
        console.error(`Not yet: ${error.message}`);
      }
    }
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
    const extensionId = unpackedExtensionId(dist);
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

    const loaded = await cdpCall(chrome, 1, "Extensions.loadUnpacked", { path: dist });
    const loadedId = loaded?.id;
    requireCondition(loadedId === extensionId,
      `Chrome loaded extension ID ${loadedId ?? "none"}; expected ${extensionId}`);
    await waitFor(async () => {
      const discovered = await extensionIdsInProfile(profile, dist);
      requireCondition(discovered.length === 1 && discovered[0] === extensionId,
        `Expected ${extensionId} for ${DIST} in this isolated Chrome profile; found ${discovered.join(", ") || "none"}`);
    });
    console.log(`Loaded and confirmed isolated Chrome profile extension ID ${extensionId}.`);
    await nativeHostPreflight(chrome, extensionId);
    console.log("Chrome-to-native-host hello passed.");
    console.log("Click the Rauser toolbar action to open its side panel.");

    await checkpoint("First-run folder cancellation",
      "Click Choose folder, cancel the native picker, and check that the panel still says None selected.",
      async () => {
        const reply = await hostConfig(wrapper);
        requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
          "The isolated host configuration changed after picker cancellation");
        requireCondition(!(await maybeLstat(configPath)), "A config file appeared after picker cancellation");
      });

    await checkpoint("Canceled native consent",
      `Choose ${notes}; enter Site URL ${origin} and Allowed path prefix /allowed. Click Enable this site, accept Chrome access, then decline the native confirmation. The panel should report cancellation.`,
      async () => {
        const reply = await hostConfig(wrapper);
        requireCondition(reply.config.storage === null && !reply.config.capture_enabled && reply.config.sites.length === 0,
          "Capture was saved even though native consent was declined");
      });

    await checkpoint("Confirmed setup",
      "Click Enable this site again, accept Chrome access and the native confirmation. If the selection token expired, choose the notes folder again. Reopen the panel if Chrome reloads the extension.",
      async () => {
        const reply = await hostConfig(wrapper);
        const selectedRoot = reply.config.storage?.root;
        requireCondition(selectedRoot && await realpath(selectedRoot) === await realpath(notes),
          `Expected the isolated notes folder; host has ${selectedRoot ?? "none"}`);
        requireCondition(reply.config.capture_enabled === true, "Capture is not enabled in the host");
        requireCondition(reply.config.sites.length === 1 &&
          reply.config.sites[0].origin === origin && reply.config.sites[0].path_prefix === "/allowed",
        "Host site rule does not match the fixture origin and /allowed prefix");
      });

    await checkpoint("Allowed and blocked visit replay",
      `Close the Rauser panel. Navigate to ${allowed}, then ${blocked}. Reopen the panel and wait until it shows 0 pending visits; click Send pending visits if needed.`,
      async () => waitFor(async () => {
        const files = await markdownFiles(path.join(notes, "log"));
        const rows = files.flatMap((file) => file.text.split("\n").filter((line) => line.startsWith("- ")));
        requireCondition(rows.filter((line) => line.includes(`<${allowed}>`)).length === 1,
          "Expected exactly one allowed visit in the daily log");
        requireCondition(!files.some((file) => file.text.includes(blocked)),
          "Blocked-path visit appeared in the daily log");
      }));

    await checkpoint("Panel reopen does not duplicate the visit",
      "Close and reopen the side panel without navigating again. Wait for 0 pending visits.",
      async () => {
        const files = await markdownFiles(path.join(notes, "log"));
        const rows = files.flatMap((file) => file.text.split("\n").filter((line) => line.startsWith("- ")));
        requireCondition(rows.filter((line) => line.includes(`<${allowed}>`)).length === 1,
          "Allowed visit was duplicated or removed");
      });

    let originalNote;
    await checkpoint("Page note creation",
      `Navigate to ${allowed}. In the panel, enter Title: ${title} and Your note: ${firstBody}; click Create page note.`,
      async () => {
        const files = await markdownFiles(path.join(notes, "pages"));
        const base = files.filter((file) => !file.path.includes(".rauser-review-"));
        requireCondition(base.length === 1 && files.length === 1, "Expected one page note and no review draft");
        requireCondition(base[0].text.includes(title) && base[0].text.includes(firstBody) &&
          base[0].text.includes(allowed), "Page note content does not match the fixture request");
        originalNote = base[0];
      });

    await checkpoint("Identical page note is idempotent",
      "Click Create page note again with the same title and body. The panel should say Page note already exists.",
      async () => {
        const files = await markdownFiles(path.join(notes, "pages"));
        requireCondition(files.length === 1 && files[0].path === originalNote.path &&
          files[0].text === originalNote.text, "Identical retry changed the note or created a file");
      });

    let reviewNote;
    await checkpoint("Changed page note creates one review draft",
      `Replace Your note with: ${changedBody}; click Create page note. The panel should say Page note needs review.`,
      async () => {
        const files = await markdownFiles(path.join(notes, "pages"));
        const review = files.filter((file) => file.path.includes(".rauser-review-"));
        const base = files.find((file) => file.path === originalNote.path);
        requireCondition(files.length === 2 && review.length === 1, "Expected one original note and one sibling review draft");
        requireCondition(base?.text === originalNote.text, "Original page note was changed");
        requireCondition(review[0].text.includes(changedBody) &&
          review[0].text.includes("review_of:") && review[0].text.includes("proposal_id:"),
        "Review draft does not contain the proposal and ownership metadata");
        reviewNote = review[0];
      });

    await checkpoint("Review draft retry is idempotent",
      "Click Create page note again with the changed body. The panel should still show the same review draft path.",
      async () => {
        const files = await markdownFiles(path.join(notes, "pages"));
        requireCondition(files.length === 2 &&
          files.some((file) => file.path === originalNote.path && file.text === originalNote.text) &&
          files.some((file) => file.path === reviewNote.path && file.text === reviewNote.text),
        "Review retry changed an existing file or created another draft");
      });

    await checkpoint("Site removal",
      `Click Remove beside ${origin}/allowed in the panel. It should disappear from Enabled sites.`,
      async () => {
        const reply = await hostConfig(wrapper);
        requireCondition(!reply.config.capture_enabled && reply.config.sites.length === 0,
          "Host still has capture enabled or the site rule");
      });
    console.log("Open Rauser's Details page in chrome://extensions and inspect Site access / permissions.");
    const grantRemoved = await ask(`Type yes after confirming Chrome no longer grants ${origin}: `);
    requireCondition(grantRemoved.toLowerCase() === "yes", "Chrome grant removal was not confirmed");

    await checkpoint("No capture after removal",
      `Close the panel, navigate to ${afterRemoval}, then reopen the panel. Check that it shows 0 pending visits.`,
      async () => {
        const files = await markdownFiles(path.join(notes, "log"));
        requireCondition(!files.some((file) => file.text.includes(afterRemoval)),
          "A visit was logged after site removal");
      });

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
