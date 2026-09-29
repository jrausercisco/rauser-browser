// Group 2 of DESIGN.md §12.2 step 6.1: the transition type and qualifiers
// Chrome reports on a main-frame onCommitted for a link click, a typed URL, a
// bookmark, a form, a reload, and back and forward, and what
// onHistoryStateUpdated reports for the board page's ?selectedIssue= changes.
// Each finding's "expected" is what DESIGN.md §5.1 says (L138, L163, L167,
// L187, L190, L192, L200, L228), quoted where it matters; "observed" is what
// the recorder logged. Findings marked informational test no DESIGN.md claim.
// Where the step 6.1 results corrected DESIGN.md (2026-09-28), the expected
// value is the corrected text, so the probe guards it.
//
// Two tabs:
// - A fresh tab opened on /board by Target.createTarget and never given
//   input, so its document has no user activation, transient or sticky. It
//   pushes KEY-4 with no activation, then orders title and pushState both
//   ways, and finally leaves by a script navigation and comes back, to see
//   why the back/forward cache is not used.
// - The probe tab, which walks a known history: /list, /page?n=link, /board,
//   ?selectedIssue=KEY-1, KEY-2, then KEY-5 pushed by script more than 5 s
//   after the last click (transient activation expired, sticky still set),
//   then KEY-3 replacing KEY-5.
// Each tab keeps one DevTools session attached with Page.enable, so
// Page.frameNavigated and Page.backForwardCacheNotUsed show whether a
// cross-document back came from the back/forward cache.
//
// What the probe cannot reach, stated in the notes too:
// - CDP Page.navigate with transitionType "typed" or "auto_bookmark" is not the
//   omnibox or a bookmark click: it carries no from_address_bar qualifier and
//   triggers no omnibox prerender.
// - history.go() is renderer-initiated. Page.navigateToHistoryEntry is
//   browser-initiated (NavigationController::GoToIndex), but the toolbar Back
//   button goes through GoBack, which also skips entries a page added without
//   user activation (the history manipulation intervention). CDP has no GoBack,
//   so that skipping is not exercised.
// - Page.reload is the browser's reload; location.reload() is the page's.
// - Every fixture response is Cache-Control: no-store, which Chrome has long
//   treated as a reason to keep a page out of the back/forward cache. The
//   bfcache finding records, per step, whether Chrome restored the page anyway
//   and the reasons it gives when it did not.

export const name = "transitions";
export const title = "Transition types on onCommitted and onHistoryStateUpdated";

// Chrome's transient user activation lasts 5 s (kActivationLifespan).
const TRANSIENT_ACTIVATION_MS = 5_000;
const ACTIVATION = "(navigator.userActivation ? { isActive: navigator.userActivation.isActive, hasBeenActive: navigator.userActivation.hasBeenActive } : null)";

const summary = (entry) => entry && {
  kind: entry.kind,
  transitionType: entry.details.transitionType,
  transitionQualifiers: entry.details.transitionQualifiers,
  ...(entry.details.documentLifecycle !== undefined ? { documentLifecycle: entry.details.documentLifecycle } : {}),
};
const typeOf = (entry) => entry ? `${entry.details.transitionType} ${JSON.stringify(entry.details.transitionQualifiers)}` : null;
const isBack = (entry) => Boolean(entry?.details.transitionQualifiers?.includes("forward_back"));

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, extra = {}) => findings.push({ claim, expected, observed, matches, ...extra });
  const info = (claim, observed, extra = {}) => check(claim, "no DESIGN.md claim (informational)", observed, true, { informational: true, ...extra });
  const board = (key) => `${ctx.urls.board}?selectedIssue=${key}`;

  // Main-frame records for one tab, in log order.
  const tabRecords = (tabId, since) => ctx.readLog({ since }).then((log) => log.filter((entry) =>
    (entry.details?.tabId === tabId) && (entry.details.frameId === undefined || entry.details.frameId === 0)));

  // Attaches to a tab with Page.enable and returns its step runner. A step
  // runs one action and returns the main-frame record of kind for url, with
  // the tab's record slice and main-frame Page events for the report.
  const pageEvents = [];
  const unsubscribe = ctx.cdp.onEvent((event) => {
    if (/^Page\.(frameNavigated|backForwardCacheNotUsed)$/.test(event.method)) pageEvents.push(event);
  });
  const attached = [];
  async function driver(targetId, tabId) {
    const page = await ctx.attach(targetId);
    attached.push(page);
    await page.send("Page.enable");
    const waitRecord = (kind, url, since) => ctx.waitForEvent((entry) => entry.kind === kind
      && entry.details.tabId === tabId && entry.details.frameId === 0 && entry.details.url === url, { since }).catch(() => null);
    async function step(kind, url, action) {
      const since = Date.now();
      const cdpSince = pageEvents.length;
      const result = await action();
      const entry = await waitRecord(kind, url, since);
      await ctx.delay(200);
      const records = await tabRecords(tabId, since);
      const cdp = pageEvents.slice(cdpSince)
        .filter((event) => event.sessionId === page.sessionId && event.params?.frame?.parentId === undefined)
        .map((event) => event.method === "Page.frameNavigated"
          ? { method: event.method, type: event.params.type, url: event.params.frame?.url }
          : { method: event.method, notRestoredExplanations: event.params.notRestoredExplanations?.map((reason) => reason.reason) });
      return { entry, since, result, records, events: records.map((record) => ctx.brief(record)), cdp };
    }
    const historyGo = (delta, url) => async () => {
      await page.evaluate(`history.go(${delta})`);
      await page.waitForLoad(url);
    };
    // Browser-initiated, like the toolbar's back and forward but without
    // GoBack's skipping.
    const historyEntry = (delta, url) => async () => {
      const { currentIndex, entries } = await page.send("Page.getNavigationHistory");
      await page.send("Page.navigateToHistoryEntry", { entryId: entries[currentIndex + delta].id });
      await page.waitForLoad(url);
    };
    return { page, step, historyGo, historyEntry };
  }

  // Where the title update for a route lands relative to its
  // onHistoryStateUpdated, from the tab's record slice.
  function titleOrder(records, url, title) {
    const history = records.find((entry) => entry.kind === "onHistoryStateUpdated" && entry.details.url === url);
    const titles = records.filter((entry) => entry.kind === "tabs.onUpdated" && entry.details.change?.title !== undefined);
    const titled = titles.find((entry) => entry.details.change.title === title);
    const urlChange = records.find((entry) => entry.kind === "tabs.onUpdated" && entry.details.change?.url === url);
    return {
      historySeq: history?.seq ?? null,
      titleSeq: titled?.seq ?? null,
      titleTabUrl: titled?.details.tab?.url ?? null,
      // What a worker reading the tab at the URL change would get.
      urlChangeTabTitle: urlChange?.details.tab?.title ?? null,
      titleUpdates: titles.map((entry) => ({ seq: entry.seq, title: entry.details.change.title, tabUrl: entry.details.tab?.url ?? null })),
    };
  }
  const newTitleAfter = (order) => order.historySeq !== null && order.titleSeq !== null && order.titleSeq > order.historySeq;

  try {
    // Opened first, so tabIdFor(board) is unambiguous.
    const probeTarget = await ctx.openTab(ctx.urls.list);
    const tabId = await ctx.tabIdFor(ctx.urls.list);
    const freshTarget = await ctx.openTab(ctx.urls.board);
    const freshTabId = await ctx.tabIdFor(ctx.urls.board);

    // Target.createTarget is how every probe opens a tab, so record what it
    // reports; DESIGN.md makes no claim about it.
    const created = await tabRecords(tabId, 0).then((records) => records.find((entry) => entry.kind === "onCommitted" && entry.details.url === ctx.urls.list));
    info("context: a CDP Target.createTarget tab's first commit", summary(created));

    // The fresh tab: pushState from a document that has never had activation.
    // Target.activateTarget is not user activation. It keeps the board's
    // 50 ms title timer from being throttled into the next step.
    await ctx.activate(freshTarget);
    const fresh = await driver(freshTarget, freshTabId);
    const pushNoActivation = await fresh.step("onHistoryStateUpdated", board("KEY-4"),
      () => fresh.page.evaluate(`(() => { const activation = ${ACTIVATION}; board.select('KEY-4'); return activation; })()`));
    await ctx.waitForEvent((entry) => entry.kind === "tabs.onUpdated" && entry.details.tabId === freshTabId
      && entry.details.change?.title === "[KEY-4] Board issue", { since: pushNoActivation.since }).catch(() => null);

    // Title set before pushState, then after it in the same task. The
    // fixture's own 50 ms timer is checked on the probe tab below.
    const titleBefore = await fresh.step("onHistoryStateUpdated", board("KEY-6"), () => fresh.page.evaluate(
      "document.title = '[KEY-6] Board issue'; history.pushState({ key: 'KEY-6' }, '', '?selectedIssue=KEY-6')"));
    await ctx.delay(300);
    const beforeOrder = titleOrder(await tabRecords(freshTabId, titleBefore.since), board("KEY-6"), "[KEY-6] Board issue");
    const titleSame = await fresh.step("onHistoryStateUpdated", board("KEY-7"), () => fresh.page.evaluate(
      "history.pushState({ key: 'KEY-7' }, '', '?selectedIssue=KEY-7'); document.title = '[KEY-7] Board issue'"));
    await ctx.delay(300);
    const sameOrder = titleOrder(await tabRecords(freshTabId, titleSame.since), board("KEY-7"), "[KEY-7] Board issue");

    // One script navigation away and one history.back(), with no Page.navigate
    // in between, to read Chrome's reasons for not using the bfcache.
    await fresh.page.evaluate(`location.href = ${JSON.stringify(ctx.urls.page("bfcache"))}`);
    await fresh.page.waitForLoad(ctx.urls.page("bfcache"));
    const freshBack = await fresh.step("onCommitted", board("KEY-7"), fresh.historyGo(-1, board("KEY-7")));

    // The probe tab. Foreground, so the board's 50 ms title timer is not held
    // back by background-tab timer throttling.
    await ctx.activate(probeTarget);
    const probe = await driver(probeTarget, tabId);
    const { page, step, historyGo, historyEntry } = probe;

    const link = await step("onCommitted", ctx.urls.page("link"), async () => {
      await page.click("#link");
      await page.waitForLoad(ctx.urls.page("link"));
    });
    check("L187: a left-click on a link commits as \"link\"", "onCommitted link", summary(link.entry),
      link.entry?.details.transitionType === "link", { events: link.events });

    const typed = await step("onCommitted", ctx.urls.board, () => page.navigate(ctx.urls.board, { transitionType: "typed" }));
    check("L187: a typed URL (CDP Page.navigate typed) commits as \"typed\"", "onCommitted typed", summary(typed.entry),
      typed.entry?.details.transitionType === "typed",
      { events: typed.events, note: "\"typed\" is CDP-simulated here: no omnibox, so no from_address_bar qualifier and no omnibox prerender." });

    // Board modal: real input clicks to KEY-1 and KEY-2, with the activation
    // state read just after each click.
    const clickThen = (selector) => async () => {
      await page.click(selector);
      return page.evaluate(ACTIVATION);
    };
    const pushes = {};
    pushes["KEY-1"] = await step("onHistoryStateUpdated", board("KEY-1"), clickThen("#key1"));
    pushes["KEY-2"] = await step("onHistoryStateUpdated", board("KEY-2"), clickThen("#key2"));
    for (const key of ["KEY-1", "KEY-2"]) {
      const { entry, events } = pushes[key];
      check(`L163/L137: a gesture pushState to ?selectedIssue=${key} fires onHistoryStateUpdated`,
        "onHistoryStateUpdated, main frame, the new URL", summary(entry), entry !== null, { events });
    }

    // The board's title timer runs 50 ms after its pushState, so this order
    // is set by the fixture; the fresh tab's findings below are the test.
    const timerOrder = titleOrder(pushes["KEY-1"].records, board("KEY-1"), "[KEY-1] Board issue");
    info("context: a title set 50 ms after pushState (order forced by the fixture's timer)", timerOrder);

    // A script push after the transient activation has expired; the
    // document keeps sticky activation from the clicks.
    const lastClick = pushes["KEY-2"].since;
    await ctx.delay(Math.max(0, lastClick + TRANSIENT_ACTIVATION_MS + 700 - Date.now()));
    pushes["KEY-5"] = await step("onHistoryStateUpdated", board("KEY-5"),
      () => page.evaluate(`(() => { const activation = ${ACTIVATION}; board.select('KEY-5'); return activation; })()`));

    const byActivation = {
      "KEY-1 (click)": { type: typeOf(pushes["KEY-1"].entry), activation: pushes["KEY-1"].result },
      "KEY-2 (click)": { type: typeOf(pushes["KEY-2"].entry), activation: pushes["KEY-2"].result },
      "KEY-5 (script, transient expired)": { type: typeOf(pushes["KEY-5"].entry), activation: pushes["KEY-5"].result },
      "KEY-4 (script, fresh document)": { type: typeOf(pushNoActivation.entry), activation: pushNoActivation.result },
    };
    const types = Object.values(byActivation).map((entry) => entry.type);
    // The comparison only means something if the activation states differ as
    // intended: transient on the clicks, sticky only on KEY-5, none on KEY-4.
    const statesAsIntended = pushes["KEY-1"].result?.isActive === true && pushes["KEY-2"].result?.isActive === true
      && pushes["KEY-5"].result?.isActive === false && pushes["KEY-5"].result?.hasBeenActive === true
      && pushNoActivation.result?.isActive === false && pushNoActivation.result?.hasBeenActive === false;
    check("L200: an SPA route change \"looks the same as clicking through issues in a board modal\"",
      "clicked pushStates, a script pushState after transient activation expired, and one with no activation ever report the same type",
      byActivation, statesAsIntended && types.every((type) => type !== null && type === types[0]),
      { events: [...pushes["KEY-5"].events, ...pushNoActivation.events],
        note: statesAsIntended ? "navigator.userActivation was read in the page at each push." : "The activation states were not as intended, so this finding is not evidence." });

    // Step 6.1 found this title update arriving ahead of the route change;
    // §5.1 (M1 mechanics) now takes the title at the URL change or from a
    // later update, so either order passes as long as one of them has it.
    check("L228/L138: a title set before pushState reaches the worker at the URL change or after onHistoryStateUpdated",
      "the tabs.onUpdated URL change carries the new title, or a title update follows onHistoryStateUpdated (§5.1 M1 mechanics)",
      beforeOrder, beforeOrder.urlChangeTabTitle === "[KEY-6] Board issue" || newTitleAfter(beforeOrder),
      { events: titleBefore.events, note: "When the title update comes first it carries the previous route's URL, so it must not be credited to the tab's previous artifact. "
        + `Title set before pushState: the tab's URL-change update carried title ${JSON.stringify(beforeOrder.urlChangeTabTitle)}.` });
    check("L228/L138: a title set in the same task just after pushState arrives after onHistoryStateUpdated",
      "a tabs.onUpdated title update for the route after its onHistoryStateUpdated", sameOrder, newTitleAfter(sameOrder),
      { events: titleSame.events, note: `Title set just after pushState: the tab's URL-change update carried title ${JSON.stringify(sameOrder.urlChangeTabTitle)}; 50 ms after (fixture timer): ${JSON.stringify(timerOrder.urlChangeTabTitle)}.` });

    const replace = await step("onHistoryStateUpdated", board("KEY-3"), () => page.click("#replace"));
    check("L163: a replaceState to ?selectedIssue=KEY-3 fires onHistoryStateUpdated",
      "onHistoryStateUpdated, main frame, the new URL", summary(replace.entry), replace.entry !== null, { events: replace.events });

    // History is now /list, /page?n=link, /board, KEY-1, KEY-2, KEY-3.
    const sameBack = await step("onHistoryStateUpdated", board("KEY-2"), historyGo(-1, board("KEY-2")));
    check("L163: a same-document back is recognizable as back", "onHistoryStateUpdated with the forward_back qualifier",
      summary(sameBack.entry), isBack(sameBack.entry), { events: sameBack.events });
    const sameForward = await step("onHistoryStateUpdated", board("KEY-3"), historyGo(1, board("KEY-3")));
    check("L163: a same-document forward is recognizable as forward", "onHistoryStateUpdated with the forward_back qualifier",
      summary(sameForward.entry), isBack(sameForward.entry), { events: sameForward.events });

    const crossBack = await step("onCommitted", ctx.urls.page("link"), historyGo(-4, ctx.urls.page("link")));
    check("L163: a cross-document back (history.go) to a link entry is recognizable as back", "onCommitted with the forward_back qualifier",
      summary(crossBack.entry), isBack(crossBack.entry), { events: crossBack.events, cdp: crossBack.cdp });
    const crossForward = await step("onCommitted", ctx.urls.board, historyGo(1, ctx.urls.board));
    check("L163: a cross-document forward (history.go) to a typed entry is recognizable as forward", "onCommitted with the forward_back qualifier",
      summary(crossForward.entry), isBack(crossForward.entry), { events: crossForward.events, cdp: crossForward.cdp });

    const browserBack = await step("onCommitted", ctx.urls.page("link"), historyEntry(-1, ctx.urls.page("link")));
    check("L163: a browser-initiated back (Page.navigateToHistoryEntry) to a link entry is recognizable as back",
      "onCommitted with the forward_back qualifier", summary(browserBack.entry), isBack(browserBack.entry),
      { events: browserBack.events, cdp: browserBack.cdp, note: "Page.navigateToHistoryEntry is GoToIndex, not the toolbar's GoBack, so entries added without activation are not skipped." });
    const browserForward = await step("onCommitted", ctx.urls.board, historyEntry(1, ctx.urls.board));
    check("L163: a browser-initiated forward (Page.navigateToHistoryEntry) to a typed entry is recognizable as forward",
      "onCommitted with the forward_back qualifier", summary(browserForward.entry), isBack(browserForward.entry),
      { events: browserForward.events, cdp: browserForward.cdp });

    // L190 keys the same-tab edge on the navigation being a link or form.
    // Step 6.1 found a back to a link entry reported as "link", so §5.1
    // (Trails) now checks forward_back first; this guards that the qualifier
    // is there to check.
    const backs = { "history.go back": crossBack.entry, "browser back": browserBack.entry };
    const backTypes = Object.fromEntries(Object.entries(backs).map(([label, entry]) => [label, typeOf(entry)]));
    check("L190/L163: a cross-document back keeps the entry's type but carries forward_back, so it is not taken for a link",
      "forward_back on every back, whatever the type (§5.1 Trails: recorded as back or forward, with no same-tab source)",
      backTypes, Object.values(backs).every((entry) => isBack(entry)),
      { note: "Back and forward carry the history entry's original transitionType; only the forward_back qualifier marks them." });

    // no-store has long kept pages out of the bfcache; the frameNavigated
    // type and the reasons Chrome gives are the evidence.
    const cache = { "fresh tab, one history.back() after a script navigation": freshBack.cdp,
      "probe tab, history.go(-4)": crossBack.cdp, "probe tab, history.go(1)": crossForward.cdp,
      "probe tab, navigateToHistoryEntry back": browserBack.cdp, "probe tab, navigateToHistoryEntry forward": browserForward.cdp };
    const fromCache = (events) => events.some((event) => event.type === "BackForwardCacheRestore");
    const restoredSteps = Object.entries(cache).filter(([, events]) => fromCache(events)).map(([label]) => label);
    const reasons = [...new Set(Object.values(cache).flat().flatMap((event) => event.notRestoredExplanations ?? []))];
    info("context: whether cross-document back/forward came from the back/forward cache", { restoredSteps, reasons, cache },
      { note: `Every fixture response is Cache-Control: no-store. Restored from the bfcache anyway: ${restoredSteps.join("; ") || "none"}. Reasons given when not restored: ${reasons.join(", ") || "none"}.` });
    // L228: M1 observes main-frame onCommitted, "using document identity and
    // lifecycle to avoid subframes, redirects, and duplicate SPA records".
    const restoredCommits = Object.fromEntries([["fresh tab history.back()", freshBack], ["browser back", browserBack],
      ["browser forward", browserForward], ["history.go back", crossBack], ["history.go forward", crossForward]]
      .map(([label, result]) => [label, { fromBfcache: fromCache(result.cdp), onCommitted: summary(result.entry) }]));
    info("context: what onCommitted reports for a bfcache restore next to a network back/forward", restoredCommits,
      { events: [...freshBack.events, ...browserBack.events] });

    const reload = await step("onCommitted", ctx.urls.board, async () => {
      await page.send("Page.reload");
      await ctx.delay(100);
      await page.waitForLoad(ctx.urls.board);
    });
    check("L167: a browser reload (Page.reload) commits as \"reload\"", "onCommitted reload", summary(reload.entry),
      reload.entry?.details.transitionType === "reload", { events: reload.events });
    // DESIGN.md names only the user's, discard, and session-restore reloads,
    // so a page's own reload is informational.
    const pageReload = await step("onCommitted", ctx.urls.board, async () => {
      await page.evaluate("location.reload()");
      await ctx.delay(100);
      await page.waitForLoad(ctx.urls.board);
    });
    info("context: what a page's own location.reload() (no activation) commits as", summary(pageReload.entry),
      { events: pageReload.events, note: `A page's location.reload() commits as ${typeOf(pageReload.entry)}, not "reload", so L167's type test does not see it; L163's map rule still counts no open while the tab's entry is unchanged.` });

    await page.navigate(ctx.urls.list, { transitionType: "typed" });
    const form = await step("onCommitted", ctx.urls.page("form"), async () => {
      await page.click("#submit");
      await page.waitForLoad(ctx.urls.page("form"));
    });
    check("L187: a form submission commits as \"form_submit\"", "onCommitted form_submit", summary(form.entry),
      form.entry?.details.transitionType === "form_submit", { events: form.events });

    const bookmark = await step("onCommitted", ctx.urls.page("bookmark"),
      () => page.navigate(ctx.urls.page("bookmark"), { transitionType: "auto_bookmark" }));
    check("L187: a bookmark (CDP Page.navigate auto_bookmark) commits as \"auto_bookmark\"", "onCommitted auto_bookmark",
      summary(bookmark.entry), bookmark.entry?.details.transitionType === "auto_bookmark",
      { events: bookmark.events, note: "\"auto_bookmark\" is CDP-simulated here, not a bookmark-bar click." });

    // The ungranted twin origin: what webNavigation and tabs.onUpdated carry.
    const otherUrl = ctx.urls.other("/page?n=ungranted");
    const other = await step("onCommitted", otherUrl, () => page.navigate(otherUrl, { transitionType: "typed" }));
    const updates = (await ctx.readLog({ since: other.since })).filter((entry) => entry.kind === "tabs.onUpdated" && entry.details.tabId === tabId);
    const updateUrls = updates.map((entry) => ({ tabUrl: entry.details.tab?.url ?? null, changeUrl: entry.details.change?.url ?? null }));
    // Step 6.1 found webNavigation reporting this URL; §5.1 (Trails) now says
    // so and has the worker drop it, so the guard is that tab metadata still
    // withholds it and webNavigation still needs that filter.
    check("L192: webNavigation reports a URL on an ungranted origin; tab metadata does not",
      "onCommitted has the ungranted URL; no tabs.onUpdated URL (§5.1 Trails: the worker drops it before storing)",
      { onCommittedUrl: other.entry?.details.url ?? null, tabsOnUpdated: updateUrls },
      other.entry?.details.url === otherUrl && updateUrls.every((entry) => !entry.tabUrl && !entry.changeUrl),
      { events: other.events, note: "webNavigation carries every URL; only tab metadata is limited to granted origins." });
  } finally {
    unsubscribe();
    for (const page of attached) await page.detach();
  }
  return findings;
}
