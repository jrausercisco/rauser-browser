import assert from "node:assert/strict";
import test from "node:test";

const { finishHostPause, withLatestHostState } = await import("../dist/coordination.js");

// Run after build:extension. The mock copies storage values as Chrome does,
// which catches accidental reliance on mutating an object returned by get().
const values = new Map();
const grants = new Map();
let permissionCheckError = false;
let onMessage;

globalThis.chrome = {
  runtime: {
    id: "test-extension",
    getURL: (path) => `chrome-extension://test-extension/${path}`,
    onMessage: { addListener(listener) { onMessage = listener; } },
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
      if (permissionCheckError) throw new Error("permissions API unavailable");
      return origins?.every((origin) => grants.get(origin) === true) ?? true;
    },
    onAdded: { addListener() {} },
    onRemoved: { addListener() {} },
  },
  sidePanel: { async setPanelBehavior() {} },
};

await import("../dist/worker.js");

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
  values.set("rauser_queue_v1", {
    version: 1,
    items: [visit("first", `${firstOrigin}/a`), visit("second", `${secondOrigin}/b`)],
    overflow_count: 3,
    rejected_count: 2,
    last_error: null,
  });
  values.set("rauser_policy_v1", {
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
  assert.equal(values.get("rauser_policy_v1"), null);
  assert.deepEqual(await send({ kind: "get_pending" }), []);

  permissionCheckError = true;
  const uncertain = await send({ kind: "get_status" });
  assert.equal(uncertain.queued, 2);
  assert.equal(values.get("rauser_queue_v1").items.length, 2);

  permissionCheckError = false;
  grants.set("https://first.example:443/*", false);
  const revoked = await send({ kind: "get_status" });
  assert.equal(revoked.queued, 1);
  assert.deepEqual(revoked.revoked_origins, [firstOrigin]);
  assert.equal(values.get("rauser_queue_v1").items[0].event.event_id, "second");

  const ids = await send({ kind: "get_pending_ids" });
  assert.deepEqual(ids, ["second"]);
  const nextQueue = values.get("rauser_queue_v1");
  nextQueue.items.push(visit("later", `${secondOrigin}/later`));
  values.set("rauser_queue_v1", nextQueue);
  const discarded = await send({ kind: "discard_pending", event_ids: ids });
  assert.equal(discarded.queued, 1);
  assert.equal(discarded.overflow_count, 3);
  assert.equal(discarded.rejected_count, 2);
  assert.equal(values.get("rauser_queue_v1").items[0].event.event_id, "later");
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
  values.set("rauser_policy_v1", {
    revision: "r2",
    expires_at: Date.now() + 60_000,
    capture_enabled: true,
    sites: [{ origin, path_prefix: "/" }],
  });
  values.set("rauser_revocations_v1", [origin]);
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
