import assert from "node:assert/strict";
import test from "node:test";

const { finishHostPause, withLatestHostState } = await import("../dist/coordination.js");
const { localTimestamp } = await import("../dist/model.js");
const { storageKey } = await import("../dist/brand.js");
const { isConfig, isResponseShape } = await import("../dist/protocol-shape.js");

// Run after build:extension. The mock copies storage values as Chrome does,
// which catches accidental reliance on mutating an object returned by get().
const values = new Map();
const grants = new Map();
const tabs = new Map();
let permissionCheckError = false;
let permissionChecks = 0;
let tabReads = 0;
let onMessage;
let onCommitted;
let onHistoryStateUpdated;
let onTabUpdated;
// Every native port the code under test opened, newest last.
const ports = [];

function fakePort() {
  const messageListeners = [];
  const disconnectListeners = [];
  const port = {
    sent: [],
    closed: false,
    postMessage(message) {
      if (port.closed) throw new Error("Attempting to use a disconnected port object");
      port.sent.push(message);
    },
    disconnect() { port.closed = true; },
    onMessage: { addListener(listener) { messageListeners.push(listener); } },
    onDisconnect: { addListener(listener) { disconnectListeners.push(listener); } },
    // Deliver a host frame, as Chrome would.
    reply(message) { for (const listener of messageListeners) listener(message); },
    // The host process exited.
    drop() {
      port.closed = true;
      for (const listener of disconnectListeners) listener();
    },
  };
  ports.push(port);
  return port;
}

globalThis.chrome = {
  runtime: {
    id: "test-extension",
    getURL: (path) => `chrome-extension://test-extension/${path}`,
    onMessage: { addListener(listener) { onMessage = listener; } },
    onInstalled: { addListener() {} },
    connectNative: () => fakePort(),
  },
  commands: { onCommand: { addListener() {} } },
  contextMenus: {
    create() {},
    removeAll(callback) { callback?.(); },
    onClicked: { addListener() {} },
  },
  storage: {
    local: {
      async get(key) {
        const result = {};
        for (const name of Array.isArray(key) ? key : [key]) {
          if (values.has(name)) result[name] = structuredClone(values.get(name));
        }
        return result;
      },
      async set(items) {
        for (const [name, value] of Object.entries(items)) {
          values.set(name, structuredClone(value));
        }
      },
      async setAccessLevel() {},
    },
  },
  permissions: {
    async contains({ origins }) {
      permissionChecks += 1;
      if (permissionCheckError) throw new Error("permissions API unavailable");
      return origins?.every((origin) => grants.get(origin) === true) ?? true;
    },
    onAdded: { addListener() {} },
    onRemoved: { addListener() {} },
  },
  tabs: {
    async get(tabId) {
      tabReads += 1;
      if (!tabs.has(tabId)) throw new Error("No tab");
      return structuredClone(tabs.get(tabId));
    },
    onUpdated: { addListener(listener) { onTabUpdated = listener; } },
  },
  webNavigation: {
    onCommitted: { addListener(listener) { onCommitted = listener; } },
    onHistoryStateUpdated: { addListener(listener) { onHistoryStateUpdated = listener; } },
  },
  sidePanel: { async setPanelBehavior() {} },
};

await import("../dist/worker.js");
const { HostClient, HostError, PROTOCOL_VERSION } = await import("../dist/native.js");
const { ConfigSession } = await import("../dist/settings.js");

function send(message) {
  return new Promise((resolve, reject) => {
    const accepted = onMessage(message, {
      id: chrome.runtime.id,
      url: chrome.runtime.getURL("panel.html"),
    }, (reply) => {
      if (reply.ok) resolve(reply.value);
      else reject(new Error(reply.error));
    });
    if (!accepted) reject(new Error("Worker did not accept the panel message"));
  });
}

// Listener work is serialized with panel messages, so a cheap message that
// performs no permission check waits for earlier navigation work to finish.
async function flush() {
  await send({ kind: "get_pending_ids" });
}

function lease(origin) {
  return {
    revision: "host-revision",
    expires_at: Date.now() + 60_000,
    capture_enabled: true,
    sites: [{ origin, path_prefix: "/" }],
  };
}

function visit(id, url) {
  return {
    event: {
      event_id: id,
      url,
      title: null,
      occurred_at: "2026-09-28T00:00:00Z",
      incognito: false,
    },
    dedupe_key: id,
    attempts: 0,
    retry_after: 0,
  };
}

test("config repair suspends capture without losing visits, while confirmed revocation purges", async () => {
  values.clear();
  grants.clear();
  const firstOrigin = "https://first.example";
  const secondOrigin = "https://second.example";
  grants.set("https://first.example:443/*", true);
  grants.set("https://second.example:443/*", true);
  values.set(storageKey("queue_v1"), {
    version: 1,
    items: [visit("first", `${firstOrigin}/a`), visit("second", `${secondOrigin}/b`)],
    overflow_count: 3,
    rejected_count: 2,
    last_error: null,
  });
  values.set(storageKey("policy_v1"), {
    revision: "host-revision",
    expires_at: Date.now() + 60_000,
    capture_enabled: true,
    sites: [
      { origin: firstOrigin, path_prefix: "/" },
      { origin: secondOrigin, path_prefix: "/" },
    ],
  });

  const suspended = await send({ kind: "suspend_policy" });
  assert.equal(suspended.queued, 2);
  assert.equal(suspended.policy_expires_at, null);
  assert.equal(values.get(storageKey("policy_v1")), null);
  assert.deepEqual(await send({ kind: "get_pending" }), []);

  permissionCheckError = true;
  const uncertain = await send({ kind: "get_status" });
  assert.equal(uncertain.queued, 2);
  assert.equal(values.get(storageKey("queue_v1")).items.length, 2);

  permissionCheckError = false;
  grants.set("https://first.example:443/*", false);
  const revoked = await send({ kind: "get_status" });
  assert.equal(revoked.queued, 1);
  assert.deepEqual(revoked.revoked_origins, [firstOrigin]);
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.event_id, "second");

  const ids = await send({ kind: "get_pending_ids" });
  assert.deepEqual(ids, ["second"]);
  const nextQueue = values.get(storageKey("queue_v1"));
  nextQueue.items.push(visit("later", `${secondOrigin}/later`));
  values.set(storageKey("queue_v1"), nextQueue);
  const discarded = await send({ kind: "discard_pending", event_ids: ids });
  assert.equal(discarded.queued, 1);
  assert.equal(discarded.overflow_count, 3);
  assert.equal(discarded.rejected_count, 2);
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.event_id, "later");
});

test("a newer local pause cannot be resumed by an older enable confirmation", async () => {
  values.clear();
  grants.clear();
  grants.set("https://first.example:443/*", true);
  const lease = {
    revision: "host-revision",
    expires_at: Date.now() + 60_000,
    capture_enabled: true,
    sites: [{ origin: "https://first.example", path_prefix: "/" }],
  };

  const previous = await send({ kind: "get_status" });
  const paused = await send({ kind: "pause_capture" });
  assert.equal(paused.pause_pending, true);
  assert.notEqual(paused.pause_token, previous.pause_token);

  const stale = await send({
    kind: "install_policy",
    lease,
    resume_after_confirmation: true,
    resume_after_pause_token: previous.pause_token,
  });
  assert.equal(stale.pause_pending, true);
  assert.equal(stale.policy_expires_at, null);

  const resumed = await send({
    kind: "install_policy",
    lease,
    resume_after_confirmation: true,
    resume_after_pause_token: paused.pause_token,
  });
  assert.equal(resumed.pause_pending, false);
  assert.equal(resumed.policy_expires_at, lease.expires_at);
});

test("a delayed lease refresh reads the host after a site removal commits", async () => {
  let tail = Promise.resolve();
  const lock = async (action) => {
    const prior = tail;
    let release;
    tail = new Promise((resolve) => { release = resolve; });
    await prior;
    try { return await action(); }
    finally { release(); }
  };
  let host = { revision: "r1", sites: ["https://removed.example"] };
  let enteredRemoval;
  const removalStarted = new Promise((resolve) => { enteredRemoval = resolve; });
  let releaseRemoval;
  const removalGate = new Promise((resolve) => { releaseRemoval = resolve; });
  const removing = lock(async () => {
    host = { revision: "r2", sites: [] };
    enteredRemoval();
    await removalGate;
  });
  await removalStarted;
  const installed = [];
  const refreshing = withLatestHostState(
    lock,
    async () => structuredClone(host),
    async (state) => { installed.push(state); },
  );
  releaseRemoval();
  await Promise.all([removing, refreshing]);
  assert.deepEqual(installed, [{ revision: "r2", sites: [] }]);
});

test("pause re-reads a conflicting host revision before clearing the local block", async () => {
  let host = { revision: "r2", config: { capture_enabled: true }, config_issue: null };
  let localPauses = 0;
  let saves = 0;
  let installedDisabled = 0;
  await finishHostPause(
    async () => structuredClone(host),
    async () => { localPauses += 1; },
    async (snapshot) => {
      saves += 1;
      if (saves === 1) {
        host = { ...host, revision: "r3" };
        throw new Error("conflict");
      }
      assert.equal(snapshot.revision, "r3");
      host = { ...snapshot, config: { capture_enabled: false } };
    },
    async () => { installedDisabled += 1; },
    (error) => error instanceof Error && error.message === "conflict",
  );
  assert.equal(localPauses, 2);
  assert.equal(saves, 2);
  assert.equal(installedDisabled, 0);
  assert.equal(host.config.capture_enabled, false);
});

test("a re-enabled origin clears only its old revocation after a matching lease and live grant", async () => {
  values.clear();
  grants.clear();
  const origin = "https://again.example";
  const pattern = "https://again.example:443/*";
  values.set(storageKey("policy_v1"), {
    revision: "r2",
    expires_at: Date.now() + 60_000,
    capture_enabled: true,
    sites: [{ origin, path_prefix: "/" }],
  });
  values.set(storageKey("revocations_v1"), [origin]);
  grants.set(pattern, true);

  const stale = await send({ kind: "ack_reenabled_origin", origin, revision: "r1" });
  assert.deepEqual(stale.revoked_origins, [origin]);

  grants.set(pattern, false);
  const removedAgain = await send({ kind: "ack_reenabled_origin", origin, revision: "r2" });
  assert.deepEqual(removedAgain.revoked_origins, [origin]);

  grants.set(pattern, true);
  const confirmed = await send({ kind: "ack_reenabled_origin", origin, revision: "r2" });
  assert.deepEqual(confirmed.revoked_origins, []);
});

test("a committed page takes its title only until the panel receives the visit", async () => {
  values.clear();
  grants.clear();
  tabs.clear();
  const origin = "https://title.example";
  grants.set("https://title.example:443/*", true);
  values.set(storageKey("policy_v1"), lease(origin));
  tabs.set(7, { id: 7, url: `${origin}/a`, title: "Previous page", incognito: false });

  onCommitted({
    tabId: 7, frameId: 0, documentId: "doc-1", documentLifecycle: "active",
    url: `${origin}/a`, timeStamp: Date.now(),
  });
  await flush();
  const [queued] = values.get(storageKey("queue_v1")).items;
  assert.equal(queued.event.title, null);
  assert.equal(queued.tab_id, 7);
  assert.equal(queued.dispatched, false);
  assert.ok(queued.retry_after > Date.now(), "an untitled visit waits for its title");
  assert.match(queued.event.occurred_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}$/);

  onTabUpdated(7, { title: "x".repeat(400) }, { id: 7, url: `${origin}/a#part`, incognito: false });
  await flush();
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.title, "x".repeat(300));

  onTabUpdated(7, { title: "Other tab" }, { id: 8, url: `${origin}/b`, incognito: false });
  onTabUpdated(8, { title: "Other tab" }, { id: 8, url: `${origin}/a`, incognito: false });
  await flush();
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.title, "x".repeat(300));

  assert.deepEqual(await send({ kind: "get_pending" }), []);
  const graceElapsed = values.get(storageKey("queue_v1"));
  graceElapsed.items[0].retry_after = 0;
  values.set(storageKey("queue_v1"), graceElapsed);
  const pending = await send({ kind: "get_pending" });
  assert.equal(pending.length, 1);
  assert.equal(values.get(storageKey("queue_v1")).items[0].dispatched, true);

  onTabUpdated(7, { title: "Too late" }, { id: 7, url: `${origin}/a`, incognito: false });
  await flush();
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.title, "x".repeat(300));
});

test("a stored visit without a dispatch flag is never patched", async () => {
  values.clear();
  grants.clear();
  const origin = "https://legacy.example";
  grants.set("https://legacy.example:443/*", true);
  values.set(storageKey("policy_v1"), lease(origin));
  values.set(storageKey("queue_v1"), {
    version: 1,
    items: [{ ...visit("legacy", `${origin}/a`), tab_id: 3 }],
    overflow_count: 0,
    rejected_count: 0,
    last_error: null,
  });
  onTabUpdated(3, { title: "Late title" }, { id: 3, url: `${origin}/a`, incognito: false });
  await flush();
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.title, null);
  const status = await send({ kind: "get_status" });
  assert.equal(status.retry_error, null);
});

test("a navigation outside every enabled site skips grant checks and tab reads", async () => {
  values.clear();
  grants.clear();
  tabs.clear();
  grants.set("https://enabled.example:443/*", true);
  values.set(storageKey("policy_v1"), lease("https://enabled.example"));
  tabs.set(4, { id: 4, url: "https://other.example/x", title: "Other", incognito: false });
  const checks = permissionChecks;
  const reads = tabReads;
  onCommitted({
    tabId: 4, frameId: 0, documentId: "doc-2", documentLifecycle: "active",
    url: "https://other.example/x", timeStamp: Date.now(),
  });
  await flush();
  assert.equal(permissionChecks, checks);
  assert.equal(tabReads, reads);
  assert.equal(values.get(storageKey("queue_v1")), undefined);

  tabs.set(4, { id: 4, url: "https://enabled.example/y", title: "Old", incognito: false });
  onCommitted({
    tabId: 4, frameId: 0, documentId: "doc-3", documentLifecycle: "active",
    url: "https://enabled.example/y", timeStamp: Date.now(),
  });
  await flush();
  assert.ok(permissionChecks > checks);
  assert.equal(values.get(storageKey("queue_v1")).items.length, 1);
});

test("a successful ack clears retry state but keeps notices until dismissed", async () => {
  values.clear();
  grants.clear();
  const origin = "https://ack.example";
  grants.set("https://ack.example:443/*", true);
  values.set(storageKey("policy_v1"), lease(origin));
  values.set(storageKey("queue_v1"), {
    version: 1,
    items: [visit("one", `${origin}/1`), visit("two", `${origin}/2`)],
    overflow_count: 2,
    rejected_count: 1,
    last_error: "Visit queue is full; newer visits were not buffered",
  });

  const retrying = await send({
    kind: "ack_visit", event_id: "one", outcome: "retryable", reason: "Host busy",
  });
  assert.equal(retrying.retry_error, "Host busy");
  assert.equal(retrying.last_error, "Visit queue is full; newer visits were not buffered");

  const partial = await send({ kind: "ack_visit", event_id: "two", outcome: "persisted", reason: null });
  assert.equal(partial.retry_error, "Host busy");

  const saved = await send({ kind: "ack_visit", event_id: "one", outcome: "persisted", reason: null });
  assert.equal(saved.retry_error, null);
  assert.equal(saved.last_error, "Visit queue is full; newer visits were not buffered");
  assert.equal(saved.overflow_count, 2);
  assert.equal(saved.rejected_count, 1);

  const cleared = await send({ kind: "clear_notices" });
  assert.equal(cleared.last_error, null);
  assert.equal(cleared.overflow_count, 0);
  assert.equal(cleared.rejected_count, 0);
  assert.equal(values.get(storageKey("queue_v1")).last_error, null);
});

test("visit timestamps use local time with a numeric UTC offset", () => {
  const previous = process.env.TZ;
  const instant = new Date("2026-09-28T03:04:05.678Z");
  try {
    process.env.TZ = "UTC";
    assert.equal(localTimestamp(instant), "2026-09-28T03:04:05+00:00");
    process.env.TZ = "America/New_York";
    assert.equal(localTimestamp(instant), "2026-09-27T23:04:05-04:00");
    process.env.TZ = "Asia/Kolkata";
    assert.equal(localTimestamp(instant), "2026-09-28T08:34:05+05:30");
    assert.equal(localTimestamp(new Date("0999-01-02T00:00:00Z")).length, 25);
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
});

test("worker accepts messages only from the side panel and settings page", async () => {
  for (const page of ["panel.html", "options.html"]) {
    const reply = await new Promise((resolve) => {
      const accepted = onMessage({ kind: "get_status" }, {
        id: chrome.runtime.id, url: chrome.runtime.getURL(page),
      }, resolve);
      assert.equal(accepted, true, page);
    });
    assert.equal(reply.ok, true, page);
  }
  for (const sender of [
    { id: chrome.runtime.id, url: chrome.runtime.getURL("other.html") },
    { id: chrome.runtime.id, url: "https://example.com/options.html" },
    { id: "another-extension", url: "chrome-extension://another-extension/options.html" },
    { id: chrome.runtime.id },
  ]) {
    let responded = false;
    const accepted = onMessage({ kind: "get_status" }, sender, () => { responded = true; });
    assert.equal(accepted, false, JSON.stringify(sender));
    assert.equal(responded, false, JSON.stringify(sender));
  }
});

test("an SPA route change waits for its new title before the panel can send it", async () => {
  values.clear();
  grants.clear();
  tabs.clear();
  const origin = "https://spa.example";
  grants.set("https://spa.example:443/*", true);
  values.set(storageKey("policy_v1"), lease(origin));
  // pushState has run, but the app has not set the new route's title yet.
  tabs.set(9, { id: 9, url: `${origin}/b`, title: "Route A", incognito: false });

  onHistoryStateUpdated({
    tabId: 9, frameId: 0, documentId: "doc-spa", documentLifecycle: "active",
    url: `${origin}/b`, timeStamp: Date.now(),
  });
  await flush();
  const [queued] = values.get(storageKey("queue_v1")).items;
  assert.equal(queued.event.title, "Route A");
  assert.ok(queued.retry_after > Date.now(), "a history update waits for its title");
  assert.deepEqual(await send({ kind: "get_pending" }), []);

  onTabUpdated(9, { title: "Route B" }, { id: 9, url: `${origin}/b`, incognito: false });
  await flush();
  assert.equal(values.get(storageKey("queue_v1")).items[0].event.title, "Route B");
});

test("a retry notice clears once nothing is left to retry, and on dismissal", async () => {
  values.clear();
  grants.clear();
  const origin = "https://retry.example";
  grants.set("https://retry.example:443/*", true);
  const setUp = (ids) => {
    values.set(storageKey("policy_v1"), lease(origin));
    values.set(storageKey("queue_v1"), {
      version: 1,
      items: ids.map((id) => visit(id, `${origin}/${id}`)),
      overflow_count: 0,
      rejected_count: 0,
      last_error: null,
    });
  };
  const retry = (id) => send({ kind: "ack_visit", event_id: id, outcome: "retryable", reason: "Host busy" });

  setUp(["one"]);
  assert.equal((await retry("one")).retry_error, "Host busy");
  const rejected = await send({ kind: "ack_visit", event_id: "one", outcome: "rejected", reason: "No" });
  assert.equal(rejected.queued, 0);
  assert.equal(rejected.retry_error, null);

  setUp(["two"]);
  await retry("two");
  assert.equal((await send({ kind: "discard_pending", event_ids: ["two"] })).retry_error, null);

  setUp(["three"]);
  await retry("three");
  assert.equal((await send({ kind: "remove_site", site: { origin, path_prefix: "/" } })).retry_error, null);

  values.delete(storageKey("locally_removed_sites_v1"));
  setUp(["four"]);
  await retry("four");
  assert.equal((await send({ kind: "pause_capture" })).retry_error, null);
  values.delete(storageKey("pause_pending_v1"));

  setUp(["five"]);
  await retry("five");
  const dismissed = await send({ kind: "clear_notices" });
  assert.equal(dismissed.queued, 1);
  assert.equal(dismissed.retry_error, null);
});

function helloResult(requestId) {
  return {
    type: "hello_result", protocol_version: PROTOCOL_VERSION, request_id: requestId,
    host_version: "0.0.0", configured: true, config_issue: null,
  };
}

function hello() {
  return {
    type: "hello", protocol_version: PROTOCOL_VERSION, request_id: crypto.randomUUID(),
  };
}

test("a host client reconnects lazily after the host exits", async () => {
  ports.length = 0;
  const client = new HostClient();
  let changes = 0;
  client.onStateChange(() => { changes += 1; });
  const first = ports[0];
  const answered = client.call(hello(), "hello_result");
  first.reply(helloResult(first.sent[0].request_id));
  await answered;
  assert.equal(client.connected, true);

  const inFlight = client.call(hello(), "hello_result");
  first.drop();
  await assert.rejects(inFlight, (error) => error instanceof HostError && error.code === "disconnected");
  // The next call starts a fresh host, so the page is still usable.
  assert.equal(client.connected, true);

  const retried = client.call(hello(), "hello_result");
  assert.equal(ports.length, 2);
  const second = ports[1];
  // A late frame on the dead port must not settle the new request.
  first.reply({ ...helloResult(second.sent[0].request_id), host_version: "stale" });
  second.reply(helloResult(second.sent[0].request_id));
  assert.equal((await retried).host_version, "0.0.0");
  assert.equal(client.connected, true);
  assert.ok(changes >= 1);

  client.disconnect();
  assert.equal(client.connected, false);
  await assert.rejects(client.call(hello(), "hello_result"),
    (error) => error instanceof HostError && error.code === "disconnected");
  assert.equal(ports.length, 2, "an explicitly closed client stays closed");
});

test("a host that cannot be started reports the session disconnected", async () => {
  ports.length = 0;
  let changes = 0;
  const session = new ConfigSession(() => { changes += 1; });
  session.config = {
    storage: null, capture_enabled: false, sites: [], strip_params: [], near_repeat_secs: 0,
  };
  session.revision = "r1";
  assert.equal(session.connected, true);

  // The host was removed, so Chrome drops the port before any reply.
  const pending = session.hello();
  ports[0].drop();
  await assert.rejects(pending, (error) => error instanceof HostError && error.code === "disconnected");
  assert.equal(session.connected, false);
  assert.ok(changes >= 1, "the page is told to redraw its controls");

  // Once the host is back, the next call reconnects and the session recovers.
  const recovered = session.hello();
  const port = ports.at(-1);
  port.reply(helloResult(port.sent[0].request_id));
  await recovered;
  assert.equal(session.connected, true);
  session.host.disconnect();
});

test("unaddressed host errors and version mismatches reach the caller", async () => {
  ports.length = 0;
  const client = new HostClient();
  const port = ports[0];

  const skewed = client.call(hello(), "hello_result");
  port.reply({
    type: "error", protocol_version: PROTOCOL_VERSION + 1, request_id: port.sent[0].request_id,
    code: "unsupported_protocol_version", message: "extension and host protocol versions differ",
  });
  await assert.rejects(skewed,
    (error) => error instanceof HostError && error.code === "unsupported_protocol_version");

  const skewedResult = client.call(hello(), "hello_result");
  port.reply({ ...helloResult(port.sent[1].request_id), protocol_version: PROTOCOL_VERSION + 1 });
  await assert.rejects(skewedResult,
    (error) => error instanceof HostError && error.code === "invalid_response");

  // The host cannot read an oversized frame's request_id, so it answers with
  // an empty one and closes the connection.
  const oversized = client.call(hello(), "hello_result");
  port.reply({
    type: "error", protocol_version: PROTOCOL_VERSION, request_id: "",
    code: "message_too_large", message: "native message exceeds the 4 MiB inbound limit",
  });
  port.drop();
  await assert.rejects(oversized,
    (error) => error instanceof HostError && error.code === "message_too_large");
  client.disconnect();
});

test("host config shape requires every v4 privacy and agent key", () => {
  const config = {
    storage: {
      root: "/notes", profile: "neutral", log_dir: "log", pages_dir: "pages", later_dir: "later",
      summaries_dir: null,
    },
    capture_enabled: false, sites: [], strip_params: [], near_repeat_secs: 300,
    agent_denylist: ["bank.example"], agent_denylist_confirmed: false, log_incognito: false,
    agent: null,
  };
  assert.equal(isConfig(config), true);
  const agent = {
    harness_id: "claude-code", adapter: "claude_code", binary: "/usr/local/bin/claude",
    args: ["-p", "{prompt}"], env_allow: [], timeout_secs: 120,
  };
  assert.equal(isConfig({ ...config, agent }), true);
  for (const key of ["agent_denylist", "agent_denylist_confirmed", "log_incognito", "agent"]) {
    const without = { ...config };
    delete without[key];
    assert.equal(isConfig(without), false, key);
  }
  assert.equal(isConfig({ ...config, log_incognito: true }), false);
  assert.equal(isConfig({ ...config, agent: { ...agent, adapter: "claude-code" } }), false);
  // The host always sends summaries_dir, as null until a folder is chosen.
  const { summaries_dir: _omitted, ...m1Storage } = config.storage;
  assert.equal(isConfig({ ...config, storage: m1Storage }), false);
  assert.equal(isConfig({ ...config, storage: { ...m1Storage, summaries_dir: 7 } }), false);

  const result = { type: "config_result", revision: "missing", config, config_issue: null };
  assert.equal(isResponseShape(result), false);
  const agentStatus = { state: "not_set_up", harness_version: null, message: "Set up" };
  assert.equal(isResponseShape({ ...result, agent_status: agentStatus }), true);
  assert.equal(isResponseShape({ ...result, agent_status: { ...agentStatus, state: "on" } }), false);
  assert.equal(isResponseShape({
    type: "agent_checked", harness_id: "claude-code", harness_version: "2.1.0", url_allowed: null,
  }), true);

  // Harness setup: every offer key is present, nullable ones as null.
  const offer = {
    offer_id: "offer-1", adapter: "claude_code", harness_id: "claude-code",
    binary: "/usr/local/bin/claude", real_path: "/opt/claude/bin/claude", version: "2.1.284",
    args: ["-p", "{prompt}"], env_required: ["HOME", "PATH"],
    env_optional: [{ name: "ANTHROPIC_API_KEY", present: true }], refusal: null,
  };
  const refused = {
    ...offer, offer_id: null, adapter: "codex", harness_id: "codex", real_path: null,
    version: null, env_optional: [], refusal: "Codex setup is not available yet",
  };
  const discovered = { type: "harnesses_discovered", offers: [offer, refused] };
  assert.equal(isResponseShape(discovered), true);
  assert.equal(isResponseShape({ ...discovered, offers: [] }), true);
  assert.equal(isResponseShape({ ...discovered, offers: null }), false);
  for (const key of Object.keys(offer)) {
    const without = { ...offer };
    delete without[key];
    assert.equal(isResponseShape({ ...discovered, offers: [without] }), false, key);
  }
  assert.equal(isResponseShape({ ...discovered, offers: [{ ...offer, adapter: "generic" }] }), false);
  assert.equal(isResponseShape({
    ...discovered, offers: [{ ...offer, env_optional: ["ANTHROPIC_API_KEY"] }],
  }), false);
  assert.equal(isResponseShape({
    ...discovered, offers: [{ ...offer, env_optional: [{ name: "X", present: "yes" }] }],
  }), false);

  const setUp = { type: "harness_setup_confirmed", harness_token: "t", config, summary: "Harness" };
  assert.equal(isResponseShape(setUp), true);
  for (const key of ["harness_token", "config", "summary"]) {
    const without = { ...setUp };
    delete without[key];
    assert.equal(isResponseShape(without), false, key);
  }
});
