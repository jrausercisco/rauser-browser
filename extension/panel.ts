import type { VisitOutcome } from "../protocol/ts/generated.js";
import {
  MAX_QUEUED_VISITS,
  exactOriginPattern,
  isHttpUrl,
  matchingSite,
  type QueuedVisit,
  type WorkerStatus,
} from "./model.js";
import { ConfigSession, describe, element, leaseFor, setupProblem, worker } from "./settings.js";
import { PROTOCOL_VERSION, newRequestId } from "./native.js";

const status = element<HTMLDivElement>("status");
const settingsButton = element<HTMLButtonElement>("open-settings");
const setupWarning = element<HTMLDivElement>("setup-warning");
const setupReason = element<HTMLSpanElement>("setup-reason");
const setupSettingsButton = element<HTMLButtonElement>("setup-open-settings");
const pauseButton = element<HTMLButtonElement>("pause-capture");
const queueSummary = element<HTMLParagraphElement>("queue-summary");
const queueWarning = element<HTMLParagraphElement>("queue-warning");
const dismissButton = element<HTMLButtonElement>("dismiss-notices");
const replayButton = element<HTMLButtonElement>("replay");
const discardButton = element<HTMLButtonElement>("discard-pending");
const noteTitle = element<HTMLInputElement>("note-title");
const noteBody = element<HTMLTextAreaElement>("note-body");
const noteButton = element<HTMLButtonElement>("create-note");
const noteResult = element<HTMLParagraphElement>("note-result");

const session = new ConfigSession(render);
let busy = false;
let replaying = false;
let renewing = false;
let tickRunning = false;
let closed = false;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
let reloadTimer: ReturnType<typeof setTimeout> | null = null;
const RETRY_POLL_MS = 30_000;
const LEASE_RENEW_WINDOW_MS = 60 * 60_000;

function show(message: string, warning = false): void {
  status.textContent = message;
  status.classList.toggle("warning", warning);
}

function changing(): boolean {
  return busy || replaying || renewing || tickRunning;
}

function updateControls(): void {
  const connected = session.connected;
  const config = session.config;
  const state = session.status;
  pauseButton.disabled = !connected || changing() || config?.capture_enabled !== true;
  replayButton.disabled = !connected || changing() || config?.capture_enabled !== true ||
    state?.pause_pending === true || (state?.queued ?? 0) === 0;
  discardButton.disabled = changing() || (state?.queued ?? 0) === 0;
  dismissButton.disabled = changing() || !state || (!state.last_error &&
    !state.overflow_count && !state.rejected_count);
  noteButton.disabled = !connected || changing() || !config?.storage ||
    config?.capture_enabled !== true || state?.pause_pending === true;
}

function renderSetupWarning(): void {
  const problem = setupProblem(session.config, session.configIssue);
  setupWarning.hidden = problem === null;
  setupReason.textContent = problem ?? "";
}

function renderQueue(state: WorkerStatus): void {
  queueSummary.textContent = `${state.queued} pending visit${state.queued === 1 ? "" : "s"}.`;
  const messages: string[] = [];
  if (state.overflow_count) messages.push(`${state.overflow_count} newer visits could not be buffered.`);
  if (state.rejected_count) messages.push(`${state.rejected_count} visits were rejected by host policy.`);
  if (state.last_error) messages.push(state.last_error);
  if (state.retry_error) messages.push(`Retrying: ${state.retry_error}`);
  if (state.pause_pending && session.config?.capture_enabled) {
    messages.push("A local pause is active until the host confirms capture is off.");
  }
  if (state.locally_removed_sites.length) {
    messages.push("Some sites are locally off until their removal is saved in the host.");
  }
  if (!state.navigation_ready && session.config?.capture_enabled) {
    messages.push("Chrome navigation listener is unavailable; visit buffering has not started.");
  }
  queueWarning.textContent = messages.join(" ");
}

function render(): void {
  renderSetupWarning();
  if (session.status) {
    renderQueue(session.status);
    scheduleReplay(session.status);
  }
  updateControls();
}

function scheduleReplay(state: WorkerStatus): void {
  if (retryTimer !== null) clearTimeout(retryTimer);
  retryTimer = null;
  if (closed || changing() || !session.config?.capture_enabled ||
      state.pause_pending || state.next_retry_at === null) return;
  const delay = Math.max(0, state.next_retry_at - Date.now());
  retryTimer = setTimeout(() => {
    retryTimer = null;
    void replayVisits();
  }, delay);
}

function openSettings(): void {
  void chrome.runtime.openOptionsPage().catch((error: unknown) => {
    // Chrome reads an unpacked extension's manifest only when it loads the
    // extension, but serves rebuilt pages from disk. A panel from a newer build
    // can therefore run against a manifest with no settings page.
    if (!chrome.runtime.getManifest().options_ui) {
      show("Chrome is still running an older Rauser build. Click Reload on Rauser in chrome://extensions, then reopen this panel.", true);
    } else {
      show(`Could not open settings: ${describe(error)}`, true);
    }
  });
}

async function reloadConfig(): Promise<void> {
  const response = await session.reload();
  if (response.config_issue) show(response.config_issue, true);
  else show(session.config!.capture_enabled
    ? "Capture is enabled for the listed sites."
    : "Capture is off.");
  if (session.status?.pause_pending && session.config?.capture_enabled) {
    show("Capture is paused locally. Use Pause capture to finish saving this setting in the host.", true);
  }
}

// The settings page saved a different revision. Wait for this panel's own
// work to finish, then read the host again.
function scheduleReload(): void {
  if (reloadTimer !== null || closed) return;
  reloadTimer = setTimeout(() => {
    reloadTimer = null;
    if (changing()) {
      scheduleReload();
      return;
    }
    busy = true;
    updateControls();
    void reloadConfig()
      .catch((error: unknown) => show(`Could not refresh settings: ${describe(error)}`, true))
      .finally(() => {
        busy = false;
        if (session.status) scheduleReplay(session.status);
        updateControls();
      });
  }, 250);
}

async function replayVisits(): Promise<void> {
  if (closed || changing() || !session.config?.capture_enabled) return;
  replaying = true;
  updateControls();
  try {
    for (let processed = 0; processed < MAX_QUEUED_VISITS; processed += 1) {
      // Re-read eligibility before each send so a site removal or Chrome grant
      // revocation cannot keep draining a stale pending snapshot.
      const [item] = await worker<QueuedVisit[]>({ kind: "get_pending" });
      if (!item) break;
      let outcome: VisitOutcome;
      let reason: string | null;
      try {
        const response = await session.host.call({
          type: "record_visit",
          protocol_version: PROTOCOL_VERSION,
          request_id: newRequestId(),
          event: item.event,
        }, "visit_recorded");
        if (response.event_id !== item.event.event_id) {
          throw new Error("Host acknowledged a different visit ID");
        }
        outcome = response.outcome;
        reason = response.reason;
      } catch (error) {
        outcome = "retryable";
        reason = describe(error);
      }
      await worker<WorkerStatus>({
        kind: "ack_visit", event_id: item.event.event_id, outcome, reason,
      });
      if (outcome === "retryable") break;
    }
  } catch (error) {
    show(`Visit replay stopped: ${describe(error)}`, true);
  } finally {
    replaying = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function renewHostPolicy(): Promise<void> {
  if (closed || busy || replaying || renewing || !session.config?.capture_enabled) return;
  renewing = true;
  updateControls();
  try {
    const response = await session.reload();
    if (response.config_issue) {
      show(response.config_issue, true);
    } else if (!session.config!.capture_enabled) {
      show("Capture is off in the host configuration.");
    }
  } catch (error) {
    show(`Cannot renew the host policy lease: ${describe(error)}. Capture stops when the current lease expires.`, true);
  } finally {
    renewing = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function panelTick(): Promise<void> {
  if (closed || changing() || !session.config) return;
  tickRunning = true;
  updateControls();
  try {
    const state = await session.refreshStatus();
    if (state.revoked_origins.length) await session.reconcileRevocations();
    if (state.policy_expires_at !== null &&
        state.policy_expires_at - Date.now() <= LEASE_RENEW_WINDOW_MS) {
      await renewHostPolicy();
    }
  } catch (error) {
    show(`Panel refresh failed: ${describe(error)}`, true);
  } finally {
    tickRunning = false;
    if (session.status) scheduleReplay(session.status);
    updateControls();
  }
}

async function pauseCapture(): Promise<void> {
  if (!session.config) return;
  busy = true;
  updateControls();
  try {
    await session.pauseCapture();
    show("Capture paused. Pending visits cleared. Enable a site in settings to resume.");
  } catch (error) {
    show(`Could not pause capture: ${describe(error)}`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function discardPendingVisits(): Promise<void> {
  if (changing()) return;
  busy = true;
  updateControls();
  try {
    const eventIds = await worker<string[]>({ kind: "get_pending_ids" });
    if (!eventIds.length) return;
    const count = eventIds.length;
    if (!window.confirm(`Discard ${count} pending visit${count === 1 ? "" : "s"}? This cannot be undone.`)) {
      return;
    }
    await worker<WorkerStatus>({ kind: "discard_pending", event_ids: eventIds });
    show("Selected pending visits discarded.");
  } catch (error) {
    show(`Could not discard pending visits: ${describe(error)}`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function dismissNotices(): Promise<void> {
  if (changing()) return;
  busy = true;
  updateControls();
  try {
    await worker<WorkerStatus>({ kind: "clear_notices" });
  } catch (error) {
    show(`Could not dismiss notices: ${describe(error)}`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function createPageNote(): Promise<void> {
  const config = session.config;
  if (!config?.storage || !config.capture_enabled || !session.revision) return;
  busy = true;
  updateControls();
  noteResult.textContent = "";
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab || tab.incognito || !tab.url) throw new Error("Open a permitted page in a normal Chrome window");
    const url = new URL(tab.url);
    if (!isHttpUrl(url) || url.username || url.password) throw new Error("This page is not an HTTP(S) site");
    const lease = leaseFor(config, session.revision);
    if (!matchingSite(lease, url.href)) throw new Error("Enable this site in settings before creating a page note");
    const allowed = await chrome.permissions.contains({
      permissions: ["webNavigation"], origins: [exactOriginPattern(url.origin)],
    });
    if (!allowed) throw new Error("Chrome access for this site was removed");
    const response = await session.host.call({
      type: "create_page_note",
      protocol_version: PROTOCOL_VERSION,
      request_id: newRequestId(),
      url: url.href,
      title: noteTitle.value.trim() || tab.title || url.hostname,
      body: noteBody.value,
    }, "page_note_result");
    const label = {
      created: "Page note created",
      already_present: "Page note already exists",
      conflict: "Page note needs review",
      created_with_warning: "Page note created with a warning",
    }[response.outcome];
    noteResult.textContent = [label, response.relative_path, response.message]
      .filter((part) => part !== null && part !== "").join(" · ");
  } catch (error) {
    noteResult.textContent = describe(error);
  } finally {
    busy = false;
    updateControls();
  }
}

settingsButton.addEventListener("click", openSettings);
setupSettingsButton.addEventListener("click", openSettings);
pauseButton.addEventListener("click", () => void pauseCapture());
replayButton.addEventListener("click", () => void replayVisits());
discardButton.addEventListener("click", () => void discardPendingVisits());
dismissButton.addEventListener("click", () => void dismissNotices());
noteButton.addEventListener("click", () => void createPageNote());
session.watchPolicy(scheduleReload);
const pollTimer = setInterval(() => void panelTick(), RETRY_POLL_MS);
window.addEventListener("pagehide", () => {
  closed = true;
  clearInterval(pollTimer);
  if (retryTimer !== null) clearTimeout(retryTimer);
  if (reloadTimer !== null) clearTimeout(reloadTimer);
  session.host.disconnect();
});

void (async () => {
  busy = true;
  updateControls();
  let initialized = false;
  try {
    const hello = await session.hello();
    await reloadConfig();
    if (hello.config_issue) show(hello.config_issue, true);
    initialized = true;
  } catch (error) {
    show(`Native host unavailable: ${describe(error)}. Install or register the development host, then reopen the panel.`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch(() => undefined);
    updateControls();
  }
  if (initialized) await replayVisits();
})();
