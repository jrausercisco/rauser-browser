// Follow-up to group 5 of the M1.5a step 6.1 probes (DESIGN.md §12.2). §5.1
// (Opens) left open whether an installed worker is running, or is woken, in
// time to see the reload commit of a tab restored with the session. In
// discard-restore the recorder was loaded after startup and missed the active
// tab's commit.
//
// Here the recorder is named with --load-extension on every launch, the
// closest this harness gets to an installed extension without touching global
// Chrome config. It is not the real path: Chrome loads an installed extension
// from prefs during ExtensionService init, with the lazy listeners its worker
// registered in earlier sessions, while a --load-extension one goes through
// UnpackedInstaller at each startup, and (the probe checks this) as a fresh
// install. Branded Chrome 154 ignores --load-extension; run this probe with
// --chrome pointing at Chrome for Testing. Under a Chrome that ignores the
// switch it reports that as inconclusive and restores the usual recorder.
//
// Order: one launch with the switch and no restore (the recorder's first
// session), four fixture tabs (three on the granted origin, one not), then
// relaunches with --restore-last-session, each with a different active tab.
//   free runs: nothing is held, so this is the real startup order for a
//     --load-extension worker. Nothing attaches to the worker until every
//     restored tab has asked the fixture and a settle delay has passed.
//   held runs: the fixture parks every request, the recorder's worker is
//     stopped once it has run for onInstalled, and then the requests are
//     released, so each restored tab commits while the extension is loaded,
//     its listeners are registered, and its worker is not running: the state
//     an installed extension is in when Chrome restores tabs, if Chrome loads
//     it first.

export const name = "restore-order";
export const title = "Restored tabs' reload commits and a worker loaded at startup (§5.1 Opens)";

const FREE_RUNS = 5;
const HELD_RUNS = 3;
const SETTLE_MS = 2_000;

const mainCommit = (url) => (entry) => entry.kind === "onCommitted" && entry.details.frameId === 0 && entry.details.url === url;
const byTime = (a, b) => a.at - b.at || a.seq - b.seq;
const pathOf = (url) => url.slice(url.indexOf("/", url.indexOf("//") + 2));

// The fixture's record of url's first request since index, if any.
function requestFor(ctx, url, index) {
  const wanted = url.slice(url.indexOf("//") + 2);
  return ctx.fixture.requests.slice(index).find((entry) => entry.url === wanted) ?? null;
}

async function measureRun(ctx, { run, urls, active, held }) {
  await ctx.activate((await ctx.findTarget(active)).targetId);
  await ctx.delay(300);
  const requestIndex = ctx.fixture.requests.length;
  await ctx.relaunch({ restoreSession: true, recorder: "flag", beforeLaunch: held ? () => ctx.fixture.hold() : null });
  const launchedAt = ctx.launchedAt;
  let stopped = null;
  let releasedAt = null;
  if (held) {
    // The worker starts for onInstalled; stop it only after that is logged.
    await ctx.waitForEvent((entry) => entry.kind === "runtime.onInstalled", { since: launchedAt, timeout: 10_000 }).catch(() => null);
    stopped = await ctx.stopWorker();
    releasedAt = Date.now();
    ctx.fixture.release();
  }
  await ctx.waitFor(() => ctx.requireCondition(urls.every((url) => requestFor(ctx, url, requestIndex)),
    "Not every restored tab has asked the fixture yet"), { timeout: 15_000, interval: 200 }).catch(() => null);
  await ctx.delay(SETTLE_MS);

  const log = (await ctx.readLog({ since: launchedAt })).sort(byTime);
  const boots = log.filter((entry) => entry.kind === "worker.boot");
  const firstEventOf = (boot) => log.find((entry) => entry.bootId === boot.bootId && entry.kind !== "worker.boot")?.kind ?? null;
  return {
    run,
    held,
    stoppedBy: stopped?.method ?? null,
    releasedMs: releasedAt === null ? null : releasedAt - launchedAt,
    boots: boots.map((boot) => ({ ms: boot.at - launchedAt, firstEvent: firstEventOf(boot) })),
    onStartup: log.some((entry) => entry.kind === "runtime.onStartup"),
    onInstalled: log.filter((entry) => entry.kind === "runtime.onInstalled").map((entry) => entry.details.reason),
    tabs: urls.map((url) => {
      const request = requestFor(ctx, url, requestIndex);
      const commit = log.find(mainCommit(url)) ?? null;
      const boot = commit ? boots.findIndex((entry) => entry.bootId === commit.bootId) : -1;
      return {
        path: pathOf(url),
        active: url === active,
        requestMs: request ? request.at - launchedAt : null,
        commit: commit ? commit.details.transitionType : null,
        commitMs: commit ? commit.at - launchedAt : null,
        // Which boot of this launch logged the commit, and whether it was
        // already running when the tab asked the fixture.
        commitBoot: boot < 0 ? null : boot,
        bootBeforeRequest: request && boots[0] ? boots[0].at <= request.at : null,
      };
    }),
    // From the commits, because chrome.tabs hides the ungranted tab's URL.
    tabIds: Object.fromEntries(urls.map((url) => [url, log.find(mainCommit(url))?.details.tabId ?? null])),
    events: log.filter((entry) => ["worker.boot", "runtime.onStartup", "runtime.onInstalled", "onBeforeNavigate", "onCommitted"].includes(entry.kind))
      .slice(0, 40).map((entry) => `${entry.at - launchedAt}ms ${ctx.brief(entry)}`),
  };
}

const summary = (entry) => ({
  run: entry.run,
  boots: entry.boots.map((boot) => `${boot.ms} ms (first: ${boot.firstEvent})`),
  ...(entry.held ? { releasedMs: entry.releasedMs, stoppedBy: entry.stoppedBy } : {}),
  tabs: entry.tabs.map((tab) => `${tab.path}${tab.active ? " (active)" : ""}: req ${tab.requestMs} ms, `
    + `${tab.commit ?? "MISSED"}${tab.commit ? ` by boot ${tab.commitBoot}` : ""}`),
});

// What the worker can read about a tab's main frame without "tabs": the
// webNavigation lookups, next to chrome.tabs.get, which gives url only on
// granted origins. ids maps url -> tab id.
function frameLookups(ctx, ids) {
  const entries = Object.entries(ids).filter(([, id]) => id !== null);
  return ctx.inWorker(`Promise.all(${JSON.stringify(entries)}.map(async ([url, tabId]) => {
    const settle = (promise) => promise.then((value) => ({ ok: true, value }), (error) => ({ ok: false, error: String(error?.message ?? error) }));
    const frame = await settle(chrome.webNavigation.getFrame({ tabId, frameId: 0 }));
    const frames = await settle(chrome.webNavigation.getAllFrames({ tabId }));
    const tab = await settle(chrome.tabs.get(tabId));
    return { url, tabId,
      getFrame: frame.ok ? frame.value?.url ?? null : "error: " + frame.error,
      getAllFramesMain: frames.ok ? frames.value?.find((entry) => entry.frameId === 0)?.url ?? null : "error: " + frames.error,
      tabsGet: tab.ok ? tab.value?.url ?? null : "error: " + tab.error };
  }))`);
}

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, extra = {}) => findings.push({
    claim, expected, observed, matches, verdict: extra.verdict ?? (matches ? "CONFIRMED" : "REFUTED"), ...extra,
  });

  // --- The recorder's first session, loaded with --load-extension ------------
  const firstAt = Date.now();
  try {
    await ctx.relaunch({ recorder: "flag" });
  } catch (error) {
    await ctx.relaunch();
    check("harness: --load-extension loads the recorder at startup", "the recorder's control page loads",
      error.message, true, { inconclusive: true, verdict: "UNPROVABLE_HEADLESS",
        note: `${ctx.chromeVersion.product} ignores --load-extension. Rerun with --chrome <Chrome for Testing binary>.` });
    return findings;
  }
  const info = (await ctx.extensionsInfo()).find((entry) => entry.id === ctx.extensionId) ?? null;
  const firstLog = await ctx.readLog({ since: firstAt });
  check("harness: --load-extension loads the recorder at startup", "the recorder is loaded without Extensions.loadUnpacked",
    { location: info?.location ?? null, state: info?.state ?? null,
      onInstalled: firstLog.filter((entry) => entry.kind === "runtime.onInstalled").map((entry) => entry.details.reason) },
    info?.state === "ENABLED");

  const granted = [ctx.urls.browse("ORD-1"), ctx.urls.browse("ORD-2"), ctx.urls.browse("ORD-3")];
  const urls = [...granted, ctx.urls.other("/page?n=ungranted")];
  for (const url of urls) await ctx.openTab(url);

  const runs = [];
  for (let index = 0; index < FREE_RUNS + HELD_RUNS; index += 1) {
    const held = index >= FREE_RUNS;
    const entry = await measureRun(ctx, { run: index + 1, urls, active: granted[index % granted.length], held });
    runs.push(entry);
    ctx.log(`run ${entry.run}${held ? " (held)" : ""}: ${JSON.stringify(summary(entry))}`);
  }
  const free = runs.filter((entry) => !entry.held);
  const heldRuns = runs.filter((entry) => entry.held);

  // Fidelity: an installed extension is not reinstalled at startup, so it
  // gets runtime.onStartup and keeps its lazy listeners. If this ever
  // changes, the free runs below become the installed case.
  const reinstalled = runs.every((entry) => entry.onInstalled.includes("install") && !entry.onStartup);
  check("harness: a --load-extension recorder is installed afresh at every launch",
    "runtime.onInstalled \"install\" and no runtime.onStartup in every run, unlike an installed extension",
    runs.map((entry) => ({ run: entry.run, onInstalled: entry.onInstalled, onStartup: entry.onStartup })), reinstalled,
    { note: "So the free runs show a worker registered during startup, not one Chrome knew from prefs; runtime.onStartup stays untested." });

  // The real startup order for this worker: the active tab asks the server
  // before the worker is up. Background tabs commit seconds later.
  const activeMissed = free.every((entry) => entry.tabs.find((tab) => tab.active).commit === null
    && entry.tabs.find((tab) => tab.active).bootBeforeRequest === false);
  const backgroundSeen = free.every((entry) => entry.tabs.filter((tab) => !tab.active).every((tab) => tab.commit === "reload"));
  check("a --load-extension worker misses the active restored tab's commit and sees the background tabs' reload commits",
    "active tab: requested before the first worker.boot, no onCommitted; others: reload (§5.1 Opens)",
    free.map(summary), activeMissed && backgroundSeen,
    { inconclusive: true, verdict: activeMissed && backgroundSeen ? "CONFIRMED" : "PARTIAL", runs: free,
      note: "Same order as the loadUnpacked recorder in discard-restore; an installed extension is loaded earlier, from prefs, so this does not settle its case." });

  // With the extension loaded and the worker stopped, does a restored tab's
  // reload commit start the worker and reach its listener? The released
  // active tab fires tabs.onUpdated just before its commit, and both arrive
  // at the woken worker, so either may be the first event.
  const woken = heldRuns.every((entry) => entry.stoppedBy !== null
    && entry.tabs.every((tab) => tab.commit === "reload" && tab.commitBoot !== null && tab.commitBoot >= 1)
    && ["tabs.onUpdated", "onBeforeNavigate", "onCommitted"].includes(entry.boots[1]?.firstEvent));
  check("a restored tab's reload commit wakes a stopped worker whose extension is loaded, and reaches its listener",
    "every restored tab logged as reload by a worker started after the stop by that tab's events",
    heldRuns.map(summary), woken, { runs: heldRuns,
      note: "Requests were parked until the worker was stopped, so the extension was loaded, with its listeners registered, before each commit, as an installed one is if Chrome loads it before restoring tabs." });

  // --- What a worker could rebuild the map from (option (b) in §5.1) ----------
  const lookups = await frameLookups(ctx, runs.at(-1).tabIds);
  const grantedLookups = lookups.filter((entry) => granted.includes(entry.url));
  check("webNavigation.getFrame and getAllFrames return a restored tab's main-frame URL without \"tabs\"",
    "the tab's URL from both lookups on a granted origin",
    lookups, grantedLookups.length === granted.length
      && grantedLookups.every((entry) => entry.getFrame === entry.url && entry.getAllFramesMain === entry.url),
    { note: "The ungranted row shows whether the lookups also reveal URLs that chrome.tabs hides without a grant." });
  return findings;
}
