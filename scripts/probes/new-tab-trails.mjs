// Group 1 of DESIGN.md §12.2 step 6.1: webNavigation.onCreatedNavigationTarget
// for links and scripts that open a new tab or window. §5.1 (L189) assumes the
// event fires for a middle-click, a target=_blank link, and window.open; that it
// gives the new tabId and the sourceTabId; and that it reaches the worker before
// the new tab's first commit and before the source tab's next navigation, so a
// tab-to-artifact map read at event time still holds the source's old page.
//
// Each case opens its own source page, /list?to=<unique target>, so every
// source and target URL is distinct. The recorder's log is in arrival order,
// which is the order a serialize()d worker would handle the events in, so the
// order checks use log positions rather than timestamps. Chrome's own
// details.timeStamp deltas go into the report next to them.
//
// The race cases aim the new tab at /slow (answered after 2.5 s, or at once for
// the fast variant) and move the source on right after the click: 200 ms after
// the release is acknowledged, or "tight", sent in the same tick as the
// release with no await in between. One variant moves the source with
// pushState instead of a navigation, sent once the release is acknowledged:
// Runtime.evaluate reaches the renderer directly while the mouse events go
// through the browser's input router, so a pushState sent in the release's
// tick can run before the click is handled, and the page would then open the
// tab from the pushed URL, which is not the race L189 is about.

import { setTimeout as delay } from "node:timers/promises";

export const name = "new-tab-trails";
export const title = "onCreatedNavigationTarget: middle-click, _blank, window.open, and a source that moves on";

// CDP modifier bits.
const META = 4;
const CTRL = 2;
const SHIFT = 8;
const SLOW_MS = 2500;
const ORDER_KINDS = new Set(["onBeforeNavigate", "onCreatedNavigationTarget", "onCommitted", "onHistoryStateUpdated", "tabs.onCreated"]);

const href = (url) => new URL(url).href;
const mainFrame = (entry) => entry.details.frameId === undefined || entry.details.frameId === 0;

// Press and release on an element. `after` runs once the press is
// acknowledged: tight sends it in the same tick as the release, otherwise it
// waits for the release's acknowledgement and then `gap` ms.
async function clickWith(page, selector, { button = "left", modifiers = 0, after = null, tight = false, gap = 0 } = {}) {
  const { x, y } = await page.point(selector);
  const buttons = { left: 1, middle: 4 }[button] ?? 0;
  await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button, buttons, modifiers, clickCount: 1 });
  const released = page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button, buttons: 0, modifiers, clickCount: 1 });
  if (!after) return released;
  if (tight) return Promise.all([released, after()]);
  await released;
  if (gap) await delay(gap);
  return after();
}

// How the source moves on in a race case. Neither waits for the load.
const moveOn = {
  navigate: (page, url) => () => page.send("Page.navigate", { url, transitionType: "typed" }),
  push: (page) => () => page.send("Runtime.evaluate", { expression: "history.pushState({}, '', '?moved=push')" }),
};

// Every case: id, label, the link page's target path, the action, and what the
// design expects. core cases carry L189's claim; info cases have no DESIGN.md
// claim and are recorded as found.
function cases(ctx) {
  const page = (n) => `/page?n=${n}`;
  const slow = (n, ms = SLOW_MS) => `/slow?d=${ms}&n=${n}`;
  const race = (id, label, selector, button, { tight, gap = 0, how = "navigate", ms = SLOW_MS }) => ({
    id, label, to: slow(id, ms), kind: "core", race: how,
    // A fast target's commit races the source's: report the pair unordered.
    ...(ms === 0 ? { commitsRace: true } : {}),
    act: (driver, away) => clickWith(driver, selector, { button, tight, gap, after: moveOn[how](driver, away) }),
  });
  return [
    { id: "middle", label: "middle-click on a link", to: page("middle"), kind: "core",
      act: (driver) => clickWith(driver, "#link", { button: "middle" }) },
    { id: "meta", label: "Meta-click on a link (macOS new tab)", to: page("meta"), kind: "core",
      act: (driver) => clickWith(driver, "#link", { modifiers: META }) },
    { id: "ctrl", label: "Ctrl-click on a link (the non-macOS new-tab chord)", to: page("ctrl"), kind: "info",
      act: (driver) => clickWith(driver, "#link", { modifiers: CTRL }) },
    { id: "shift", label: "Shift-click on a link (new window)", to: page("shift"), kind: "core",
      act: (driver) => clickWith(driver, "#link", { modifiers: SHIFT }) },
    { id: "blank", label: "click on a target=_blank link", to: page("blank"), kind: "core",
      act: (driver) => clickWith(driver, "#blank") },
    { id: "noopener", label: "click on a target=_blank rel=noopener link", to: page("noopener"), kind: "core",
      act: (driver) => clickWith(driver, "#noopener") },
    { id: "open", label: "window.open from a click", to: page("open"), kind: "core",
      act: (driver) => clickWith(driver, "#open") },
    { id: "popup", label: "window.open popup (new window) from a click", to: page("popup"), kind: "core",
      act: (driver) => clickWith(driver, "#popup") },
    { id: "open-no-gesture", label: "window.open with no user gesture", to: page("open-no-gesture"), kind: "info", quiet: true,
      act: (driver) => driver.evaluate("window.open(document.querySelector('#blank').href) !== null", { userGesture: false }) },
    { id: "twice", label: "middle-click the same link twice (two opens, L169)", to: page("twice"), kind: "core", count: 2,
      act: async (driver) => {
        await clickWith(driver, "#link", { button: "middle" });
        await ctx.delay(150);
        await clickWith(driver, "#link", { button: "middle" });
      } },
    { id: "cross", label: "target=_blank to the ungranted localhost origin", to: page("cross"), kind: "cross",
      target: ctx.urls.other("/page?n=cross"), act: (driver) => clickWith(driver, "#cross") },
    race("race-middle-200", "middle-click to a slow page; source navigates 200 ms later", "#link", "middle", { gap: 200 }),
    race("race-blank-200", "_blank to a slow page; source navigates 200 ms later", "#blank", "left", { gap: 200 }),
    race("race-open-200", "window.open to a slow page; source navigates 200 ms later", "#open", "left", { gap: 200 }),
    race("race-middle-tight", "middle-click to a slow page; source navigates in the release's tick", "#link", "middle", { tight: true }),
    race("race-blank-tight", "_blank to a slow page; source navigates in the release's tick", "#blank", "left", { tight: true }),
    race("race-open-tight", "window.open to a slow page; source navigates in the release's tick", "#open", "left", { tight: true }),
    race("race-blank-tight-fast", "_blank to a fast page; source navigates in the release's tick", "#blank", "left", { tight: true, ms: 0 }),
    race("race-blank-push", "_blank to a slow page; source pushStates once the release is acknowledged", "#blank", "left", { tight: false, how: "push" }),
  ];
}

// Positions, deltas, and the order line for one case's log slice.
function summarize(log, { sourceTabId, sourceUrl, awayUrl, race, commitsRace = false }) {
  const created = log.filter((entry) => entry.kind === "onCreatedNavigationTarget" && entry.details.sourceTabId === sourceTabId);
  const newTabIds = new Set([
    ...created.map((entry) => entry.details.tabId),
    ...log.filter((entry) => entry.kind === "tabs.onCreated" && entry.details.tab?.id !== sourceTabId).map((entry) => entry.details.tab.id),
  ]);
  const at = (entry) => log.indexOf(entry);
  const firstCommit = (tabId) => log.find((entry) => entry.kind === "onCommitted" && entry.details.tabId === tabId && entry.details.frameId === 0);
  const moved = race === "push"
    ? log.find((entry) => entry.kind === "onHistoryStateUpdated" && entry.details.tabId === sourceTabId && entry.details.frameId === 0)
    : log.find((entry) => entry.kind === "onCommitted" && entry.details.tabId === sourceTabId && entry.details.frameId === 0 && entry.details.url === awayUrl);
  const sourceBefore = log.find((entry) => entry.kind === "onBeforeNavigate" && entry.details.tabId === sourceTabId && entry.details.frameId === 0);
  const opens = created.map((entry) => {
    const commit = firstCommit(entry.details.tabId);
    const newBefore = log.find((item) => item.kind === "onBeforeNavigate" && item.details.tabId === entry.details.tabId && item.details.frameId === 0);
    const tabCreated = log.find((item) => item.kind === "tabs.onCreated" && item.details.tab?.id === entry.details.tabId);
    return {
      tabId: entry.details.tabId,
      sourceTabId: entry.details.sourceTabId,
      sourceFrameId: entry.details.sourceFrameId,
      sourceProcessId: entry.details.sourceProcessId,
      url: entry.details.url,
      sourceMapped: entry.extra.sourceMapped,
      sourceMappedIsSource: entry.extra.sourceMapped === sourceUrl,
      beforeNewCommit: commit ? at(entry) < at(commit) : null,
      beforeSourceMove: moved ? at(entry) < at(moved) : null,
      afterNewBeforeNavigate: newBefore ? at(entry) > at(newBefore) : null,
      afterSourceBeforeNavigate: sourceBefore ? at(entry) > at(sourceBefore) : null,
      newCommit: commit ? { url: commit.details.url, transitionType: commit.details.transitionType, transitionQualifiers: commit.details.transitionQualifiers,
        documentLifecycle: commit.details.documentLifecycle } : null,
      // Chrome's own clock, relative to onCreatedNavigationTarget.
      chromeMs: {
        newBeforeNavigate: newBefore ? Math.round(newBefore.details.timeStamp - entry.details.timeStamp) : null,
        newCommit: commit ? Math.round(commit.details.timeStamp - entry.details.timeStamp) : null,
        sourceBeforeNavigate: sourceBefore ? Math.round(sourceBefore.details.timeStamp - entry.details.timeStamp) : null,
        sourceMove: moved ? Math.round(moved.details.timeStamp - entry.details.timeStamp) : null,
      },
      tabsOnCreated: tabCreated ? { openerTabId: tabCreated.details.tab.openerTabId ?? null, windowId: tabCreated.details.tab.windowId,
        url: tabCreated.details.tab.url ?? null, pendingUrl: tabCreated.details.tab.pendingUrl ?? null } : null,
    };
  });
  const role = (entry) => {
    if (entry.kind === "onCreatedNavigationTarget") return entry.details.sourceTabId === sourceTabId ? "created" : null;
    const tabId = entry.kind === "tabs.onCreated" ? entry.details.tab?.id : entry.details.tabId;
    if (tabId === sourceTabId) return "source";
    return newTabIds.has(tabId) ? "new" : null;
  };
  const ordered = log.filter((entry) => ORDER_KINDS.has(entry.kind) && mainFrame(entry) && role(entry));
  const label = (entry) => (role(entry) === "created" ? "created" : `${role(entry)}.${entry.kind}`)
    + (entry.kind === "onCommitted" ? `(${entry.details.transitionType})` : "");
  let order = ordered.map(label);
  // Where the new tab's first commit and the source's move genuinely race,
  // print them as one unordered {a, b} step at the earlier one's position so
  // the line is the same from run to run. The checks never compare the two.
  const racing = commitsRace && moved ? created.map((entry) => firstCommit(entry.details.tabId)).filter(Boolean).concat(moved) : [];
  if (racing.length > 1) {
    const positions = racing.map((entry) => ordered.indexOf(entry)).filter((index) => index >= 0);
    const first = Math.min(...positions);
    const pair = `{${positions.map((index) => order[index]).sort().join(", ")}}`;
    order = order.map((item, index) => (index === first ? pair : item)).filter((_, index) => index === first || !positions.includes(index));
  }
  return { opens, newTabIds: [...newTabIds], sourceMoved: moved ? { kind: moved.kind, url: moved.details.url } : null, order: order.join(" < ") };
}

async function runCase(ctx, spec) {
  await ctx.reset();
  const target = spec.target ?? `${ctx.origin}${spec.to}`;
  const sourceUrl = href(ctx.urls.listTo(spec.to));
  const awayUrl = href(ctx.urls.page(`away-${spec.id}`));
  const start = Date.now();
  // A foreground tab, so the page is visible and its clicks are not throttled.
  const sourceTarget = await ctx.openTab(sourceUrl, { background: false });
  const sourceTabId = await ctx.tabIdFor(sourceUrl);
  // The recorder's map must hold the source page before the click.
  await ctx.waitForEvent((entry) => entry.kind === "onCommitted" && entry.details.tabId === sourceTabId && entry.details.url === sourceUrl, { since: start });
  const since = Date.now();
  const driver = await ctx.attach(sourceTarget);
  let actResult = null;
  try {
    actResult = await spec.act(driver, awayUrl);
  } finally {
    await driver.detach();
  }
  const wanted = spec.count ?? 1;
  const createdFrom = (log) => log.filter((entry) => entry.kind === "onCreatedNavigationTarget" && entry.details.sourceTabId === sourceTabId);
  await ctx.waitFor(async () => ctx.requireCondition(createdFrom(await ctx.readLog({ since })).length >= wanted, "waiting for onCreatedNavigationTarget"),
    { timeout: spec.quiet || spec.kind === "info" ? 2_500 : 8_000, interval: 300 }).catch(() => undefined);
  // Let every new tab commit and the source move on before reading the slice.
  await ctx.waitFor(async () => {
    const log = await ctx.readLog({ since });
    const summary = summarize(log, { sourceTabId, sourceUrl, awayUrl, race: spec.race, commitsRace: spec.commitsRace });
    ctx.requireCondition(summary.opens.every((open) => open.newCommit), "waiting for the new tab's commit");
    ctx.requireCondition(!spec.race || summary.sourceMoved, "waiting for the source to move on");
    return true;
  }, { timeout: SLOW_MS + 6_000, interval: 300 }).catch(() => undefined);
  await ctx.delay(300);
  const log = await ctx.readLog({ since });
  const summary = summarize(log, { sourceTabId, sourceUrl, awayUrl, race: spec.race, commitsRace: spec.commitsRace });
  const targets = (await ctx.pageTargets()).map((info) => info.url);
  return { spec, target: href(target), sourceUrl, sourceTabId, awayUrl, actResult, summary, targets,
    events: log.filter(mainFrame).map((entry) => ctx.brief(entry)) };
}

const short = (qualifiers) => JSON.stringify(qualifiers ?? []);
// Fixture ports and tab IDs change every run; the observed text leaves them
// out (they stay in the slice) so reruns compare line for line.
const noPort = (text) => text.replace(/(\/\/(?:127\.0\.0\.1|localhost)):\d+/g, "$1:<port>");

function observedLine(result) {
  const { summary } = result;
  if (!summary.opens.length) {
    const opened = result.targets.includes(result.target);
    return `no onCreatedNavigationTarget; new tab ${opened ? "opened" : "not opened"}; order ${summary.order || "(none)"}`;
  }
  return summary.opens.map((open) => [
    `tab ${open.tabId === result.sourceTabId ? "=source(WRONG)" : "new"} src ${open.sourceTabId === result.sourceTabId ? "=source(ok)" : `${open.sourceTabId}(WRONG)`} frame ${open.sourceFrameId}`,
    `sourceMapped ${open.sourceMappedIsSource ? "=source page" : noPort(JSON.stringify(open.sourceMapped))}`,
    `before new commit ${open.beforeNewCommit}`,
    ...(result.spec.race ? [`before source ${result.spec.race === "push" ? "pushState" : "commit"} ${open.beforeSourceMove}`] : []),
    `new commit ${open.newCommit ? `${open.newCommit.transitionType} ${short(open.newCommit.transitionQualifiers)}` : "none"}`,
    // Chrome's timeStamp deltas vary run to run; they stay in slice.summary.opens[].chromeMs.
  ].join("; ")).join(" | ")
    + (summary.opens.length > 1 ? `; ${new Set(summary.opens.map((open) => open.tabId)).size} distinct new tabs` : "")
    + ` || order ${summary.order}`;
}

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, extra = {}) => findings.push({ claim, expected, observed, matches, ...extra });
  const results = [];
  for (const spec of cases(ctx)) {
    const result = await runCase(ctx, spec);
    results.push(result);
    ctx.log(`${spec.id}: ${observedLine(result)}`);
  }

  for (const result of results) {
    const { spec, summary } = result;
    const opens = summary.opens;
    const slice = { case: spec.id, sourceUrl: result.sourceUrl, sourceTabId: result.sourceTabId, target: result.target, awayUrl: result.awayUrl,
      actResult: result.actResult, summary, events: result.events };
    if (spec.kind === "core") {
      const good = opens.length === (spec.count ?? 1) && new Set(opens.map((open) => open.tabId)).size === opens.length && opens.every((open) =>
        open.sourceTabId === result.sourceTabId && open.url === result.target && open.sourceMappedIsSource
        && open.beforeNewCommit === true && (!spec.race || open.beforeSourceMove === true));
      const expected = `${spec.count ?? 1} event(s), distinct new tabId, sourceTabId=source, url=target, before the new tab's commit`
        + `${spec.race ? " and before the source moves on" : ""}; map lookup = the source page`;
      const noTab = !opens.length && !result.targets.includes(result.target) && !summary.newTabIds.length;
      check(`L189: ${spec.label}`, expected, observedLine(result), good,
        { case: spec.id, slice, ...(noTab ? { note: `${spec.id}: no new tab opened at all, so L189 was not exercised.` } : {}) });
    } else if (spec.kind === "cross") {
      const open = opens[0];
      // Step 6.1 found the ungranted URL in the event; §5.1 (Trails) now says
      // so and has the worker drop it, so the guard is the corrected text.
      check(`L192: ${spec.label}: onCreatedNavigationTarget reports the ungranted URL; tabs.onCreated does not`,
        "the event carries the target URL; tabs.onCreated has no url or pendingUrl (§5.1 Trails: the worker drops it before storing)",
        open ? `onCreatedNavigationTarget url=${noPort(open.url)}; tabs.onCreated url=${open.tabsOnCreated?.url} pendingUrl=${open.tabsOnCreated?.pendingUrl}; ${observedLine(result)}` : observedLine(result),
        Boolean(open) && open.url === result.target && !open.tabsOnCreated?.url && !open.tabsOnCreated?.pendingUrl,
        { case: spec.id, slice, note: "cross: webNavigation needs no host permission, so its URLs are not limited to granted origins; the worker must filter before it stores." });
    } else {
      check(`info: ${spec.label}`, "(no DESIGN.md claim; recorded as found)",
        `${spec.id === "open-no-gesture" ? `window.open returned ${result.actResult ? "a window" : "null"}; ` : ""}${observedLine(result)}`, true,
        { case: spec.id, slice });
    }
  }

  // The new tab's own first commit: L167 says a reload is never an open, so a
  // new-tab open depends on this commit not being "reload". Whether anything
  // on it marks the tab as new decides whether the commit alone could say so.
  const commits = results.flatMap((result) => result.summary.opens.map((open) => ({ case: result.spec.id, ...open.newCommit })));
  const types = [...new Set(commits.filter((commit) => commit.transitionType).map((commit) => `${commit.transitionType} ${short(commit.transitionQualifiers)}`))];
  check("L163/L167: a new tab's first commit is not \"reload\", so it can count as an open", "never reload",
    types.join(", "), commits.length > 0 && commits.every((commit) => commit.transitionType && commit.transitionType !== "reload"),
    { commits, note: `new tab first commit types seen: ${types.join(", ")}.` });

  // L189 does not use openerTabId, whose readability without "tabs" it calls
  // untested. Record whether tabs.onCreated carried it.
  const openers = results.flatMap((result) => result.summary.opens.map((open) => ({ case: result.spec.id, source: result.sourceTabId,
    openerTabId: open.tabsOnCreated?.openerTabId ?? null, pendingUrl: open.tabsOnCreated?.pendingUrl ?? null })));
  const readable = openers.filter((entry) => entry.openerTabId !== null);
  check("info: tabs.onCreated openerTabId without \"tabs\" (L189 does not use it)", "(no DESIGN.md claim; recorded as found)",
    `openerTabId present in ${readable.length}/${openers.length}; equals source in ${readable.filter((entry) => entry.openerTabId === entry.source).length}; missing in ${openers.filter((entry) => entry.openerTabId === null).map((entry) => entry.case).join(",") || "none"}`,
    true, { openers, note: `tabs.onCreated pendingUrl set in ${openers.filter((entry) => entry.pendingUrl).map((entry) => entry.case).join(",") || "none"}.` });

  // Arrival order is what a serialize()d worker sees; details.timeStamp is
  // Chrome's own clock. They need not agree, so record both.
  const all = results.flatMap((result) => result.summary.opens.map((open) => ({ case: result.spec.id, open })));
  const arrivedFirst = all.filter(({ open }) => open.afterNewBeforeNavigate === false).map((entry) => entry.case);
  // The case names are stable; the millisecond values are not, so they go in
  // the extra details only.
  const stamped = all.filter(({ open }) => open.chromeMs.newBeforeNavigate !== null && open.chromeMs.newBeforeNavigate < 0)
    .map(({ case: id, open }) => ({ case: id, newBeforeNavigateMs: open.chromeMs.newBeforeNavigate }));
  check("info: onCreatedNavigationTarget arrives before the new tab's onBeforeNavigate; Chrome's timeStamps", "(no DESIGN.md claim; recorded as found)",
    `arrived before onBeforeNavigate in ${arrivedFirst.length}/${all.length}; timeStamp after onBeforeNavigate in ${stamped.map((entry) => entry.case).join(",") || "none"}`,
    true, { stamped, note: "Order checks use arrival order; details.timeStamp can put the new tab's onBeforeNavigate earlier than onCreatedNavigationTarget, so a worker must not sort by timeStamp." });

  // A tab the browser opens by itself (Target.createTarget, like a typed URL
  // in a new tab) has no source, so L189's edge must not appear for it.
  await ctx.reset();
  const since = Date.now();
  const direct = ctx.urls.page("direct-new-tab");
  await ctx.openTab(direct);
  await ctx.delay(1_000);
  const log = await ctx.readLog({ since });
  const createdEvents = log.filter((entry) => entry.kind === "onCreatedNavigationTarget");
  const commit = log.find((entry) => entry.kind === "onCommitted" && entry.details.url === href(direct) && entry.details.frameId === 0);
  check("L189 (control): a browser-opened tab (Target.createTarget) fires no onCreatedNavigationTarget", "no event",
    `${createdEvents.length} event(s); its commit ${commit ? `${commit.details.transitionType} ${short(commit.details.transitionQualifiers)}` : "none"}`,
    createdEvents.length === 0, { events: log.filter(mainFrame).map((entry) => ctx.brief(entry)) });
  return findings;
}
