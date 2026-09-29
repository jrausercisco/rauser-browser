import assert from "node:assert/strict";
import test from "node:test";

const { storageKey } = await import("../dist/brand.js");
const { PROTOCOL_VERSION } = await import("../dist/native.js");

// Run after build:extension. Each test loads a fresh copy of the settings page
// against a fake DOM, a scripted native host, a stub worker, and a Chrome
// permission store that fires onAdded/onRemoved as Chrome does.
const NOTE_ORIGINS_KEY = storageKey("note_origins_v1");
const SITE = { origin: "https://a.com", path_prefix: "/" };
const PATTERN = "https://a.com:443/*";
const AGENT_STATUS = { state: "not_set_up", harness_version: null, message: "Set up an AI harness." };

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.textContent = "";
    this.value = "";
    this.disabled = false;
    this.children = [];
    this.dataset = {};
    this.listeners = new Map();
    this.classList = { toggle() {}, add() {}, remove() {} };
  }
  addEventListener(type, listener) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }
  dispatch(type) {
    for (const listener of this.listeners.get(type) ?? []) listener({ type });
  }
  click() {
    if (!this.disabled) this.dispatch("click");
  }
  setAttribute() {}
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  querySelectorAll(selector) {
    const tag = selector.toUpperCase();
    const found = [];
    const visit = (node) => {
      for (const child of node.children ?? []) {
        if (child.tagName === tag) found.push(child);
        visit(child);
      }
    };
    visit(this);
    return found;
  }
}

let env;
let loads = 0;

function statusFor(overrides = {}) {
  return {
    queued: 0, next_retry_at: null, overflow_count: 0, rejected_count: 0, last_error: null,
    retry_error: null, policy_expires_at: null, revoked_origins: [], pause_pending: false,
    pause_token: null, navigation_ready: true, locally_removed_sites: [], ...overrides,
  };
}

function configWith(sites) {
  return {
    storage: {
      root: "/vault", profile: "neutral", log_dir: "log", pages_dir: "pages", later_dir: "later",
      summaries_dir: null,
    },
    capture_enabled: sites.length > 0,
    sites,
    strip_params: [],
    near_repeat_secs: 60,
    agent_denylist: [],
    agent_denylist_confirmed: false,
    log_incognito: false,
    agent: null,
  };
}

function fire(listeners, permissions) {
  setTimeout(() => { for (const listener of listeners) listener(permissions); }, 0);
}

function install({ sites = [SITE], grants = [], storage = {}, host = {}, workerStatus = {} } = {}) {
  const elements = new Map();
  const state = {
    grants: new Set(grants),
    storage: new Map(Object.entries(storage)),
    config: configWith(sites),
    revision: 1,
    configIssue: null,
    added: [],
    removed: [],
    elements,
  };
  for (const [id, tag] of [
    ["status", "div"], ["folder-path", "output"], ["choose-folder", "button"], ["site-url", "input"],
    ["site-path", "input"], ["enable-site", "button"], ["pause-capture", "button"], ["sites-list", "ul"],
    ["agent-state", "p"], ["detect-harnesses", "button"], ["harness-offers", "ul"], ["harness-env", "ul"],
    ["summaries-dir", "input"], ["denylist-input", "input"], ["denylist-preview", "output"],
    ["denylist-add", "button"], ["denylist-suggestions", "div"], ["denylist-list", "ul"],
    ["setup-harness", "button"], ["remove-harness", "button"],
  ]) {
    elements.set(id, new FakeElement(tag));
  }
  elements.get("site-path").value = "/";
  elements.get("status").textContent = "Connecting to native host…";

  function hostReply(request) {
    const custom = host[request.type]?.(request, state);
    if (custom) return custom;
    switch (request.type) {
      case "hello":
        return { type: "hello_result", host_version: "0.0.0", configured: true, config_issue: null };
      case "get_config":
        return {
          type: "config_result", revision: String(state.revision), config: state.config,
          config_issue: state.configIssue, agent_status: AGENT_STATUS,
        };
      case "choose_folder":
        return { type: "folder_chosen", path: "/vault", picker_token: `picker-${state.connects}` };
      case "confirm_config":
        return { type: "config_confirmed", consent_token: "consent", summary: "summary" };
      case "update_config":
        state.config = request.config;
        state.configIssue = null;
        state.revision += 1;
        return { type: "config_updated", revision: String(state.revision), config: state.config };
      default:
        throw new Error(`Unexpected host request ${request.type}`);
    }
  }

  const has = ({ permissions = [], origins = [] }) =>
    [...permissions, ...origins].every((entry) => state.grants.has(entry));

  env = state;
  globalThis.document = {
    getElementById: (id) => elements.get(id) ?? null,
    createElement: (tag) => new FakeElement(tag),
  };
  globalThis.window = { addEventListener() {} };
  globalThis.chrome = {
    runtime: {
      lastError: undefined,
      reload() {},
      connectNative() {
        const messageListeners = [];
        const disconnectListeners = [];
        // The test can end this host process as Chrome would report it.
        state.exitHost = () => { for (const listener of disconnectListeners) listener(); };
        state.connects = (state.connects ?? 0) + 1;
        return {
          onMessage: { addListener(listener) { messageListeners.push(listener); } },
          onDisconnect: { addListener(listener) { disconnectListeners.push(listener); } },
          postMessage(request) {
            setTimeout(() => {
              const reply = hostReply(request);
              for (const listener of messageListeners) {
                listener({ ...reply, protocol_version: PROTOCOL_VERSION, request_id: request.request_id });
              }
            }, 0);
          },
          disconnect() {},
        };
      },
      async sendMessage(request) {
        if (request.kind === "remove_site") {
          return { ok: true, value: statusFor({ ...workerStatus, locally_removed_sites: [request.site] }) };
        }
        return { ok: true, value: statusFor(workerStatus) };
      },
    },
    storage: {
      local: {
        async get(key) {
          const result = {};
          for (const name of Array.isArray(key) ? key : [key]) {
            if (state.storage.has(name)) result[name] = structuredClone(state.storage.get(name));
          }
          return result;
        },
        async set(items) {
          for (const [name, value] of Object.entries(items)) state.storage.set(name, structuredClone(value));
        },
        async remove(keys) {
          for (const name of Array.isArray(keys) ? keys : [keys]) state.storage.delete(name);
        },
      },
      onChanged: { addListener() {} },
    },
    permissions: {
      async contains(permissions) { return has(permissions); },
      async request({ permissions = [], origins = [] }) {
        const entries = [...permissions, ...origins].filter((entry) => !state.grants.has(entry));
        for (const entry of entries) state.grants.add(entry);
        if (entries.length) fire(state.added, { permissions, origins });
        return true;
      },
      async remove({ permissions = [], origins = [] }) {
        for (const entry of [...permissions, ...origins]) state.grants.delete(entry);
        fire(state.removed, { permissions, origins });
        return true;
      },
      onAdded: { addListener(listener) { state.added.push(listener); } },
      onRemoved: { addListener(listener) { state.removed.push(listener); } },
    },
  };
  return state;
}

async function until(condition, what) {
  const deadline = Date.now() + 2_000;
  while (!condition()) {
    if (Date.now() > deadline) {
      throw new Error(`Timed out waiting for ${what}; status: ${env.elements.get("status").textContent}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

const statusText = () => env.elements.get("status").textContent;
const idle = () => !env.elements.get("choose-folder").disabled;

async function openSettings({ connected = true } = {}) {
  loads += 1;
  await import(`../dist/options.js?load=${loads}`);
  await until(() => (idle() || !connected) && !statusText().startsWith("Connecting"),
    "the settings page to load");
}

async function typeSite(url) {
  env.elements.get("site-url").value = url;
  env.elements.get("site-url").dispatch("input");
  await until(() => !env.elements.get("enable-site").disabled, "Enable to become available");
}

async function removeListedSite() {
  const [remove] = env.elements.get("sites-list").querySelectorAll("button");
  remove.click();
  await until(() => statusText().startsWith("Removed ") && idle(), "the removal to finish");
}

test("a canceled re-enable after removing a site rolls back Chrome grants", async () => {
  const state = install({
    grants: ["webNavigation", PATTERN],
    host: {
      confirm_config: (_request, current) => current.cancelNext
        ? { type: "error", code: "cancelled", message: "The user canceled" }
        : null,
    },
  });
  await openSettings();
  await typeSite("https://a.com");

  await removeListedSite();
  assert.equal(state.grants.has(PATTERN), false);
  assert.equal(state.grants.has("webNavigation"), false);

  // The preflight taken before removal said both grants were already held.
  await until(() => !env.elements.get("enable-site").disabled, "Enable after removal");
  state.cancelNext = true;
  env.elements.get("enable-site").click();
  await until(() => statusText().startsWith("Setup failed") && idle(), "the canceled setup");
  await until(() => !state.grants.has(PATTERN) && !state.grants.has("webNavigation"),
    "the new Chrome grants to be rolled back");
});

test("removing a logging site keeps an origin grant the side panel made for notes", async () => {
  const state = install({
    grants: ["webNavigation", PATTERN],
    storage: { [NOTE_ORIGINS_KEY]: [SITE.origin] },
  });
  await openSettings();
  await removeListedSite();
  assert.equal(state.grants.has(PATTERN), true);
  assert.equal(state.grants.has("webNavigation"), false);
  assert.deepEqual(state.storage.get(NOTE_ORIGINS_KEY), [SITE.origin]);
  assert.match(statusText(), /kept for its page notes/);
});

test("a notes grant Chrome no longer holds does not protect a later logging grant", async () => {
  const state = install({ sites: [], storage: { [NOTE_ORIGINS_KEY]: [SITE.origin] } });
  await openSettings();
  await typeSite("https://a.com");
  env.elements.get("enable-site").click();
  await until(() => statusText().startsWith("Capture enabled") && idle(), "setup to finish");
  await removeListedSite();
  assert.equal(state.grants.has(PATTERN), false);
  assert.deepEqual(state.storage.get(NOTE_ORIGINS_KEY) ?? [], []);
});

test("a settings load failure after hello is not reported as a missing host", async () => {
  install({
    workerStatus: { revoked_origins: ["https://b.com"] },
    host: {
      get_config: (_request, current) => ({
        type: "config_result", revision: "1", config: current.config, config_issue: "config.json is unreadable",
        agent_status: AGENT_STATUS,
      }),
    },
  });
  await openSettings();
  assert.doesNotMatch(statusText(), /Native host unavailable/);
  assert.match(statusText(), /config\.json is unreadable/);
});

test("a failed hello reports the host as unavailable", async () => {
  install({
    host: { hello: () => ({ type: "error", code: "internal", message: "host broke" }) },
  });
  await openSettings({ connected: false });
  assert.match(statusText(), /^Native host unavailable: host broke\./);
});

test("the panel's recorded notes origins survive until Chrome drops the grant", async () => {
  const state = install({ grants: [PATTERN] });
  const { noteOrigins, pruneNoteOrigins, recordNoteOrigin } = await import("../dist/grants.js");
  await recordNoteOrigin(SITE.origin);
  await recordNoteOrigin(SITE.origin);
  await recordNoteOrigin("https://b.com");
  assert.deepEqual(state.storage.get(NOTE_ORIGINS_KEY), [SITE.origin, "https://b.com"]);
  await pruneNoteOrigins();
  assert.deepEqual([...await noteOrigins()], [SITE.origin]);
});

const FOLDER_ISSUE = "The selected notes folder is unavailable or changed. Choose it again.";

test("a kept config with a folder issue needs a new folder pick, which repairs it", async () => {
  const state = install({ grants: ["webNavigation", PATTERN] });
  state.configIssue = FOLDER_ISSUE;
  await openSettings();
  assert.match(statusText(), /needs repair/);
  const [remove] = env.elements.get("sites-list").querySelectorAll("button");
  assert.equal(remove.disabled, true);
  assert.equal(env.elements.get("pause-capture").disabled, true);
  env.elements.get("site-url").value = "https://b.com";
  env.elements.get("site-url").dispatch("input");
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(env.elements.get("enable-site").disabled, true);

  env.elements.get("choose-folder").click();
  await until(() => state.config.storage?.root === "/vault" && idle(), "the repair to be saved");
  assert.match(statusText(), /^Capture is enabled for the listed sites/);
  assert.deepEqual(state.config.sites, [SITE]);
  await until(() => !env.elements.get("enable-site").disabled, "Enable after the repair");
});

test("choosing a folder saves it at once, leaving sites and capture as they were", async () => {
  const saves = [];
  const state = install({
    sites: [],
    host: { update_config: (request) => { saves.push(request); return null; } },
  });
  state.config = { ...state.config, storage: null };
  await openSettings();
  env.elements.get("choose-folder").click();
  await until(() => state.config.storage?.root === "/vault" && idle(), "the folder to be saved");
  assert.equal(saves.length, 1);
  assert.equal(saves[0].picker_token, "picker-1");
  assert.equal(saves[0].consent_token, "consent");
  assert.equal(state.config.capture_enabled, false);
  assert.deepEqual(state.config.sites, []);
  assert.equal(env.elements.get("folder-path").textContent, "/vault");
  assert.match(statusText(), /^No sites are enabled for capture\. Enable a site to begin\./);
});

test("a canceled repair still rolls back the Chrome grants it added", async () => {
  const state = install({
    grants: ["webNavigation", PATTERN],
    host: {
      confirm_config: () => ({ type: "error", code: "cancelled", message: "The user canceled" }),
    },
  });
  state.configIssue = FOLDER_ISSUE;
  await openSettings();
  env.elements.get("choose-folder").click();
  // The declined repair keeps the selection for the site's own confirmation.
  await until(() => statusText().startsWith("Canceled. /vault was not saved") && idle(), "the folder pick");
  await typeSite("https://b.com");
  env.elements.get("enable-site").click();
  await until(() => statusText().startsWith("Setup failed") && idle(), "the canceled setup");
  await until(() => !state.grants.has("https://b.com:443/*"), "the new origin grant to be rolled back");
  assert.equal(state.grants.has("webNavigation"), true);
  assert.equal(state.grants.has(PATTERN), true);
});

test("a host restart drops a folder selection only the old host knew", async () => {
  const state = install({
    sites: [],
    host: {
      // The first folder save is declined, so the selection stays on the page.
      confirm_config: (_request, current) => current.connects === 1
        ? { type: "error", code: "cancelled", message: "The user canceled" }
        : null,
      update_config: (request, current) =>
        request.picker_token !== null && request.picker_token !== `picker-${current.connects}`
        ? { type: "error", code: "unauthorized", message: "folder selection token is unknown" }
        : null,
    },
  });
  state.config = { ...state.config, storage: null };
  await openSettings();
  env.elements.get("choose-folder").click();
  await until(() => statusText().startsWith("Canceled. /vault was not saved") && idle(), "the folder pick");
  await typeSite("https://a.com");

  state.exitHost();
  assert.match(statusText(), /Choose the folder again/);
  assert.equal(env.elements.get("folder-path").textContent, "None selected");
  assert.equal(env.elements.get("enable-site").disabled, true);

  env.elements.get("choose-folder").click();
  await until(() => state.config.storage?.root === "/vault" && idle(), "the second folder pick to be saved");
  await until(() => !env.elements.get("enable-site").disabled, "Enable after the second pick");
  env.elements.get("enable-site").click();
  await until(() => statusText().startsWith("Capture enabled") && idle(), "setup to finish");
});

test("a host restart drops harness offers only the old host knew", async () => {
  const offer = {
    offer_id: "offer-1", adapter: "claude_code", harness_id: "claude-code",
    binary: "/usr/local/bin/claude", real_path: "/opt/claude/bin/claude", version: "2.1.284",
    args: ["-p", "{prompt}"], env_required: ["HOME", "PATH"], env_optional: [], refusal: null,
  };
  const state = install({
    host: {
      discover_harnesses: (_request, current) =>
        ({ type: "harnesses_discovered", offers: [{ ...offer, offer_id: `offer-${current.connects}` }] }),
    },
  });
  await openSettings();
  env.elements.get("detect-harnesses").click();
  await until(() => !env.elements.get("setup-harness").disabled, "Set up to become available");

  state.exitHost();
  assert.match(statusText(), /detected harnesses were lost\. Detect harnesses again/);
  assert.equal(env.elements.get("setup-harness").disabled, true);

  env.elements.get("detect-harnesses").click();
  await until(() => !env.elements.get("setup-harness").disabled, "Set up after detecting again");
});
