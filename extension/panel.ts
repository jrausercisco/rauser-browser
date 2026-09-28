import type {
  ConfigSnapshot,
  SiteConfig,
  StorageConfig,
  VisitOutcome,
} from "../protocol/ts/generated.js";
import {
  MAX_QUEUED_VISITS,
  POLICY_LEASE_MS,
  exactOriginPattern,
  isHttpUrl,
  matchingSite,
  type PolicyLease,
  type QueuedVisit,
  type WorkerReply,
  type WorkerRequest,
  type WorkerStatus,
} from "./model.js";
import { HostClient, HostError, PROTOCOL_VERSION, newRequestId } from "./native.js";

const status = element<HTMLDivElement>("status");
const folderPath = element<HTMLOutputElement>("folder-path");
const chooseFolderButton = element<HTMLButtonElement>("choose-folder");
const siteUrl = element<HTMLInputElement>("site-url");
const sitePath = element<HTMLInputElement>("site-path");
const enableButton = element<HTMLButtonElement>("enable-site");
const pauseButton = element<HTMLButtonElement>("pause-capture");
const sitesList = element<HTMLUListElement>("sites-list");
const queueSummary = element<HTMLParagraphElement>("queue-summary");
const queueWarning = element<HTMLParagraphElement>("queue-warning");
const replayButton = element<HTMLButtonElement>("replay");
const noteTitle = element<HTMLInputElement>("note-title");
const noteBody = element<HTMLTextAreaElement>("note-body");
const noteButton = element<HTMLButtonElement>("create-note");
const noteResult = element<HTMLParagraphElement>("note-result");

const host = new HostClient();
let currentConfig: ConfigSnapshot | null = null;
let revision: string | null = null;
let picker: { path: string; token: string } | null = null;
let busy = false;
let replaying = false;
let renewing = false;
let tickRunning = false;
let closed = false;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
let lastWorkerStatus: WorkerStatus | null = null;
const RETRY_POLL_MS = 30_000;
const LEASE_RENEW_WINDOW_MS = 60 * 60_000;
let preflightVersion = 0;
let preflight: {
  origin: string;
  pattern: string;
  apiGranted: boolean;
  originGranted: boolean;
} | null = null;

function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) throw new Error(`Missing panel element: ${id}`);
  return found as T;
}

function describe(error: unknown): string {
  if (error instanceof HostError && error.code === "cancelled") {
    return "Canceled. No settings were changed.";
  }
  return error instanceof Error ? error.message : String(error);
}

function show(message: string, warning = false): void {
  status.textContent = message;
  status.classList.toggle("warning", warning);
}

function updateControls(): void {
  const connected = currentConfig !== null && revision !== null;
  const changing = busy || replaying || renewing || tickRunning;
  chooseFolderButton.disabled = !connected || changing;
  enableButton.disabled = !connected || changing || preflight === null ||
    (!picker && !currentConfig?.storage);
  pauseButton.disabled = !connected || changing || currentConfig?.capture_enabled !== true;
  replayButton.disabled = !connected || changing || currentConfig?.capture_enabled !== true ||
    lastWorkerStatus?.pause_pending === true;
  noteButton.disabled = !connected || changing || !currentConfig?.storage ||
    currentConfig?.capture_enabled !== true || lastWorkerStatus?.pause_pending === true;
  for (const button of sitesList.querySelectorAll("button")) {
    button.disabled = !connected || changing;
  }
}

function sameSite(left: SiteConfig, right: SiteConfig): boolean {
  return left.origin === right.origin && left.path_prefix === right.path_prefix;
}

function renderConfig(): void {
  const config = currentConfig;
  folderPath.textContent = picker?.path ?? config?.storage?.root ?? "None selected";
  sitesList.replaceChildren();
  if (!config?.sites.length) {
    const item = document.createElement("li");
    item.textContent = "None";
    sitesList.append(item);
  } else {
    for (const site of config.sites) {
      const item = document.createElement("li");
      const label = document.createElement("span");
      label.textContent = `${site.origin}${site.path_prefix}`;
      item.append(label);
      if (lastWorkerStatus?.locally_removed_sites.some((entry) => sameSite(entry, site))) {
        const local = document.createElement("small");
        local.textContent = " Locally off; host update pending.";
        item.append(local);
      }
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "secondary";
      remove.textContent = "Remove";
      remove.setAttribute("aria-label", `Remove ${site.origin}${site.path_prefix}`);
      remove.addEventListener("click", () => void removeSite(site));
      item.append(remove);
      sitesList.append(item);
    }
  }
  updateControls();
}

function parseSite(): { site: SiteConfig; pattern: string } {
  const url = new URL(siteUrl.value.trim());
  if (!isHttpUrl(url) || url.username || url.password) {
    throw new Error("Enter an HTTP(S) site URL without credentials");
  }
  if (url.pathname !== "/" || url.search || url.hash) {
    throw new Error("Enter only the site origin here; use Allowed path prefix to narrow capture");
  }
  const path = sitePath.value.trim();
  if (!path.startsWith("/") || path.includes("?") || path.includes("#") ||
      path.includes("\\") || path.split("/").some((part) => part === "." || part === "..")) {
    throw new Error("Path prefix must start with / and contain no query, fragment, or traversal");
  }
  const site = { origin: url.origin, path_prefix: path };
  return { site, pattern: exactOriginPattern(site.origin) };
}

async function refreshPreflight(): Promise<void> {
  const version = ++preflightVersion;
  preflight = null;
  updateControls();
  let parsed: ReturnType<typeof parseSite>;
  try {
    parsed = parseSite();
  } catch {
    return;
  }
  try {
    const [apiGranted, originGranted] = await Promise.all([
      chrome.permissions.contains({ permissions: ["webNavigation"] }),
      chrome.permissions.contains({ origins: [parsed.pattern] }),
    ]);
    if (version !== preflightVersion) return;
    preflight = {
      origin: parsed.site.origin,
      pattern: parsed.pattern,
      apiGranted,
      originGranted,
    };
    updateControls();
  } catch (error) {
    if (version === preflightVersion) show(`Cannot inspect Chrome permissions: ${describe(error)}`, true);
  }
}

async function worker<T>(request: WorkerRequest): Promise<T> {
  const reply = await chrome.runtime.sendMessage<WorkerReply<T>>(request);
  if (!reply || !reply.ok || reply.value === null) {
    throw new Error(reply?.error ?? "Extension worker did not respond");
  }
  return reply.value;
}

function leaseFor(config: ConfigSnapshot, currentRevision: string): PolicyLease {
  return {
    revision: currentRevision,
    expires_at: Date.now() + POLICY_LEASE_MS,
    capture_enabled: config.capture_enabled,
    sites: config.sites,
  };
}

async function installHostPolicy(resumeAfterConfirmation = false): Promise<void> {
  if (!currentConfig || !revision) return;
  if (!currentConfig.storage && currentConfig.capture_enabled) {
    await worker<WorkerStatus>({ kind: "clear_policy" });
    return;
  }
  await worker<WorkerStatus>({
    kind: "install_policy",
    lease: leaseFor(currentConfig, revision),
    // A disabled config returned by the host proves that a pending local pause
    // reached the host and can now be cleared safely.
    resume_after_confirmation: resumeAfterConfirmation || !currentConfig.capture_enabled,
  });
}

async function refreshQueue(): Promise<WorkerStatus> {
  const state = await worker<WorkerStatus>({ kind: "get_status" });
  lastWorkerStatus = state;
  queueSummary.textContent = `${state.queued} pending visit${state.queued === 1 ? "" : "s"}.`;
  const messages: string[] = [];
  if (state.overflow_count) messages.push(`${state.overflow_count} newer visits could not be buffered.`);
  if (state.rejected_count) messages.push(`${state.rejected_count} visits were rejected by host policy.`);
  if (state.last_error) messages.push(state.last_error);
  if (state.pause_pending && currentConfig?.capture_enabled) {
    messages.push("A local pause is active until the host confirms capture is off.");
  }
  if (state.locally_removed_sites.length) {
    messages.push("Some sites are locally off until their removal is saved in the host.");
  }
  if (!state.navigation_ready && currentConfig?.capture_enabled) {
    messages.push("Chrome navigation listener is unavailable; visit buffering has not started.");
  }
  queueWarning.textContent = messages.join(" ");
  renderConfig();
  replayButton.disabled = !currentConfig?.capture_enabled || state.queued === 0 ||
    state.pause_pending || busy || replaying || renewing || tickRunning;
  scheduleReplay(state);
  return state;
}

function scheduleReplay(state: WorkerStatus): void {
  if (retryTimer !== null) clearTimeout(retryTimer);
  retryTimer = null;
  if (closed || busy || replaying || renewing || tickRunning || !currentConfig?.capture_enabled ||
      state.pause_pending || state.next_retry_at === null) return;
  const delay = Math.max(0, state.next_retry_at - Date.now());
  retryTimer = setTimeout(() => {
    retryTimer = null;
    void replayVisits();
  }, delay);
}

async function reloadConfig(): Promise<void> {
  const response = await host.call({
    type: "get_config", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
  }, "config_result");
  currentConfig = response.config;
  revision = response.revision;
  renderConfig();
  if (response.config_issue) {
    await worker<WorkerStatus>({ kind: "clear_policy" });
    show(response.config_issue, true);
  } else {
    await installHostPolicy();
    show(currentConfig.capture_enabled
      ? "Capture is enabled for the listed sites."
      : "Capture is off. Choose a folder and enable a site to begin.");
  }
  await reconcileRevocations();
  const state = await refreshQueue();
  if (state.pause_pending && currentConfig.capture_enabled) {
    show("Capture is paused locally. Use Pause capture to finish saving this setting in the host.", true);
  }
}

async function saveConfig(
  next: ConfigSnapshot,
  pickerToken: string | null,
  onCommitted?: () => void,
  resumeAfterConfirmation = false,
): Promise<void> {
  if (!revision) throw new Error("Host configuration has not loaded");
  const expectedRevision = revision;
  // The host shows the exact expansion. Its token binds this snapshot and revision.
  const confirmed = await host.call({
    type: "confirm_config",
    protocol_version: PROTOCOL_VERSION,
    request_id: newRequestId(),
    expected_revision: expectedRevision,
    config: next,
    picker_token: pickerToken,
  }, "config_confirmed", 5 * 60_000);
  const updated = await host.call({
    type: "update_config",
    protocol_version: PROTOCOL_VERSION,
    request_id: newRequestId(),
    expected_revision: expectedRevision,
    config: next,
    picker_token: pickerToken,
    consent_token: confirmed.consent_token,
  }, "config_updated");
  currentConfig = updated.config;
  revision = updated.revision;
  picker = null;
  onCommitted?.();
  renderConfig();
  await installHostPolicy(resumeAfterConfirmation);
  await refreshQueue();
}

async function reconcileRevocations(): Promise<void> {
  if (!currentConfig || !revision) return;
  const state = await worker<WorkerStatus>({ kind: "get_status" });
  if (!state.revoked_origins.length) return;
  const revoked = new Set(state.revoked_origins);
  const sites = currentConfig.sites.filter((site) => !revoked.has(site.origin));
  if (sites.length !== currentConfig.sites.length) {
    await saveConfig({
      ...currentConfig,
      sites,
      capture_enabled: currentConfig.capture_enabled && sites.length > 0,
    }, null);
  }
  await worker<WorkerStatus>({ kind: "ack_revocations", origins: state.revoked_origins });
  renderConfig();
}

async function replayVisits(): Promise<void> {
  if (closed || replaying || busy || renewing || tickRunning || !currentConfig?.capture_enabled) return;
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
        const response = await host.call({
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
    await refreshQueue().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function renewHostPolicy(): Promise<void> {
  if (closed || busy || replaying || renewing || !currentConfig?.capture_enabled) return;
  renewing = true;
  updateControls();
  try {
    const response = await host.call({
      type: "get_config", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "config_result");
    currentConfig = response.config;
    revision = response.revision;
    renderConfig();
    if (response.config_issue) {
      await worker<WorkerStatus>({ kind: "clear_policy" });
      show(response.config_issue, true);
    } else {
      await installHostPolicy();
      if (!currentConfig.capture_enabled) show("Capture is off in the host configuration.");
    }
  } catch (error) {
    show(`Cannot renew the host policy lease: ${describe(error)}. Capture stops when the current lease expires.`, true);
  } finally {
    renewing = false;
    await refreshQueue().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function panelTick(): Promise<void> {
  if (closed || busy || replaying || renewing || tickRunning || !currentConfig) return;
  tickRunning = true;
  updateControls();
  try {
    const state = await refreshQueue();
    if (state.revoked_origins.length) await reconcileRevocations();
    if (state.policy_expires_at !== null &&
        state.policy_expires_at - Date.now() <= LEASE_RENEW_WINDOW_MS) {
      await renewHostPolicy();
    }
  } catch (error) {
    show(`Panel refresh failed: ${describe(error)}`, true);
  } finally {
    tickRunning = false;
    if (lastWorkerStatus) scheduleReplay(lastWorkerStatus);
    updateControls();
  }
}

async function chooseFolder(): Promise<void> {
  busy = true;
  updateControls();
  try {
    const response = await host.call({
      type: "choose_folder", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "folder_chosen", 5 * 60_000);
    picker = { path: response.path, token: response.picker_token };
    folderPath.textContent = response.path;
    updateControls();
    show(`Folder selected: ${response.path}. Enable a site to save it.`);
  } catch (error) {
    show(describe(error), true);
  } finally {
    busy = false;
    updateControls();
  }
}

async function rollbackNewPermissions(grant: {
  pattern: string; apiGranted: boolean; originGranted: boolean;
}): Promise<void> {
  const removals: Promise<boolean>[] = [];
  if (!grant.originGranted) removals.push(chrome.permissions.remove({ origins: [grant.pattern] }));
  if (!grant.apiGranted) removals.push(chrome.permissions.remove({ permissions: ["webNavigation"] }));
  await Promise.all(removals);
}

function enableSite(): void {
  if (busy || replaying || renewing || tickRunning || !currentConfig || !revision || !preflight) return;
  let parsed: ReturnType<typeof parseSite>;
  try {
    parsed = parseSite();
  } catch (error) {
    show(describe(error), true);
    return;
  }
  if (parsed.site.origin !== preflight.origin || parsed.pattern !== preflight.pattern) return;
  if (lastWorkerStatus?.locally_removed_sites.some((site) => sameSite(site, parsed.site))) {
    show("Finish removing this site from the host before enabling it again.", true);
    return;
  }
  if (!picker && !currentConfig.storage) {
    show("Choose a notes folder first.", true);
    return;
  }

  // This call must run in the click stack. An await before it loses Chrome's
  // user gesture, which prevents the optional permission prompt.
  const granted = chrome.permissions.request({
    permissions: ["webNavigation"], origins: [parsed.pattern],
  });
  const previous = preflight;
  busy = true;
  updateControls();
  void (async () => {
    let committed = false;
    let permissionGranted = false;
    try {
      permissionGranted = await granted;
      if (!permissionGranted) {
        show("Chrome access was declined. Capture remains off.", true);
        return;
      }
      const storage: StorageConfig = picker
        ? {
            root: picker.path,
            profile: "neutral",
            log_dir: currentConfig!.storage?.log_dir ?? "log",
            pages_dir: currentConfig!.storage?.pages_dir ?? "pages",
            later_dir: currentConfig!.storage?.later_dir ?? "later",
          }
        : { ...currentConfig!.storage!, profile: "neutral" };
      const sites = currentConfig!.sites.filter(
        (site) => site.origin !== parsed.site.origin || site.path_prefix !== parsed.site.path_prefix,
      );
      sites.push(parsed.site);
      const next: ConfigSnapshot = {
        ...currentConfig!, storage, sites, capture_enabled: true,
      };
      await saveConfig(next, picker?.token ?? null, () => { committed = true; }, true);
      const workerState = await refreshQueue();
      if (!workerState.navigation_ready) {
        show("Chrome is restarting the extension to activate the new navigation permission. Reopen the panel.", true);
        setTimeout(() => chrome.runtime.reload(), 1_000);
        return;
      }
      show(`Capture enabled for ${parsed.site.origin}${parsed.site.path_prefix}.`);
      await replayVisits();
    } catch (error) {
      show(`Setup failed: ${describe(error)}`, true);
      if (error instanceof HostError && error.code === "conflict") {
        picker = null;
        await reloadConfig().catch(() => undefined);
      }
    } finally {
      if (!committed && permissionGranted) {
        try {
          await rollbackNewPermissions(previous);
        } catch (error) {
          show(`Could not remove new Chrome access: ${describe(error)}`, true);
        }
      }
      busy = false;
      await refreshQueue().catch((error: unknown) => show(describe(error), true));
      updateControls();
      void refreshPreflight();
    }
  })();
}

async function pauseCapture(): Promise<void> {
  if (!currentConfig) return;
  busy = true;
  updateControls();
  try {
    // Stop local buffering immediately, even if the host is unavailable.
    await worker<WorkerStatus>({ kind: "pause_capture" });
    await saveConfig({ ...currentConfig, capture_enabled: false }, null);
    show("Capture paused. Pending visits cleared.");
  } catch (error) {
    show(`Could not pause capture: ${describe(error)}`, true);
  } finally {
    busy = false;
    await refreshQueue().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function removeSite(site: SiteConfig): Promise<void> {
  if (busy || replaying || renewing || tickRunning || !currentConfig || !revision ||
      !currentConfig.sites.some((entry) => sameSite(entry, site))) return;
  busy = true;
  updateControls();
  let locallyRemoved = false;
  let hostCommitted = false;
  let hostFailure: string | null = null;
  let permissionFailure: string | null = null;
  try {
    // The worker serializes this with navigation capture and persists a local
    // block before the host update. A failed update stays blocked on reload.
    lastWorkerStatus = await worker<WorkerStatus>({ kind: "remove_site", site });
    locallyRemoved = true;
    renderConfig();
    const sites = currentConfig.sites.filter((entry) => !sameSite(entry, site));
    const removeOriginGrant = !sites.some((entry) => entry.origin === site.origin);
    try {
      await saveConfig({
        ...currentConfig,
        sites,
        capture_enabled: currentConfig.capture_enabled && sites.length > 0,
      }, null, () => { hostCommitted = true; });
    } catch (error) {
      hostFailure = hostCommitted
        ? `The host saved removal, but the extension policy refresh failed: ${describe(error)}`
        : `Host update failed: ${describe(error)}`;
      if (error instanceof HostError && error.code === "conflict") {
        await reloadConfig().catch(() => undefined);
      }
    }
    if (removeOriginGrant) {
      try {
        await chrome.permissions.remove({ origins: [exactOriginPattern(site.origin)] });
        if (sites.length === 0) {
          await chrome.permissions.remove({ permissions: ["webNavigation"] });
        }
      } catch (error) {
        permissionFailure = `Could not remove Chrome access: ${describe(error)}`;
      }
    }
    if (hostFailure || permissionFailure) {
      const next = !hostFailure ? "" : hostCommitted
        ? " The host removal is saved. Reopen the panel to refresh its policy."
        : " The site remains locally off; use Remove again to finish the host update.";
      const grant = permissionFailure
        ? " Remove this origin from Chrome extension settings if it remains granted."
        : "";
      show(`${[hostFailure, permissionFailure].filter(Boolean).join(" ")}${next}${grant}`, true);
    } else if (hostCommitted) {
      show(`Removed ${site.origin}${site.path_prefix}.`);
    } else {
      show("The site is locally off; reopen the panel to confirm the host update.", true);
    }
  } catch (error) {
    show(locallyRemoved
      ? `The site is locally off, but removal needs attention: ${describe(error)}`
      : `Could not stop this site locally: ${describe(error)}`, true);
  } finally {
    busy = false;
    await refreshQueue().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function createPageNote(): Promise<void> {
  if (!currentConfig?.storage || !currentConfig.capture_enabled || !revision) return;
  busy = true;
  updateControls();
  noteResult.textContent = "";
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab || tab.incognito || !tab.url) throw new Error("Open a permitted page in a normal Chrome window");
    const url = new URL(tab.url);
    if (!isHttpUrl(url) || url.username || url.password) throw new Error("This page is not an HTTP(S) site");
    const lease = leaseFor(currentConfig, revision);
    if (!matchingSite(lease, url.href)) throw new Error("Enable this site before creating a page note");
    const allowed = await chrome.permissions.contains({
      permissions: ["webNavigation"], origins: [exactOriginPattern(url.origin)],
    });
    if (!allowed) throw new Error("Chrome access for this site was removed");
    const response = await host.call({
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

chooseFolderButton.addEventListener("click", () => void chooseFolder());
enableButton.addEventListener("click", enableSite);
pauseButton.addEventListener("click", () => void pauseCapture());
replayButton.addEventListener("click", () => void replayVisits());
noteButton.addEventListener("click", () => void createPageNote());
siteUrl.addEventListener("input", () => void refreshPreflight());
sitePath.addEventListener("input", () => void refreshPreflight());
const pollTimer = setInterval(() => void panelTick(), RETRY_POLL_MS);
window.addEventListener("pagehide", () => {
  closed = true;
  clearInterval(pollTimer);
  if (retryTimer !== null) clearTimeout(retryTimer);
  host.disconnect();
});

void (async () => {
  busy = true;
  updateControls();
  let initialized = false;
  try {
    const hello = await host.call({
      type: "hello", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "hello_result");
    await reloadConfig();
    if (hello.config_issue) show(hello.config_issue, true);
    initialized = true;
  } catch (error) {
    show(`Native host unavailable: ${describe(error)}. Install or register the development host, then reopen the panel.`, true);
  } finally {
    busy = false;
    await refreshQueue().catch(() => undefined);
    updateControls();
  }
  if (initialized) await replayVisits();
})();
