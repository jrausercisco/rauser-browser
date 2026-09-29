// Group 5 of the M1.5a step 6.1 probes (DESIGN.md §12.2). §5.1 L167 says a
// commit with transition type "reload" is never an open, and that Chrome uses
// that type when the user switches to a tab it discarded and for tabs
// restored with a session. This probe records what Chrome actually commits in
// each case, what happens to the tab ID on a discard, and whether a worker
// registered at top level is running early enough at browser startup to see
// the restored tabs' commits at all.
//
// Order: two artifact-like tabs are discarded from the recorder and switched
// to, one with chrome.tabs.update and one with Target.activateTarget. A third
// is closed and reopened by a separate sessions helper, so the recorder keeps
// Brauser's permissions. Then Chrome is relaunched twice on the same profile
// with --restore-last-session: first with the fixture holding every request
// until the recorder is loaded again, then with the recorder also named by
// --load-extension, so it can exist before the restored tabs commit.

import path from "node:path";
import { fileURLToPath } from "node:url";

export const name = "discard-restore";
export const title = "Discarded-tab switch, reopened tab, and session restore commit as reload (§5.1 L167)";

const HELPER = path.join(path.dirname(fileURLToPath(import.meta.url)), "sessions-helper");

const mainCommit = (url) => (entry) => entry.kind === "onCommitted" && entry.details.frameId === 0 && entry.details.url === url;
const transition = (entry) => (entry ? `${entry.details.transitionType} ${JSON.stringify(entry.details.transitionQualifiers ?? [])}` : "no onCommitted");
const isReload = (entry) => entry?.details.transitionType === "reload";
const byTime = (a, b) => a.at - b.at || a.seq - b.seq;

// Records that concern these tab IDs, in arrival order, as one-line briefs.
function sliceFor(ctx, log, tabIds) {
  const ids = new Set(tabIds.filter((id) => id !== null && id !== undefined));
  return log.filter((entry) => {
    const { details = {} } = entry;
    return ids.has(details.tabId) || ids.has(details.replacedTabId) || ids.has(details.addedTabId)
      || ids.has(details.removedTabId) || ids.has(details.sourceTabId) || ids.has(details.tab?.id);
  }).sort(byTime).map((entry) => ctx.brief(entry));
}

// Evaluate in the sessions helper's worker, with the same short timeout and
// retries the runner uses for the recorder. An action with side effects must
// pass retries: 0, after a warm-up call, because a retry after a timed-out
// first attempt would close or restore a second time.
async function inHelper(ctx, id, expression, { retries = 3 } = {}) {
  const url = `chrome-extension://${id}/worker.js`;
  let lastError;
  for (let attempt = 0; attempt <= retries; attempt += 1) {
    const target = await ctx.waitFor(async () => {
      const found = (await ctx.cdp.targets()).find((entry) => entry.type === "service_worker" && entry.url === url);
      ctx.requireCondition(found, "The sessions helper worker is not running");
      return found;
    }, { timeout: 12_000, interval: 200 });
    let page;
    try {
      page = await ctx.attach(target.targetId);
      return await page.evaluate(expression, { timeout: 6_000 });
    } catch (error) {
      lastError = error;
      if (!/timed out|No target|not found|detached/i.test(error.message)) throw error;
    } finally {
      await page?.detach();
    }
  }
  throw lastError;
}

// Discard the tab showing url from the recorder, then switch to it. Returns
// what Chrome did at each step; nothing here throws on a surprising answer.
async function discardAndSwitch(ctx, url, how) {
  const targetBefore = (await ctx.findTarget(url)).targetId;
  const oldTabId = await ctx.tabIdFor(url);
  const since = Date.now();
  const discarded = await ctx.inWorker(`probe.discard(${oldTabId})`);
  const result = { url, how, oldTabId, discard: discarded, newTabId: null, commit: null };
  if (!discarded.ok) return result;
  const newTabId = discarded.value?.id ?? null;
  result.newTabId = newTabId;
  await ctx.waitForEvent((entry) => entry.kind === "tabs.onReplaced" && entry.details.removedTabId === oldTabId,
    { since, timeout: 3_000 }).catch(() => null);
  result.tabAfterDiscard = (await ctx.tabs()).find((tab) => tab.id === newTabId) ?? null;
  // The recorder's map is keyed by tab ID and does not follow onReplaced, so
  // this is what a lookup at the switch would find without the re-keying
  // §5.1 now asks for: the old ID still mapped, the new one not.
  const map = await ctx.inWorker("probe.tabMap()");
  result.mapAtSwitch = { oldIdMapped: map[oldTabId] ?? null, newIdMapped: map[newTabId] ?? null };
  const targetAfter = await ctx.findTarget(url).then((target) => target.targetId, () => null);
  result.targetIdChanged = targetAfter === null ? "no target" : targetAfter !== targetBefore;

  const requestsBefore = ctx.fixture.requests.length;
  const switchedAt = Date.now();
  if (how === "chrome.tabs.update") {
    result.switch = await ctx.control(`chrome.tabs.update(${newTabId}, { active: true }).then((tab) => ({ ok: true, id: tab.id }), (error) => ({ ok: false, error: String(error) }))`);
  } else {
    result.switch = targetAfter === null ? { ok: false, error: "no page target for the discarded tab" }
      : await ctx.activate(targetAfter).then(() => ({ ok: true }), (error) => ({ ok: false, error: error.message }));
  }
  const commit = await ctx.waitForEvent(mainCommit(url), { since: switchedAt, timeout: 8_000 }).catch(() => null);
  await ctx.delay(400);
  const log = await ctx.readLog({ since });
  result.commit = commit && { transitionType: commit.details.transitionType, transitionQualifiers: commit.details.transitionQualifiers, tabId: commit.details.tabId };
  result.refetched = ctx.fixture.requests.slice(requestsBefore).filter((request) => url.endsWith(request.url.slice(request.url.indexOf("/")))).length;
  const activated = log.find((entry) => entry.kind === "tabs.onActivated" && entry.details.tabId === newTabId && entry.at >= switchedAt);
  result.activatedBeforeCommit = Boolean(activated && commit && byTime(activated, commit) < 0);
  // Both replace events must name old -> new and arrive before the switch's
  // tabs.onActivated, or a worker re-keying on them would still miss.
  const tabsReplaced = log.find((entry) => entry.kind === "tabs.onReplaced" && entry.details.removedTabId === oldTabId);
  const navReplaced = log.find((entry) => entry.kind === "onTabReplaced" && entry.details.replacedTabId === oldTabId);
  const before = (entry) => Boolean(entry && activated && byTime(entry, activated) < 0);
  result.replaced = {
    tabsOnReplaced: tabsReplaced ? `${tabsReplaced.details.removedTabId} -> ${tabsReplaced.details.addedTabId}` : null,
    onTabReplaced: navReplaced ? `${navReplaced.details.replacedTabId} -> ${navReplaced.details.tabId}` : null,
    toNewId: tabsReplaced?.details.addedTabId === newTabId && navReplaced?.details.tabId === newTabId,
    beforeActivate: before(tabsReplaced) && before(navReplaced),
  };
  result.events = sliceFor(ctx, log, [oldTabId, newTabId]);
  return result;
}

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, extra = {}) => findings.push({
    claim, expected, observed, matches, verdict: extra.verdict ?? (matches ? "CONFIRMED" : "REFUTED"), ...extra,
  });

  // --- Discard, then switch ---------------------------------------------------
  const urlA = ctx.urls.browse("DIS-1");
  const urlB = ctx.urls.browse("DIS-2");
  const targetA = await ctx.openTab(urlA);
  await ctx.openTab(urlB);
  await ctx.activate(targetA);
  await ctx.delay(300);

  const first = await discardAndSwitch(ctx, urlB, "chrome.tabs.update");
  // B is now active, so A is the background tab for the second switch.
  const second = await discardAndSwitch(ctx, urlA, "Target.activateTarget");
  const switches = [first, second];

  check("harness: chrome.tabs.discard works without the \"tabs\" permission", "resolves with the discarded tab",
    switches.map((entry) => (entry.discard.ok ? `ok discarded=${entry.discard.value?.discarded}` : entry.discard.error)),
    switches.every((entry) => entry.discard.ok),
    { note: "Brauser never discards; the recorder calls chrome.tabs.discard only to create the case." });
  for (const entry of switches) {
    check(`switching to a discarded tab commits as reload (switch by ${entry.how})`, "reload (§5.1 L167)",
      entry.commit ? `${entry.commit.transitionType} ${JSON.stringify(entry.commit.transitionQualifiers)}` : "no onCommitted",
      entry.commit?.transitionType === "reload", { details: entry });
  }
  // Step 6.1 found that a discard changes the tab ID; §5.1 (Opens) now has
  // the worker follow tabs.onReplaced, so these two guard that behavior.
  check("a discard gives the tab a new ID, announced by tabs.onReplaced and webNavigation.onTabReplaced",
    "new ID; both replace events name old -> new before the switch's tabs.onActivated (§5.1 Opens)",
    switches.map((entry) => ({ ids: `${entry.oldTabId} -> ${entry.newTabId}`, targetChanged: entry.targetIdChanged, ...entry.replaced })),
    switches.every((entry) => entry.discard.ok && entry.newTabId !== entry.oldTabId && entry.replaced?.toNewId && entry.replaced?.beforeActivate),
    { note: "If a later Chrome keeps the ID, re-keying on onReplaced is harmless, but §5.1's note on discards should be revisited." });
  check("a map that follows tabs.onReplaced has the tab's artifact at the switch's tabs.onActivated",
    "old ID mapped, and the replace arrives before tabs.onActivated, which comes before the reload commit (§5.1 Opens)",
    switches.map((entry) => ({ activatedBeforeCommit: entry.activatedBeforeCommit, ...entry.mapAtSwitch,
      replacedBeforeActivate: entry.replaced?.beforeActivate ?? false })),
    switches.every((entry) => Boolean(entry.mapAtSwitch?.oldIdMapped) && entry.replaced?.beforeActivate && entry.activatedBeforeCommit),
    { note: "The recorder's map updates on commits and route changes only, so newIdMapped is null; moved on the replace events, the entry is oldIdMapped." });

  // --- Reopen a closed tab (sessions helper) ---------------------------------
  const urlC = ctx.urls.browse("DIS-3");
  await ctx.openTab(urlC);
  const helper = await ctx.loadExtension(HELPER);
  const closedTabId = await ctx.tabIdFor(urlC);
  const reopenSince = Date.now();
  await inHelper(ctx, helper.id, "typeof helper.reopen");
  const reopened = await inHelper(ctx, helper.id, `helper.reopen(${closedTabId})`, { retries: 0 }).catch((error) => ({ error: error.message }));
  const reopenCommit = await ctx.waitForEvent(mainCommit(urlC), { since: reopenSince, timeout: 8_000 }).catch(() => null);
  await ctx.delay(300);
  const reopenLog = await ctx.readLog({ since: reopenSince });
  check("a tab reopened with chrome.sessions.restore commits as reload", "reload (§5.1 L167: restored tabs)",
    { commit: transition(reopenCommit), closedTabId, reopened,
      createdNavigationTarget: reopenLog.some((entry) => entry.kind === "onCreatedNavigationTarget") },
    isReload(reopenCommit) && Number.isInteger(reopened.tabId) && reopened.tabId !== closedTabId && reopenCommit.details.tabId === reopened.tabId,
    { events: sliceFor(ctx, reopenLog, [closedTabId, reopened.tabId]), note: "The sessions helper, not the recorder, holds \"sessions\"." });

  // --- The session to restore: three issue tabs and a board modal ------------
  const targetBoard = await ctx.openTab(ctx.urls.board);
  const boardUrl = `${ctx.urls.board}?selectedIssue=KEY-7`;
  const boardSince = Date.now();
  await ctx.withPage(targetBoard, (page) => page.evaluate("board.select('KEY-7')", { userGesture: true }));
  await ctx.waitForEvent((entry) => entry.kind === "onHistoryStateUpdated" && entry.details.url === boardUrl, { since: boardSince }).catch(() => null);
  await ctx.activate((await ctx.findTarget(urlA)).targetId);
  await ctx.delay(500);
  const restoredUrls = [urlA, urlB, urlC, boardUrl];
  const sessionTabs = (await ctx.tabs()).filter((tab) => restoredUrls.includes(tab.url));

  // --- Relaunch 1: restore with every fixture request held -------------------
  // The recorder, loaded with Extensions.loadUnpacked, is gone until the
  // runner reloads it, so the gate parks the restored tabs' requests and
  // their commits wait for the recorder.
  const relaunchAt = Date.now();
  const requestsBeforeRelaunch = ctx.fixture.requests.length;
  const heldRestart = await ctx.relaunch({ restoreSession: true, beforeLaunch: () => ctx.fixture.hold() });
  const recorderBackAt = Date.now();
  const held = ctx.fixture.heldCount();
  const beforeRelease = (await ctx.readLog({ since: relaunchAt })).filter((entry) => entry.kind === "onCommitted"
    && restoredUrls.includes(entry.details.url));
  const released = ctx.fixture.release();
  const heldCommits = {};
  for (const url of restoredUrls) {
    heldCommits[url] = await ctx.waitForEvent(mainCommit(url), { since: relaunchAt, timeout: 10_000 }).catch(() => null);
  }
  // Headless may load background restored tabs eagerly; activating each shows
  // whether a lazily restored tab commits again.
  const activateSince = Date.now();
  for (const url of restoredUrls) {
    const target = await ctx.findTarget(url, { timeout: 3_000 }).catch(() => null);
    if (target) await ctx.activate(target.targetId);
    await ctx.delay(400);
  }
  const afterActivate = (await ctx.readLog({ since: activateSince })).filter((entry) => entry.kind === "onCommitted"
    && restoredUrls.includes(entry.details.url)).map((entry) => ctx.brief(entry));
  const heldLog = await ctx.readLog({ since: relaunchAt });
  // When each restored tab asked the fixture, relative to the recorder being
  // back (negative: parked by the gate before the recorder loaded), and
  // whether its onBeforeNavigate, which may precede the reload, was seen.
  const restoreTiming = Object.fromEntries(restoredUrls.map((url) => {
    const pathname = url.slice(ctx.origin.length);
    const request = ctx.fixture.requests.slice(requestsBeforeRelaunch).find((entry) => entry.url.endsWith(pathname));
    const commit = heldCommits[url];
    return [pathname, {
      requestMs: request ? request.at - recorderBackAt : null,
      commitMs: commit ? commit.at - recorderBackAt : null,
      onBeforeNavigateSeen: heldLog.some((entry) => entry.kind === "onBeforeNavigate" && entry.details.frameId === 0 && entry.details.url === url),
    }];
  }));

  check("harness: the hold gate parked the restored tabs' requests until the recorder was back",
    "requests held, no restored commit before release",
    { pagesRestored: heldRestart.pageTargets.map((target) => target.url), heldAtRecorderLoad: held, released,
      commitsBeforeRelease: beforeRelease.length },
    held > 0 && beforeRelease.length === 0);
  check("tabs restored with the session commit as reload (--restore-last-session)", "reload for every restored tab (§5.1 L167)",
    Object.fromEntries(restoredUrls.map((url) => [url.slice(ctx.origin.length), transition(heldCommits[url])])),
    restoredUrls.every((url) => isReload(heldCommits[url])),
    { sessionTabs, restoreTiming, commitsOnLaterActivation: afterActivate,
      events: heldLog.filter((entry) => ["onCommitted", "onHistoryStateUpdated", "runtime.onStartup", "runtime.onInstalled", "worker.boot"].includes(entry.kind))
        .sort(byTime).map((entry) => ctx.brief(entry)),
      note: "Commits were held until the recorder was reloaded, so this shows the transition type, not startup timing." });

  // --- Relaunch 2: the recorder present at startup ---------------------------
  // Extensions.loadUnpacked lasts one browser session, so --load-extension
  // is the only way to have the recorder installed before restore. Branded
  // Chrome may ignore that switch; the log shows which happened. No hold, so
  // restored tabs commit as soon as Chrome restores them.
  const startupArgs = [`--load-extension=${ctx.recorderDir}`, "--disable-features=DisableLoadExtensionCommandLineSwitch"];
  const startupAt = Date.now();
  const startupRequests = ctx.fixture.requests.length;
  const startupRestart = await ctx.relaunch({ restoreSession: true, args: startupArgs });
  const startupBackAt = Date.now();
  // Background restored tabs load a couple of seconds after the active one,
  // so wait for each before judging it missed.
  for (const url of restoredUrls) {
    await ctx.waitForEvent(mainCommit(url), { since: startupAt, timeout: url === restoredUrls[0] ? 8_000 : 4_000 }).catch(() => null);
  }
  const finalBoot = await ctx.workerBootId();
  const startupLog = (await ctx.readLog({ since: startupAt })).sort(byTime);
  // The slice starts with the closing browser's last records, so only a
  // worker.boot in it marks a worker that ran in the relaunched browser.
  const earlyBoots = startupLog.filter((entry) => entry.kind === "worker.boot" && entry.bootId !== finalBoot)
    .map((entry) => entry.bootId);
  const startupCommits = {};
  // Whether each restored tab asked the fixture after the recorder was back;
  // only those commits can reach a worker that registers after startup.
  const requestedAfterBack = {};
  for (const url of restoredUrls) {
    const pathname = url.slice(ctx.origin.length);
    const commit = startupLog.find(mainCommit(url));
    const request = ctx.fixture.requests.slice(startupRequests).find((entry) => entry.url.endsWith(pathname));
    const requestMs = request ? `request ${request.at - startupBackAt} ms after the recorder was back` : "no request";
    requestedAfterBack[url] = Boolean(request && request.at >= startupBackAt);
    startupCommits[pathname] = `${commit ? `${transition(commit)} boot=${commit.bootId === finalBoot ? "reloaded" : "startup"}` : "not logged"}; ${requestMs}`;
  }
  const runtimeKinds = startupLog.filter((entry) => entry.kind.startsWith("runtime.") || entry.kind === "worker.boot")
    .map((entry) => `${entry.kind}${entry.details.reason ? `(${entry.details.reason})` : ""} boot=${entry.bootId === finalBoot ? "reloaded" : "startup"}`);
  const presentAtStartup = earlyBoots.length > 0;
  const allLogged = restoredUrls.every((url) => isReload(startupLog.find(mainCommit(url))));
  const events = startupLog.map((entry) => ctx.brief(entry)).slice(0, 80);
  // A Chrome that ignores --load-extension leaves the startup ordering
  // untestable here, which is a harness limit, not a Chrome finding, so it is
  // reported as inconclusive rather than as a mismatch.
  check("harness: whether --load-extension has the recorder running before the restored tabs commit",
    "a worker.boot before the Extensions.loadUnpacked one (inconclusive if Chrome ignores the switch)",
    { recorderRunningBeforeLoadUnpacked: presentAtStartup, runtime: runtimeKinds }, true,
    presentAtStartup ? {} : { inconclusive: true, verdict: "UNPROVABLE_HEADLESS", note: `${ctx.chromeVersion.product} ignored --load-extension (the feature override did not bring it back); only one worker.boot, with onInstalled reason "install", after the runner's loadUnpacked.` });
  if (!presentAtStartup) {
    // What a worker that registers late does see: this is the only startup
    // ordering the harness can produce. §5.1 (Opens) now states that such a
    // worker misses a commit Chrome made before it was running, so the check
    // is that every tab requested after the recorder was back is logged as
    // reload; which tabs those are varies by run.
    const lateSeen = restoredUrls.filter((url) => requestedAfterBack[url]);
    const lateOk = lateSeen.length > 0 && lateSeen.every((url) => isReload(startupLog.find(mainCommit(url))));
    check("a worker loaded after startup sees the restored tabs' reload commits made after it is running",
      "every restored tab requested after the recorder was back is logged as reload; earlier ones may be missed (§5.1 Opens)",
      startupCommits, lateOk,
      { inconclusive: true, verdict: allLogged ? "CONFIRMED" : "PARTIAL", events, pagesRestored: startupRestart.pageTargets.map((target) => target.url),
        note: "Brauser is installed persistently; whether its worker is running before restored tabs commit is untested headless, so this shows only a late worker's view." });
    return findings;
  }
  check("a worker installed at startup sees the restored tabs' reload commits",
    "every restored tab's onCommitted reload reaches the worker (§5.1 L167, §3.2 startup sync)",
    startupCommits, allLogged, { events });
  check("runtime.onStartup fires for a worker installed at startup", "runtime.onStartup (§3.2 sync at browser startup)",
    runtimeKinds, startupLog.some((entry) => entry.kind === "runtime.onStartup" && entry.bootId !== finalBoot));
  return findings;
}
