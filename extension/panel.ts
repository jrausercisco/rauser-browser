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

// --- Page area and note state (§5.3, §5.7) ---------------------------------
// The panel follows the active tab. A note is identified by the tab's exact
// URL; `noteLoadSeq` lets a superseded load from a since-abandoned tab be
// ignored instead of clobbering a newer one.
let noteUrl: string | null = null;
let noteCreationTitle = "";
let noteRevision: string | null = null;
let noteLoaded = false;
let noteDirty = false;
let noteSaving = false;
let noteLoadSeq = 0;
let saveTimer: ReturnType<typeof setTimeout> | null = null;
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

function setNoteEditable(editable: boolean): void {
  noteBody.disabled = !editable;
}

function hideConflict(): void {
  noteConflict.hidden = true;
  noteUnsaved.value = "";
}

function showConflict(unsavedText: string): void {
  noteUnsaved.value = unsavedText;
  noteConflict.hidden = false;
}

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

async function loadNoteFor(tab: ChromeTab): Promise<void> {
  const url = tab.url!;
  const seq = ++noteLoadSeq;
  noteUrl = url;
  noteCreationTitle = tab.title?.trim() || new URL(url).hostname;
  noteRevision = null;
  noteLoaded = false;
  noteDirty = false;
  setNoteEditable(false);
  noteBody.value = "";
  showNoteStatus("");
  hideConflict();
  try {
    const response = await session.host.call({
      type: "load_note",
      protocol_version: PROTOCOL_VERSION,
      request_id: newRequestId(),
      url,
    }, "note_loaded");
    if (seq !== noteLoadSeq) return; // A newer tab switch has already started.
    noteRevision = response.revision;
    noteBody.value = response.body;
    noteLoaded = true;
    setNoteEditable(true);
    showNoteStatus(response.exists ? "Saved" : "");
  } catch (error) {
    if (seq !== noteLoadSeq) return;
    showNoteStatus(`Could not load this page's note: ${describe(error)}`, true);
  }
}

function scheduleNoteSave(): void {
  noteDirty = true;
  showNoteStatus("");
  if (saveTimer !== null) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void flushNoteSave();
  }, NOTE_SAVE_DEBOUNCE_MS);
}

async function flushNoteSave(): Promise<void> {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  if (!noteDirty || !noteLoaded || noteSaving || noteUrl === null || noteRevision === null) return;
  const url = noteUrl;
  const expectedRevision = noteRevision;
  const body = noteBody.value;
  const title = noteCreationTitle;
  noteSaving = true;
  noteDirty = false;
  showNoteStatus("Saving…");
  try {
    const response = await session.host.callAny({
      type: "save_note",
      protocol_version: PROTOCOL_VERSION,
      request_id: newRequestId(),
      url,
      title,
      body,
      expected_revision: expectedRevision,
    }, ["note_saved", "note_conflict"]);
    if (noteUrl !== url) return; // The active tab moved on while this saved.
    if (response.type === "note_saved") {
      noteRevision = response.revision;
      showNoteStatus("Saved");
    } else {
      // The note changed elsewhere since this panel last loaded it. Nothing
      // was written; show the newer note and let the user copy their text
      // back in rather than losing it (§4.4, §5.3).
      noteRevision = response.revision;
      noteBody.value = response.body;
      showConflict(body);
      showNoteStatus("Could not save; this note changed elsewhere.", true);
    }
  } catch (error) {
    if (noteUrl !== url) return;
    noteDirty = true; // Retry on the next edit, tab change, or close.
    showNoteStatus(`Could not save: ${describe(error)}`, true);
  } finally {
    noteSaving = false;
  }
}

function copyUnsavedText(): void {
  void navigator.clipboard.writeText(noteUnsaved.value).catch((error: unknown) => {
    showNoteStatus(`Could not copy: ${describe(error)}`, true);
  });
}

/** Follows the active tab (§5.7). Flushes any pending save for the page
 * being left before switching the panel to a new one. */
async function refreshActiveTab(): Promise<void> {
  let tab: ChromeTab | undefined;
  try {
    [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  } catch {
    tab = undefined;
  }
  if (!tab || tab.incognito) {
    await flushNoteSave();
    pageTitle.textContent = "No page yet";
    pageUrl.textContent = "";
    pageUnusable.hidden = true;
    noteUnavailable.hidden = true;
    noteGrantOrigin.hidden = true;
    noteUrl = null;
    noteLoaded = false;
    setNoteEditable(false);
    noteBody.value = "";
    return;
  }

  if (tab.url === undefined) {
    // §5.3: without an origin grant or a live activeTab, the panel cannot
    // see this tab's address at all.
    await flushNoteSave();
    pageTitle.textContent = tab.title?.trim() || "This page";
    pageUrl.textContent = "";
    pageUnusable.hidden = true;
    noteUnavailable.hidden = false;
    noteGrantOrigin.hidden = true;
    noteUrl = null;
    noteLoaded = false;
    setNoteEditable(false);
    noteBody.value = "";
    return;
  }

  let url: URL;
  try {
    url = new URL(tab.url);
  } catch {
    url = new URL("about:blank");
  }
  if (!isHttpUrl(url)) {
    await flushNoteSave();
    pageTitle.textContent = tab.title?.trim() || tab.url;
    pageUrl.textContent = tab.url;
    pageUnusable.hidden = false;
    noteUnavailable.hidden = true;
    noteGrantOrigin.hidden = true;
    noteUrl = null;
    noteLoaded = false;
    setNoteEditable(false);
    noteBody.value = "";
    return;
  }

  pageTitle.textContent = tab.title?.trim() || url.hostname;
  pageUrl.textContent = tab.url;
  pageUnusable.hidden = true;
  noteUnavailable.hidden = true;
  void refreshGrantButton(url.origin);

  if (tab.url !== noteUrl) {
    await flushNoteSave();
    await loadNoteFor(tab);
  }
}

settingsButton.addEventListener("click", openSettings);
setupSettingsButton.addEventListener("click", openSettings);
pauseButton.addEventListener("click", () => void pauseCapture());
replayButton.addEventListener("click", () => void replayVisits());
discardButton.addEventListener("click", () => void discardPendingVisits());
dismissButton.addEventListener("click", () => void dismissNotices());
noteBody.addEventListener("input", scheduleNoteSave);
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
  void flushNoteSave();
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
  await refreshActiveTab();
  if (initialized) await replayVisits();
})();
