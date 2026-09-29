// Harness self-check. It proves the pieces every M1.5a probe relies on: the
// recorder's posture, a navigation reaching the log as onCommitted, a forced
// worker stop and restart, and a relaunch on the same profile. Its claims are
// about the harness, not about DESIGN.md.

export const name = "self-check";
export const title = "Probe harness self-check";

const committed = (url) => (entry) => entry.kind === "onCommitted" && entry.details.frameId === 0 && entry.details.url === url;

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, note) => findings.push({ claim, expected, observed, matches, ...(note ? { note } : {}) });

  const permissions = await ctx.inWorker("probe.permissions()");
  check("harness: recorder has no \"tabs\" permission", "no tabs", permissions.permissions,
    !permissions.permissions.includes("tabs") && permissions.permissions.includes("webNavigation"));
  check("harness: recorder has host access to the fixture origin", "http://127.0.0.1/*", permissions.origins,
    permissions.origins.includes("http://127.0.0.1/*"),
    "A static host permission stands in for Brauser's optional exact-origin grant.");
  // §8 says Chrome labels webNavigation "Read your browsing history". The
  // recorder has no "tabs", so that warning can only come from webNavigation.
  const recorder = (await ctx.extensionsInfo()).find((entry) => entry.id === ctx.extensionId);
  const warnings = recorder?.permissions?.simplePermissions?.map((entry) => entry.message) ?? null;
  check("webNavigation without \"tabs\" gives the \"Read your browsing history\" install warning",
    "\"Read your browsing history\" (§8 webNavigation row)", warnings,
    Array.isArray(warnings) && warnings.includes("Read your browsing history"),
    "Read from developerPrivate.getExtensionsInfo for the recorder, whose only warning-bearing API permission is webNavigation.");

  const first = ctx.urls.page("self-check");
  const since = Date.now();
  await ctx.navigate(ctx.homeTargetId, first, { transitionType: "typed" });
  const commit = await ctx.waitForEvent(committed(first), { since }).catch(() => null);
  check("harness: a typed navigation reaches the log as onCommitted", "onCommitted typed", commit && ctx.brief(commit),
    commit?.details.transitionType === "typed");

  const stopped = await ctx.stopWorker();
  const running = await ctx.workerTarget();
  check("harness: stopWorker leaves no recorder worker target", "no worker target", running ? running.url : "none",
    running === null, `Stopped with ${stopped.method}.`);

  const wake = ctx.urls.page("after-stop");
  const wakeSince = Date.now();
  await ctx.navigate(ctx.homeTargetId, wake, { transitionType: "link" });
  const bootId = await ctx.waitForRestart(stopped.bootId).catch(() => null);
  const woke = await ctx.waitForEvent(committed(wake), { since: wakeSince }).catch(() => null);
  check("harness: a navigation restarts the stopped worker and is logged", "new bootId, onCommitted",
    { restarted: bootId !== null && bootId !== stopped.bootId, commit: woke && ctx.brief(woke) },
    bootId !== null && woke?.bootId === bootId);

  const { pageTargets } = await ctx.relaunch();
  const again = ctx.urls.page("after-relaunch");
  const relaunchSince = Date.now();
  await ctx.navigate(ctx.homeTargetId, again, { transitionType: "typed" });
  const relaunched = await ctx.waitForEvent(committed(again), { since: relaunchSince }).catch(() => null);
  check("harness: after relaunch the recorder is back with the same ID and logs", "onCommitted after relaunch",
    { extensionId: ctx.extensionId, pagesBeforeReload: pageTargets.length, commit: relaunched && ctx.brief(relaunched) },
    relaunched !== null);
  return findings;
}
