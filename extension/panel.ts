import type { VisitOutcome } from "../protocol/ts/generated.js";
import {
  MAX_QUEUED_VISITS,
  exactOriginPattern,
  isHttpUrl,
  type QueuedVisit,
  type WorkerStatus,
} from "./model.js";
import { ConfigSession, describe, element, setupProblem, worker } from "./settings.js";
import { PROTOCOL_VERSION, newRequestId } from "./native.js";
import { APP_NAME } from "./brand.js";
import { NoteEditor, type UnsavedText } from "./note-editor.js";

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
const captureStrip = element<HTMLDetailsElement>("capture-strip");
const captureStripSummary = element<HTMLElement>("capture-strip-summary");

const pageTitle = element<HTMLParagraphElement>("page-title");
const pageUrl = element<HTMLParagraphElement>("page-url");
const pageUnusable = element<HTMLParagraphElement>("page-unusable");
const noteStatus = element<HTMLSpanElement>("note-status");
const noteUnavailable = element<HTMLParagraphElement>("note-unavailable");
const noteBody = element<HTMLTextAreaElement>("note-body");
const noteGrantOrigin = element<HTMLButtonElement>("note-grant-origin");
const noteConflict = element<HTMLDivElement>("note-conflict");
const noteConflictMessage = noteConflict.querySelector("p")!;
const noteUnsaved = element<HTMLTextAreaElement>("note-unsaved");
const copyUnsavedButton = element<HTMLButtonElement>("copy-unsaved");

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
const NOTE_SAVE_DEBOUNCE_MS = 1_000;

// --- Page area (§5.3, §5.7) --------------------------------------------------
// The panel follows the active tab. `tabRefreshSeq` lets a refresh whose tab
// query was overtaken by a newer one be ignored; the note itself is owned by
// `notes` (note-editor.ts), which serializes every load and save.
let tabRefreshSeq = 0;
let grantOrigin: string | null = null;

function show(message: string, warning = false): void {
  status.textContent = message;
  status.classList.toggle("warning", warning);
}

function showNoteStatus(message: string, warning = false): void {
  noteStatus.textContent = message;
  noteStatus.classList.toggle("error", warning);
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
}

function renderSetupWarning(): void {
  const problem = setupProblem(session.config, session.configIssue);
  setupWarning.hidden = problem === null;
  setupReason.textContent = problem ?? "";
}

function renderQueue(state: WorkerStatus): void {
  queueSummary.textContent = `${state.queued} pending visit${state.queued === 1 ? "" : "s"}.`;
  const captureOn = session.config?.capture_enabled === true && state.pause_pending !== true;
  captureStripSummary.textContent =
    `${captureOn ? "●" : "○"} Capture ${captureOn ? "on" : "off"} · ${state.queued} pending`;
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
  // Open the strip on its own once there is something worth surfacing.
  if (messages.length && !captureStrip.open) captureStrip.open = true;
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
      show(`Chrome is still running an older ${APP_NAME} build. Click Reload on ${APP_NAME} in chrome://extensions, then reopen this panel.`, true);
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
        // A note that could not load before (say, no folder yet) may load now.
        void notes.retry();
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

// --- Page area and note (§5.3, §5.7) ---------------------------------------

function showUnsavedText(entries: readonly UnsavedText[]): void {
  const only = entries.length === 1 ? entries[0]! : null;
  noteConflictMessage.textContent = only
    ? `${only.reason} Copy your text for ${only.url} back in below:`
    : "Some note text could not be saved. Copy it back in below:";
  noteUnsaved.value = only
    ? only.text
    : entries.map((entry) => `${entry.url}\n${entry.reason}\n\n${entry.text}`).join("\n\n");
  noteConflict.hidden = entries.length === 0;
}

const notes = new NoteEditor({
  readBody: () => noteBody.value,
  writeBody: (body) => { noteBody.value = body; },
  setEditable: (editable) => { noteBody.disabled = !editable; },
  setStatus: showNoteStatus,
  showUnsaved: showUnsavedText,
}, {
  load: (url) => session.host.call({
    type: "load_note",
    protocol_version: PROTOCOL_VERSION,
    request_id: newRequestId(),
    url,
  }, "note_loaded"),
  save: (request) => session.host.callAny({
    type: "save_note",
    protocol_version: PROTOCOL_VERSION,
    request_id: newRequestId(),
    ...request,
  }, ["note_saved", "note_conflict"]),
}, describe, NOTE_SAVE_DEBOUNCE_MS);

async function refreshGrantButton(origin: string): Promise<void> {
  try {
    const granted = await chrome.permissions.contains({ origins: [exactOriginPattern(origin)] });
    grantOrigin = granted ? null : origin;
    noteGrantOrigin.hidden = granted;
  } catch {
    grantOrigin = null;
    noteGrantOrigin.hidden = true;
  }
}

function requestNoteOrigin(): void {
  const origin = grantOrigin;
  if (!origin) return;
  // This call must run in the click stack to keep Chrome's user gesture.
  const granted = chrome.permissions.request({ origins: [exactOriginPattern(origin)] });
  void granted.then((ok) => {
    if (ok) {
      grantOrigin = null;
      noteGrantOrigin.hidden = true;
    } else {
      showNoteStatus("Chrome access was declined.", true);
    }
  }).catch((error: unknown) => showNoteStatus(describe(error), true));
}

function copyUnsavedText(): void {
  void navigator.clipboard.writeText(noteUnsaved.value).catch((error: unknown) => {
    showNoteStatus(`Could not copy: ${describe(error)}`, true);
  });
}

/** Follows the active tab (§5.7). The header and the note editor are pointed
 * at the tab this call found in the same step, so they cannot disagree;
 * `notes` saves the page being left before loading the new one. */
async function refreshActiveTab(): Promise<void> {
  const seq = ++tabRefreshSeq;
  let tab: ChromeTab | undefined;
  try {
    [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  } catch {
    tab = undefined;
  }
  if (seq !== tabRefreshSeq) return; // A newer refresh has the current tab.

  if (!tab || tab.incognito) {
    pageTitle.textContent = "No page yet";
    pageUrl.textContent = "";
    pageUnusable.hidden = true;
    noteUnavailable.hidden = true;
    noteGrantOrigin.hidden = true;
    await notes.showPage(null);
    return;
  }

  if (tab.url === undefined) {
    // §5.3: without an origin grant or a live activeTab, the panel cannot
    // see this tab's address at all.
    pageTitle.textContent = tab.title?.trim() || "This page";
    pageUrl.textContent = "";
    pageUnusable.hidden = true;
    noteUnavailable.hidden = false;
    noteGrantOrigin.hidden = true;
    await notes.showPage(null);
    return;
  }

  let url: URL;
  try {
    url = new URL(tab.url);
  } catch {
    url = new URL("about:blank");
  }
  if (!isHttpUrl(url)) {
    pageTitle.textContent = tab.title?.trim() || tab.url;
    pageUrl.textContent = tab.url;
    pageUnusable.hidden = false;
    noteUnavailable.hidden = true;
    noteGrantOrigin.hidden = true;
    await notes.showPage(null);
    return;
  }

  pageTitle.textContent = tab.title?.trim() || url.hostname;
  pageUrl.textContent = tab.url;
  pageUnusable.hidden = true;
  noteUnavailable.hidden = true;
  void refreshGrantButton(url.origin);
  await notes.showPage({ url: tab.url, title: tab.title?.trim() || url.hostname });
}

settingsButton.addEventListener("click", openSettings);
setupSettingsButton.addEventListener("click", openSettings);
pauseButton.addEventListener("click", () => void pauseCapture());
replayButton.addEventListener("click", () => void replayVisits());
discardButton.addEventListener("click", () => void discardPendingVisits());
dismissButton.addEventListener("click", () => void dismissNotices());
noteBody.addEventListener("input", () => notes.edited());
noteGrantOrigin.addEventListener("click", requestNoteOrigin);
copyUnsavedButton.addEventListener("click", copyUnsavedText);
session.watchPolicy(scheduleReload);
chrome.tabs.onActivated.addListener(() => void refreshActiveTab());
chrome.tabs.onUpdated.addListener((_tabId, changeInfo) => {
  if (changeInfo.title !== undefined || changeInfo.url !== undefined || changeInfo.status === "complete") {
    void refreshActiveTab();
  }
});
const pollTimer = setInterval(() => void panelTick(), RETRY_POLL_MS);
window.addEventListener("pagehide", () => {
  closed = true;
  clearInterval(pollTimer);
  if (retryTimer !== null) clearTimeout(retryTimer);
  if (reloadTimer !== null) clearTimeout(reloadTimer);
  // Best effort: MV3 gives no guarantee this completes before teardown.
  void notes.flush();
  session.host.disconnect();
});

void (async () => {
  busy = true;
  updateControls();
  let initialized = false;
  let reachedHost = false;
  try {
    const hello = await session.hello();
    reachedHost = true;
    await reloadConfig();
    if (hello.config_issue) show(hello.config_issue, true);
    initialized = true;
  } catch (error) {
    // Only a failed hello means the host is missing; later failures come from
    // a host that answered, so reinstalling it would not help.
    show(reachedHost
      ? `Could not read the host settings: ${describe(error)}. Open settings to check the configuration, then reopen the panel.`
      : `Native host unavailable: ${describe(error)}. Install or register the development host, then reopen the panel.`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch(() => undefined);
    updateControls();
  }
  await refreshActiveTab();
  if (initialized) await replayVisits();
})();
