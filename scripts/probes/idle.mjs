// Group 4 (DESIGN.md §12.2 step 6.1): idle.setDetectionInterval at 5 minutes.
// §5.1 says the worker sets the idle detection interval to 5 minutes instead
// of the 60-second default and that Chrome's minimum is 15 seconds; §8 says
// idle has no install warning. This probe can only show which values the API
// accepts. Idle and locked are system-wide states driven by the real user's
// input, so it never waits for the 5-minute transition, never locks the
// screen, and never asserts on the state queryState reports.

import path from "node:path";
import { fileURLToPath } from "node:url";

export const name = "idle";
export const title = "idle.setDetectionInterval at 5 minutes";

const IDLE_ONLY = path.join(path.dirname(fileURLToPath(import.meta.url)), "idle-only-extension");
const STATES = ["active", "idle", "locked"];

// Runs call in the worker and says how it ended: a synchronous throw, a
// rejected promise, or a return. probe.attempt folds the first two together.
const outcome = (call) => `(() => {
  try {
    const result = ${call};
    return Promise.resolve(result).then(
      (value) => ({ ok: true, how: result instanceof Promise ? "resolved" : "returned", value: value ?? null }),
      (error) => ({ ok: false, how: "rejected", error: String(error?.message ?? error) }));
  } catch (error) {
    return { ok: false, how: "threw", error: String(error?.message ?? error) };
  }
})()`;

const setDetection = (ctx, seconds) => ctx.inWorker(outcome(`chrome.idle.setDetectionInterval(${JSON.stringify(seconds)})`));
const queryState = (ctx, seconds) => ctx.inWorker(outcome(`chrome.idle.queryState(${JSON.stringify(seconds)})`));
const show = (result) => (result.ok ? `${result.how} ${JSON.stringify(result.value)}` : `${result.how}: ${result.error}`);

export async function run(ctx) {
  const findings = [];
  const check = (claim, expected, observed, matches, note) => findings.push({ claim, expected, observed, matches, ...(note ? { note } : {}) });
  const since = Date.now();

  // Setting the interval: the design value, the stated minimum, and below it.
  // Non-integers and a string are recorded because a settings page may pass one.
  const set = {};
  for (const seconds of [300, 15, 14, 5, 0, -1, 15.5, 14.5, "300"]) set[JSON.stringify(seconds)] = await setDetection(ctx, seconds);
  // Leave the worker at the design value.
  const final = await setDetection(ctx, 300);

  check("idle.setDetectionInterval(300) is accepted", "accepted (§5.1: 5 minutes)", show(set["300"]), set["300"].ok);
  check("setDetectionInterval(15), the stated minimum, is accepted", "accepted (§5.1: minimum 15 s)", show(set["15"]), set["15"].ok);
  check("setDetectionInterval(14) is rejected, not clamped", "rejected (§5.1: minimum 15 s)", show(set["14"]), !set["14"].ok);
  check("setDetectionInterval(5) and (0) are rejected", "rejected (§5.1: minimum 15 s)",
    { 5: show(set["5"]), 0: show(set["0"]), "-1": show(set["-1"]) }, !set["5"].ok && !set["0"].ok && !set["-1"].ok,
    `Below 15 Chrome ${set["5"].how === "threw" ? "throws synchronously" : set["5"].how}; config must validate whole seconds of at least 15 before calling (§5.1 Engagement).`);
  check("record: non-integer and string intervals", "no DESIGN claim; recorded as observed",
    { 15.5: show(set["15.5"]), 14.5: show(set["14.5"]), '"300"': show(set['"300"']) }, true,
    "Recorded only; a settings page should pass whole seconds of at least 15.");
  check("re-setting 300 after other values is accepted", "accepted", show(final), final.ok);

  // queryState has the same 15-second floor; its value is the real user's
  // machine state, so only its shape is checked.
  const query = {};
  for (const seconds of [300, 15, 14, 5]) query[String(seconds)] = await queryState(ctx, seconds);
  check("idle.queryState(300) returns a valid state", STATES.join("|"), show(query["300"]),
    query["300"].ok && STATES.includes(query["300"].value),
    "The value is the real user's system-wide state and is not asserted; headless cannot drive idle or locked.");
  check("queryState shares the 15 s floor", "15 accepted, 14 and 5 rejected",
    { 15: show(query["15"]), 14: show(query["14"]), 5: show(query["5"]) },
    query["15"].ok && !query["14"].ok && !query["5"].ok);

  // There is no getter, so the interval in effect can never be read back.
  const api = await ctx.inWorker(`({
    keys: Object.keys(chrome.idle).sort(),
    getDetectionInterval: typeof chrome.idle.getDetectionInterval,
    getAutoLockDelay: typeof chrome.idle.getAutoLockDelay,
    setDetectionIntervalReturns: String(chrome.idle.setDetectionInterval(300)),
  })`);
  check("the idle API has no interval getter", "no getter (the design assumes none)", api,
    api.getDetectionInterval === "undefined",
    `getAutoLockDelay is ${api.getAutoLockDelay}; the interval must be re-set at every worker start because it cannot be read.`);

  // A fresh worker after a forced stop accepts the call again at top level.
  const stopped = await ctx.stopWorker();
  await ctx.wakeWorker();
  const bootId = await ctx.waitForRestart(stopped.bootId).catch(() => null);
  const again = await setDetection(ctx, 300);
  check("a restarted worker can set 300 again", "new bootId; accepted", { restarted: bootId !== null && bootId !== stopped.bootId, set: show(again) },
    bootId !== null && bootId !== stopped.bootId && again.ok,
    "Whether the earlier setting survived the stop cannot be observed without a getter.");

  // §8: idle has no install warning. An extension that asks only for idle
  // shows exactly the warnings idle brings.
  const { id } = await ctx.loadExtension(IDLE_ONLY);
  const info = (await ctx.extensionsInfo()).find((entry) => entry.id === id);
  const warnings = info?.permissions?.simplePermissions?.map((entry) => entry.message) ?? null;
  check("idle alone gives no install warning", "no warnings (§8)", { id, warnings }, Array.isArray(warnings) && warnings.length === 0,
    "Read from developerPrivate.getExtensionsInfo; the idle-only extension stays loaded for the rest of the browser session.");

  // Any idle.onStateChanged during the run came from the real user's input.
  const changes = await ctx.readLog({ since, kinds: ["idle.onStateChanged"] });
  ctx.log(`idle.onStateChanged during the run: ${changes.length ? changes.map((entry) => entry.details.state).join(", ") : "none"}`);
  return findings;
}
