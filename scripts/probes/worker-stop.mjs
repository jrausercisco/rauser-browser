// Group 3 of DESIGN.md §12.2 step 6.1: does worker state kept in
// chrome.storage survive a forced service-worker stop and a browser restart?
// §5.1 keeps the tab-to-artifact map in storage.session (L187) and the pending
// hourly activity total in trusted-only storage.local "so it survives a browser
// restart" (L175), and M1 "does not keep dwell or scroll timers only in
// service-worker memory" (L228). The probe writes both kinds of state from the
// worker, plus an in-memory global, then stops the worker with CDP. It proves
// the stop by the worker target disappearing and the restart by a new bootId
// (a random UUID taken at the worker's top level) and a new target ID. A
// navigation made while the worker is stopped must wake it and reach the
// top-level listener. Chrome is then closed and relaunched on the same profile.
//
// Headless limits: a forced stop stands in for Chrome's own idle termination,
// although the probe also waits to see whether an idle worker stops by itself.
// After the relaunch the recorder has to be loaded again (Extensions.loadUnpacked
// lasts one browser session), and Chrome clears storage.session on an
// extension reload as well as on a restart, so the restart result cannot tell
// the two apart; that finding is reported as inconclusive. Likewise the local
// result covers a restart plus an unpacked reload, not a persistently
// installed extension, which is not reloaded at startup.

export const name = "worker-stop";
export const title = "storage.session and a pending storage.local total across a worker stop and a browser restart";

const SESSION_KEY = "probeSession";
const PENDING_KEY = "probePending";
const IDLE_WAIT_MS = 60_000;
// Chrome's idle timer is 30 s; allow polling slack either side.
const IDLE_MIN_MS = 25_000;
const IDLE_MAX_MS = 40_000;

const mainCommit = (url) => (entry) => entry.kind === "onCommitted" && entry.details.frameId === 0 && entry.details.url === url;
const mainBefore = (url) => (entry) => entry.kind === "onBeforeNavigate" && entry.details.frameId === 0 && entry.details.url === url;

// A stand-in pending total keyed as §5.1 L175 describes: provisional
// artifact_id, local date, hour, and UTC offset.
function pendingTotal(now) {
  const date = new Date(now);
  const pad = (value) => String(value).padStart(2, "0");
  return {
    artifact_id: "jira:KEY-1",
    local_date: `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`,
    hour: date.getHours(),
    utc_offset_min: -date.getTimezoneOffset(),
    opens: 1,
    focused_secs: 42,
    background_secs: 7,
    modes: ["view"],
    sources: [{ from: "jira:KEY-0", transition: "link" }],
  };
}

// Chrome returns stored objects with their keys sorted, so compare with
// sorted keys.
const canonical = (value) => (Array.isArray(value) ? value.map(canonical)
  : value && typeof value === "object" ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])])) : value);
const same = (a, b) => JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, extra = {}) => findings.push({ claim, expected, observed, matches, ...extra });

  // Give the recorder's own tab map an entry, so the map it keeps in
  // storage.session is part of what must survive.
  const source = ctx.urls.browse("KEY-1");
  await ctx.navigate(ctx.homeTargetId, source, { transitionType: "typed" });
  const homeTabId = await ctx.tabIdFor(source);
  await ctx.waitFor(async () => ctx.requireCondition(
    (await ctx.inWorker("probe.tabMap()"))[homeTabId] === source, "The tab map has no entry for the source tab yet"), { timeout: 6_000 });

  // State as the worker would write it, plus an in-memory global and the
  // storage facts §3.2 relies on.
  const now = Date.now();
  const session = { tab: homeTabId, artifact: "jira:KEY-1", focusedSince: now, mode: "focused" };
  const pending = pendingTotal(now);
  const setup = await ctx.inWorker(`(async () => {
    const access = await probe.attempt(() => chrome.storage.local.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" }));
    const sessionAccess = await probe.attempt(() => chrome.storage.session.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" }));
    await chrome.storage.session.set({ ${SESSION_KEY}: ${JSON.stringify(session)} });
    await chrome.storage.local.set({ ${PENDING_KEY}: ${JSON.stringify(pending)} });
    globalThis.probeMarker = { bootId: probe.bootId, setAt: Date.now() };
    return {
      bootId: probe.bootId,
      memory: probe.memory.length,
      localAccess: access,
      sessionAccess,
      sessionQuota: chrome.storage.session.QUOTA_BYTES ?? null,
      sessionBytes: await chrome.storage.session.getBytesInUse(null),
      localBytes: await chrome.storage.local.getBytesInUse(null),
      marker: globalThis.probeMarker,
      tabMap: await probe.tabMap(),
    };
  })()`);
  const before = await ctx.workerTarget();
  check("setAccessLevel TRUSTED_CONTEXTS is accepted on storage.local (§3.2 L68)",
    "resolves", setup.localAccess, setup.localAccess.ok === true,
    { sessionAccess: setup.sessionAccess, sessionQuota: setup.sessionQuota, sessionBytes: setup.sessionBytes, localBytes: setup.localBytes,
      note: "Only acceptance is checked: no content script tries to read storage.local, so trusted-only enforcement is not shown." });

  // Forced stop. stopWorker returns the stopped worker's bootId.
  const stopped = await ctx.stopWorker();
  const afterStop = await ctx.workerTarget();
  check("a forced stop removes the worker target", "no worker target",
    { method: stopped.method, target: afterStop ? afterStop.targetId : "none" },
    afterStop === null && stopped.bootId === setup.bootId,
    { note: `Stopped with ${stopped.method}.` });

  // While the worker is stopped, an extension page reads the same storage.
  // This shows the values are held by Chrome, not by the worker, and whether
  // such a read starts the worker.
  const whileStopped = await ctx.control(`Promise.all([
    chrome.storage.session.get(${JSON.stringify([SESSION_KEY, "tabmap"])}),
    chrome.storage.local.get(${JSON.stringify(PENDING_KEY)}),
  ]).then(([session, local]) => ({ session, local }))`);
  const stillStopped = (await ctx.workerTarget()) === null;
  check("while the worker is stopped, storage.session and storage.local still hold its values",
    "session and pending total readable from an extension page",
    { session: whileStopped.session[SESSION_KEY] ?? null, pending: whileStopped.local[PENDING_KEY]?.focused_secs ?? null, workerStarted: !stillStopped },
    same(whileStopped.session[SESSION_KEY], session) && same(whileStopped.local[PENDING_KEY], pending),
    { note: stillStopped ? "Reading storage from the control tab did not start the worker." : "Reading storage from the control tab started the worker." });
  // The tab map is checked here, before any new worker runs: after the wake
  // the new worker's onCommitted rewrites the source tab's entry whether or
  // not the old map survived, so only this read shows the old map itself.
  check("the recorder's tab map in storage.session kept the source tab's entry across the stop",
    `${homeTabId} -> ${source}, the whole pre-stop map unchanged, read while the worker is stopped`,
    { entry: whileStopped.session.tabmap?.[homeTabId] ?? null, workerStarted: !stillStopped },
    stillStopped && whileStopped.session.tabmap?.[homeTabId] === source && same(whileStopped.session.tabmap, setup.tabMap),
    { tabMapBefore: setup.tabMap, tabMapWhileStopped: whileStopped.session.tabmap ?? null });

  // A navigation while stopped: Chrome must start the worker for it and
  // deliver the event to the listener registered at top level.
  const wake = ctx.urls.page("wake");
  const wakeSince = Date.now();
  await ctx.navigate(ctx.homeTargetId, wake, { transitionType: "link" });
  const restartedBootId = await ctx.waitForRestart(stopped.bootId).catch(() => null);
  const after = await ctx.workerTarget();
  const woke = await ctx.waitForEvent(mainCommit(wake), { since: wakeSince }).catch(() => null);
  const newBoot = restartedBootId ? await ctx.readLog({ bootId: restartedBootId }) : [];
  const bootRecord = newBoot.find((entry) => entry.kind === "worker.boot");
  const wokeBefore = newBoot.find(mainBefore(wake));
  const wokeCommit = newBoot.find(mainCommit(wake));
  const oldBootWake = (await ctx.readLog({ bootId: stopped.bootId })).filter((entry) => entry.details?.url === wake);
  check("a navigation while the worker is stopped starts a new worker", "new bootId and a new worker target",
    { oldBoot: stopped.bootId.slice(0, 8), newBoot: restartedBootId?.slice(0, 8) ?? null, oldTarget: before?.targetId.slice(0, 8), newTarget: after?.targetId.slice(0, 8) ?? null },
    restartedBootId !== null && restartedBootId !== stopped.bootId && after !== null && after.targetId !== before?.targetId);
  check("the waking navigation's events reach the new worker's top-level listeners",
    "onBeforeNavigate then onCommitted logged under the new bootId, none under the old",
    { onBeforeNavigate: wokeBefore ? ctx.brief(wokeBefore) : null, onCommitted: woke ? ctx.brief(woke) : null, underOldBoot: oldBootWake.length },
    Boolean(woke && wokeBefore && wokeCommit && bootRecord) && woke.bootId === restartedBootId
      && wokeBefore.seq < wokeCommit.seq && oldBootWake.length === 0,
    { events: newBoot.map((entry) => ctx.brief(entry)),
      note: "onBeforeNavigate is the event that started the worker, so its presence under the new bootId shows the waking event itself was delivered." });

  // The new worker's view of the state written by the old one.
  const restarted = await ctx.inWorker(`(async () => ({
    bootId: probe.bootId,
    memory: probe.memory.length,
    firstMemory: probe.memory[0]?.kind ?? null,
    marker: globalThis.probeMarker ?? null,
    session: (await chrome.storage.session.get(${JSON.stringify(SESSION_KEY)}))[${JSON.stringify(SESSION_KEY)}] ?? null,
    pending: (await chrome.storage.local.get(${JSON.stringify(PENDING_KEY)}))[${JSON.stringify(PENDING_KEY)}] ?? null,
    tabMap: await probe.tabMap(),
  }))()`);
  check("an in-memory global does not survive the stop (L228: no timers only in worker memory)",
    "globalThis.probeMarker is gone and memory restarts",
    { marker: restarted.marker, memory: restarted.memory, firstMemory: restarted.firstMemory, memoryBefore: setup.memory },
    restarted.marker === null && restarted.firstMemory === "worker.boot");
  check("storage.session survives the stop (L187 tab-to-artifact map)", "probeSession unchanged",
    restarted.session, same(restarted.session, session),
    { tabMapBefore: setup.tabMap, tabMapAfter: restarted.tabMap });
  check("a pending storage.local activity total survives the stop (L175)", "focused_secs 42, unchanged",
    restarted.pending && { focused_secs: restarted.pending.focused_secs, hour: restarted.pending.hour, artifact_id: restarted.pending.artifact_id },
    same(restarted.pending, pending));

  // Chrome's own idle stop. With DevTools detached and no events, an MV3
  // worker is expected to stop after about 30 seconds. The control tab stays
  // open, as the side panel might. Polling Target.getTargets does not attach.
  const idleBootId = restarted.bootId;
  const idleSince = Date.now();
  let idleStoppedAfter = null;
  while (Date.now() - idleSince < IDLE_WAIT_MS) {
    if (ctx.signal.aborted) throw ctx.signal.reason;
    if (!(await ctx.workerTarget())) {
      idleStoppedAfter = Date.now() - idleSince;
      break;
    }
    await ctx.delay(1_000);
  }
  let idleRestart = null;
  if (idleStoppedAfter !== null) {
    const idleWake = ctx.urls.page("idle-wake");
    const idleWakeSince = Date.now();
    await ctx.navigate(ctx.homeTargetId, idleWake, { transitionType: "link" });
    const bootId = await ctx.waitForRestart(idleBootId).catch(() => null);
    const commit = await ctx.waitForEvent(mainCommit(idleWake), { since: idleWakeSince }).catch(() => null);
    const state = await ctx.inWorker(`Promise.all([chrome.storage.session.get(${JSON.stringify(SESSION_KEY)}), chrome.storage.local.get(${JSON.stringify(PENDING_KEY)})])
      .then(([session, local]) => ({ session: session.${SESSION_KEY} ?? null, pending: local.${PENDING_KEY} ?? null }))`);
    idleRestart = { bootId, commitBoot: commit?.bootId ?? null, sessionKept: same(state.session, session), pendingKept: same(state.pending, pending) };
  }
  check("an idle worker stops by itself after about 30 s, and state survives that stop too",
    `stopped after ${IDLE_MIN_MS / 1000}-${IDLE_MAX_MS / 1000} s; new bootId; session and local kept`,
    { stoppedAfterMs: idleStoppedAfter, waitedMs: IDLE_WAIT_MS, restart: idleRestart && { ...idleRestart, bootId: idleRestart.bootId?.slice(0, 8) ?? null, commitBoot: idleRestart.commitBoot?.slice(0, 8) ?? null } },
    idleStoppedAfter !== null && idleStoppedAfter >= IDLE_MIN_MS && idleStoppedAfter <= IDLE_MAX_MS && idleRestart?.bootId !== null && idleRestart?.bootId !== idleBootId
      && idleRestart.commitBoot === idleRestart.bootId && idleRestart.sessionKept && idleRestart.pendingKept,
    { note: "The control tab (an extension page) stayed open while waiting for the idle stop." });

  // Full browser restart on the same profile. Browser.close is graceful.
  const lastBootId = await ctx.workerBootId();
  const { pageTargets } = await ctx.relaunch();
  const relaunchBootId = await ctx.workerBootId();
  const afterRelaunch = await ctx.inWorker(`(async () => ({
    session: await chrome.storage.session.get(null),
    pending: (await chrome.storage.local.get(${JSON.stringify(PENDING_KEY)}))[${JSON.stringify(PENDING_KEY)}] ?? null,
    log: ((await chrome.storage.local.get("log")).log ?? []).map((entry) => ({ bootId: entry.bootId, kind: entry.kind })),
    localAccess: await probe.attempt(() => chrome.storage.local.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" })),
  }))()`);
  const bootKinds = afterRelaunch.log.filter((entry) => entry.bootId === relaunchBootId).map((entry) => entry.kind);
  const lifecycle = bootKinds.filter((kind) => kind.startsWith("runtime.") || kind === "worker.boot");
  // The new worker's own events refill the recorder's tab map at once, so
  // only the pre-restart values count: probeSession and the fixture URLs the
  // map held before.
  const oldUrls = [source, wake, ctx.urls.page("idle-wake")];
  const restartMap = afterRelaunch.session.tabmap ?? {};
  const staleEntries = Object.values(restartMap).filter((url) => oldUrls.includes(url));
  check("after a browser restart plus recorder reload storage.session has none of the pre-restart values", "no probeSession; no pre-restart tab map entry",
    { keys: Object.keys(afterRelaunch.session), probeSession: afterRelaunch.session[SESSION_KEY] ?? null, tabmap: restartMap },
    afterRelaunch.session[SESSION_KEY] === undefined && staleEntries.length === 0,
    { inconclusive: true,
      note: "Inconclusive for the restart alone: the relaunch reloads the recorder with Extensions.loadUnpacked, which also clears storage.session, so this cannot separate a restart from a reload." });
  check("after a browser restart plus unpacked reload the pending storage.local total is kept (L175)", "focused_secs 42, unchanged",
    afterRelaunch.pending && { focused_secs: afterRelaunch.pending.focused_secs, artifact_id: afterRelaunch.pending.artifact_id },
    same(afterRelaunch.pending, pending),
    { logBootsBefore: [...new Set(afterRelaunch.log.filter((entry) => entry.bootId !== relaunchBootId).map((entry) => entry.bootId.slice(0, 8)))],
      lastBootBefore: lastBootId.slice(0, 8), relaunchBoot: relaunchBootId.slice(0, 8), relaunchLifecycle: lifecycle,
      pagesBeforeReload: pageTargets.length,
      note: "Shown for a restart plus an Extensions.loadUnpacked reload of the same directory and ID, not for a persistently installed extension." });
  return findings;
}
