import type { VisitEvent } from "../protocol/ts/generated.js";
import {
  MAX_QUEUE_BYTES,
  MAX_QUEUED_VISITS,
  POLICY_STORAGE_KEY,
  TRUSTED_PAGES,
  emptyQueue,
  exactOriginPattern,
  isPolicyLease,
  isQueuedVisit,
  localTimestamp,
  matchingSite,
  siteMatchesUrl,
  type PolicyLease,
  type QueuedVisit,
  type QueueState,
  type WorkerReply,
  type WorkerRequest,
  type WorkerStatus,
} from "./model.js";
import { APP_NAME, BINARY_NAME, storageKey } from "./brand.js";

const QUEUE_KEY = storageKey("queue_v1");
const POLICY_KEY = POLICY_STORAGE_KEY;
const REVOCATIONS_KEY = storageKey("revocations_v1");
const PAUSE_KEY = storageKey("pause_pending_v1");
const PAUSE_TOKEN_KEY = storageKey("pause_token_v1");
const REMOVED_SITES_KEY = storageKey("locally_removed_sites_v1");
// Matches MAX_URL_BYTES in host/src/capture.rs, so the host never rejects a
// visit the worker chose to buffer.
const MAX_VISIT_URL_BYTES = 8_192;
const MAX_TITLE_CHARS = 300;
const DEDUPE_WINDOW_MS = 2_000;
// Hold an untitled visit briefly so tabs.onUpdated can supply its title
// before an open panel sends, and thereby freezes, the event.
const TITLE_GRACE_MS = 3_000;
const encoder = new TextEncoder();

let storageReady: Promise<void> | null = null;
let sequence: Promise<unknown> = Promise.resolve();
let navigationReady = false;

function ensureTrustedStorage(): Promise<void> {
  storageReady ??= chrome.storage.local.setAccessLevel({ accessLevel: "TRUSTED_CONTEXTS" });
  return storageReady;
}

function serialize<T>(work: () => Promise<T>): Promise<T> {
  const next = sequence.then(work, work);
  sequence = next.then(
    () => undefined,
    () => undefined,
  );
  return next;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function readPolicy(): Promise<PolicyLease | null> {
  const stored = await chrome.storage.local.get(POLICY_KEY);
  const lease = stored[POLICY_KEY];
  return isPolicyLease(lease) ? lease : null;
}

async function readQueue(): Promise<QueueState> {
  const stored = await chrome.storage.local.get(QUEUE_KEY);
  const value = stored[QUEUE_KEY];
  if (value === undefined) return emptyQueue();
  if (typeof value !== "object" || value === null) {
    throw new Error("Visit queue is unreadable; capture is paused");
  }
  const state = value as Record<string, unknown>;
  if (
    state.version !== 1 ||
    !Array.isArray(state.items) ||
    !state.items.every(isQueuedVisit) ||
    typeof state.overflow_count !== "number" ||
    typeof state.rejected_count !== "number" ||
    !(state.last_error === null || typeof state.last_error === "string") ||
    !(state.retry_error === undefined || state.retry_error === null ||
      typeof state.retry_error === "string")
  ) {
    throw new Error("Visit queue is unreadable; capture is paused");
  }
  // Queues stored before retry_error existed kept retry failures in last_error.
  return { ...(value as QueueState), retry_error: (state.retry_error as string | null | undefined) ?? null };
}

async function writeQueue(state: QueueState): Promise<void> {
  await chrome.storage.local.set({ [QUEUE_KEY]: state });
}

async function readRevocations(): Promise<string[]> {
  const stored = await chrome.storage.local.get(REVOCATIONS_KEY);
  const value = stored[REVOCATIONS_KEY];
  return Array.isArray(value) && value.every((entry) => typeof entry === "string")
    ? value.slice(0, 256)
    : [];
}

async function pausePending(): Promise<boolean> {
  const stored = await chrome.storage.local.get(PAUSE_KEY);
  return stored[PAUSE_KEY] === true;
}

async function readPauseToken(): Promise<string | null> {
  const stored = await chrome.storage.local.get(PAUSE_TOKEN_KEY);
  const value = stored[PAUSE_TOKEN_KEY];
  if (value === undefined) return null;
  if (typeof value !== "string") throw new Error("Local pause state is unreadable; capture is paused");
  return value;
}

function sameSite(left: { origin: string; path_prefix: string }, right: {
  origin: string; path_prefix: string;
}): boolean {
  return left.origin === right.origin && left.path_prefix === right.path_prefix;
}

function validSite(value: unknown): value is { origin: string; path_prefix: string } {
  if (typeof value !== "object" || value === null) return false;
  const site = value as Record<string, unknown>;
  if (typeof site.origin !== "string" || typeof site.path_prefix !== "string") return false;
  try {
    exactOriginPattern(site.origin);
  } catch {
    return false;
  }
  const path = site.path_prefix;
  return path.startsWith("/") && !path.includes("?") && !path.includes("#") &&
    !path.includes("\\") && !path.split("/").some((part) => part === "." || part === "..");
}

async function readLocallyRemovedSites(): Promise<Array<{ origin: string; path_prefix: string }>> {
  const stored = await chrome.storage.local.get(REMOVED_SITES_KEY);
  const value = stored[REMOVED_SITES_KEY];
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.length > 256 || !value.every(validSite)) {
    throw new Error("Local site removal state is unreadable; capture is paused");
  }
  return value;
}

function matchesAnySite(sites: Array<{ origin: string; path_prefix: string }>, rawUrl: string): boolean {
  try {
    const url = new URL(rawUrl);
    return sites.some((site) => siteMatchesUrl(site, url));
  } catch {
    return false;
  }
}

async function checkedGrant(origin: string): Promise<boolean | null> {
  try {
    return await chrome.permissions.contains({
      permissions: ["webNavigation"],
      origins: [exactOriginPattern(origin)],
    });
  } catch {
    return null;
  }
}

async function purgeRevokedQueueWithoutPolicy(): Promise<void> {
  const queue = await readQueue();
  if (!queue.items.length) return;
  const grants = new Map<string, boolean | null>();
  const retained: QueuedVisit[] = [];
  const revoked = new Set<string>();
  for (const item of queue.items) {
    let origin: string;
    try {
      const url = new URL(item.event.url);
      if (url.protocol !== "http:" && url.protocol !== "https:") continue;
      origin = url.origin;
    } catch {
      continue;
    }
    if (!grants.has(origin)) grants.set(origin, await checkedGrant(origin));
    if (grants.get(origin) !== false) retained.push(item);
    else revoked.add(origin);
  }
  if (retained.length === queue.items.length) return;
  queue.items = retained;
  const removed = [...revoked];
  queue.last_error = removed.length
    ? `Chrome access was removed for ${removed.join(", ")}; pending visits were purged`
    : "Invalid pending visits were purged";
  const revocations = await readRevocations();
  await chrome.storage.local.set({
    [QUEUE_KEY]: queue,
    [REVOCATIONS_KEY]: [...new Set([...revocations, ...removed])].slice(0, 256),
  });
}

async function reconcileGrants(): Promise<PolicyLease | null> {
  // The local removal list is an independent safety gate. If it is corrupt,
  // fail closed even when an older cached policy is otherwise readable.
  const locallyRemoved = await readLocallyRemovedSites();
  let lease = await readPolicy();
  if (!lease) {
    await purgeRevokedQueueWithoutPolicy();
    return null;
  }
  if (await pausePending()) return null;
  const activeSites = lease.sites.filter((site) =>
    !locallyRemoved.some((removed) => sameSite(removed, site)));
  if (activeSites.length !== lease.sites.length) {
    lease = { ...lease, sites: activeSites };
    const queue = await readQueue();
    queue.items = queue.items.filter((item) => matchesAnySite(activeSites, item.event.url));
    await chrome.storage.local.set({ [POLICY_KEY]: lease, [QUEUE_KEY]: queue });
  }
  if (!lease.capture_enabled) {
    const queue = await readQueue();
    if (queue.items.length) {
      queue.items = [];
      await writeQueue(queue);
    }
    return lease;
  }

  const retained = [];
  const removed: string[] = [];
  for (const site of lease.sites) {
    const grant = await checkedGrant(site.origin);
    if (grant === null) throw new Error("Chrome access could not be checked; capture is paused");
    if (grant) retained.push(site);
    else removed.push(site.origin);
  }
  if (!removed.length) return lease;

  const narrowed: PolicyLease = { ...lease, sites: retained };
  const queue = await readQueue();
  queue.items = queue.items.filter((item) => matchesAnySite(narrowed.sites, item.event.url));
  queue.last_error = `Chrome access was removed for ${removed.join(", ")}; pending visits were purged`;
  const revocations = await readRevocations();
  await chrome.storage.local.set({
    [POLICY_KEY]: narrowed,
    [QUEUE_KEY]: queue,
    [REVOCATIONS_KEY]: [...new Set([...revocations, ...removed])].slice(0, 256),
  });
  return narrowed;
}

function dedupeKey(details: ChromeNavigationDetails): string {
  const url = new URL(details.url);
  url.hash = "";
  const document = details.documentId ?? "unknown-document";
  const timeBucket = Math.floor(details.timeStamp / DEDUPE_WINDOW_MS);
  return `${details.tabId}:${document}:${timeBucket}:${url.href}`;
}

function withoutHash(rawUrl: string): string | null {
  try {
    const url = new URL(rawUrl);
    url.hash = "";
    return url.href;
  } catch {
    return null;
  }
}

async function captureNavigation(
  details: ChromeNavigationDetails,
  useCurrentTitle: boolean,
): Promise<void> {
  if (details.frameId !== 0 || details.documentLifecycle !== "active" || details.tabId < 0) {
    return;
  }
  await ensureTrustedStorage();
  // Most navigations are for sites the user never enabled. Check the cached
  // lease first so they cost one storage read, not a full grant reconcile.
  const cached = await readPolicy();
  if (!cached || !matchingSite(cached, details.url)) return;
  const lease = await reconcileGrants();
  if (!lease || !matchingSite(lease, details.url)) return;
  if (encoder.encode(details.url).length > MAX_VISIT_URL_BYTES) return;

  let tab: ChromeTab;
  try {
    tab = await chrome.tabs.get(details.tabId);
  } catch {
    return;
  }
  if (tab.incognito !== false) return;

  const visitedAt = new Date(details.timeStamp);
  if (!Number.isFinite(visitedAt.getTime())) return;
  const occurred_at = localTimestamp(visitedAt);
  // At commit, the tab still shows the previous document's title. The title
  // arrives later through tabs.onUpdated. A same-document history update
  // already has its current title.
  let title: string | null = null;
  if (useCurrentTitle && tab.url) {
    // A page title is optional. The navigation URL remains authoritative.
    const current = withoutHash(tab.url);
    if (current !== null && current === withoutHash(details.url)) {
      title = tab.title?.slice(0, MAX_TITLE_CHARS) ?? null;
    }
  }

  const event: VisitEvent = {
    event_id: crypto.randomUUID(),
    url: details.url,
    title,
    occurred_at,
    incognito: false,
  };
  const queue = await readQueue();
  const key = dedupeKey(details);
  if (queue.items.some((item) => item.dedupe_key === key)) return;
  const item: QueuedVisit = {
    event,
    dedupe_key: key,
    attempts: 0,
    retry_after: title === null ? Date.now() + TITLE_GRACE_MS : 0,
    tab_id: details.tabId,
    dispatched: false,
  };
  const nextItems = [...queue.items, item];
  if (
    nextItems.length > MAX_QUEUED_VISITS ||
    encoder.encode(JSON.stringify(nextItems)).length > MAX_QUEUE_BYTES
  ) {
    queue.overflow_count += 1;
    queue.last_error = "Visit queue is full; newer visits were not buffered";
  } else {
    queue.items = nextItems;
  }
  await writeQueue(queue);
}

async function patchTitle(tabId: number, rawTitle: string, tab: ChromeTab): Promise<void> {
  if (tab.incognito !== false || !tab.url) return;
  await ensureTrustedStorage();
  const lease = await readPolicy();
  if (!lease) return;
  const current = withoutHash(tab.url);
  if (current === null) return;
  const queue = await readQueue();
  let latest: QueuedVisit | undefined;
  for (const item of queue.items) {
    if (item.tab_id === tabId) latest = item;
  }
  // Only the tab's newest visit may take the title, and only before the panel
  // has seen it. A dispatched event may already be partly written by the host.
  if (!latest || latest.dispatched !== false || withoutHash(latest.event.url) !== current ||
      !matchingSite(lease, latest.event.url)) {
    return;
  }
  const title = rawTitle.slice(0, MAX_TITLE_CHARS);
  if (latest.event.title === title) return;
  latest.event = { ...latest.event, title };
  await writeQueue(queue);
}

async function installPolicy(
  lease: PolicyLease,
  resumeAfterConfirmation: boolean,
  resumeAfterPauseToken: string | null,
): Promise<WorkerStatus> {
  if (!isPolicyLease(lease) || lease.expires_at <= Date.now()) {
    throw new Error("Host policy lease is invalid or expired");
  }
  if (await pausePending()) {
    if (!resumeAfterConfirmation) return currentStatus();
    // A pause requested during native confirmation must win over an older
    // enable operation, even if that enable later commits its host config.
    if (lease.capture_enabled && resumeAfterPauseToken !== await readPauseToken()) {
      return currentStatus();
    }
  }
  const removed = await readLocallyRemovedSites();
  const stillRemoved = removed.filter((site) => lease.sites.some((entry) => sameSite(entry, site)));
  const effectiveLease: PolicyLease = {
    ...lease,
    sites: lease.sites.filter((site) => !stillRemoved.some((entry) => sameSite(entry, site))),
  };
  const queue = await readQueue();
  queue.items = queue.items.filter((item) =>
    effectiveLease.capture_enabled && matchesAnySite(effectiveLease.sites, item.event.url));
  await chrome.storage.local.set({
    [POLICY_KEY]: effectiveLease,
    [PAUSE_KEY]: false,
    [REMOVED_SITES_KEY]: stillRemoved,
    [QUEUE_KEY]: queue,
  });
  const reconciled = await reconcileGrants();
  return currentStatus(reconciled);
}

async function currentStatus(
  lease: PolicyLease | null = null,
  queue: QueueState | null = null,
): Promise<WorkerStatus> {
  const policy = lease ?? (await readPolicy());
  const visits = queue ?? (await readQueue());
  const paused = await pausePending();
  const nextRetryAt = !paused && policy?.capture_enabled && policy.expires_at > Date.now()
    ? visits.items
      .filter((item) => matchesAnySite(policy.sites, item.event.url))
      .reduce<number | null>((earliest, item) =>
        earliest === null ? item.retry_after : Math.min(earliest, item.retry_after), null)
    : null;
  return {
    queued: visits.items.length,
    next_retry_at: nextRetryAt,
    overflow_count: visits.overflow_count,
    rejected_count: visits.rejected_count,
    last_error: visits.last_error,
    retry_error: visits.retry_error,
    policy_expires_at: policy?.expires_at ?? null,
    revoked_origins: await readRevocations(),
    pause_pending: paused,
    pause_token: await readPauseToken(),
    navigation_ready: navigationReady,
    locally_removed_sites: await readLocallyRemovedSites(),
  };
}

async function handleMessage(message: WorkerRequest): Promise<unknown> {
  registerNavigationListeners();
  await ensureTrustedStorage();
  switch (message.kind) {
    case "install_policy":
      return installPolicy(message.lease, message.resume_after_confirmation,
        message.resume_after_pause_token);
    case "suspend_policy":
      // A repairable host config issue is not a user revocation. Remove the
      // capture lease immediately, but leave queued visits and an explicit
      // local pause untouched until a valid host policy can reconcile them.
      await chrome.storage.local.set({ [POLICY_KEY]: null });
      await reconcileGrants();
      return currentStatus();
    case "pause_capture": {
      const queue = await readQueue().catch(() => emptyQueue());
      queue.items = [];
      await chrome.storage.local.set({
        [PAUSE_KEY]: true, [PAUSE_TOKEN_KEY]: crypto.randomUUID(),
        [POLICY_KEY]: null, [QUEUE_KEY]: queue,
      });
      return currentStatus(null, queue);
    }
    case "get_pending_ids":
      return (await readQueue()).items.map((item) => item.event.event_id);
    case "discard_pending": {
      if (!Array.isArray(message.event_ids) ||
          message.event_ids.length > MAX_QUEUED_VISITS ||
          !message.event_ids.every((id) => typeof id === "string")) {
        throw new Error("Invalid pending-visit discard request");
      }
      const selected = new Set(message.event_ids);
      const queue = await readQueue();
      queue.items = queue.items.filter((item) => !selected.has(item.event.event_id));
      await writeQueue(queue);
      return currentStatus(null, queue);
    }
    case "remove_site": {
      if (!validSite(message.site)) throw new Error("Invalid site removal request");
      const site = message.site;
      const removed = await readLocallyRemovedSites();
      const nextRemoved = removed.some((entry) => sameSite(entry, site))
        ? removed : [...removed, site];
      if (nextRemoved.length > 256) throw new Error("Too many locally removed sites");
      const lease = await readPolicy();
      const remaining = lease?.sites.filter((entry) => !sameSite(entry, site)) ?? [];
      const queue = await readQueue();
      queue.items = queue.items.filter((item) =>
        !matchesAnySite([site], item.event.url) || matchesAnySite(remaining, item.event.url));
      await chrome.storage.local.set({
        [REMOVED_SITES_KEY]: nextRemoved,
        [POLICY_KEY]: lease ? { ...lease, sites: remaining } : null,
        [QUEUE_KEY]: queue,
      });
      return currentStatus(null, queue);
    }
    case "get_pending": {
      const lease = await reconcileGrants();
      if (!lease || lease.expires_at <= Date.now() || !lease.capture_enabled) return [];
      const queue = await readQueue();
      const ready = queue.items.filter(
        (item) =>
          item.retry_after <= Date.now() && matchingSite(lease, item.event.url) !== null,
      );
      // Freeze these events before the panel can send them to the host.
      if (ready.some((item) => item.dispatched !== true)) {
        for (const item of ready) item.dispatched = true;
        await writeQueue(queue);
      }
      return ready;
    }
    case "ack_visit": {
      const queue = await readQueue();
      const item = queue.items.find((entry) => entry.event.event_id === message.event_id);
      if (!item) return currentStatus(null, queue);
      if (message.outcome === "retryable") {
        item.attempts += 1;
        item.retry_after = Date.now() + Math.min(60 * 60_000, 5_000 * 2 ** Math.min(item.attempts, 10));
        queue.retry_error = message.reason ?? "The host could not save this visit yet";
      } else {
        queue.items = queue.items.filter((entry) => entry.event.event_id !== message.event_id);
        if (message.outcome === "rejected") {
          queue.rejected_count += 1;
          queue.last_error = message.reason ?? "A visit was rejected by host policy";
        } else if (!queue.items.some((entry) => entry.attempts > 0)) {
          // last_error holds purge, overflow, and rejection notices. Only the
          // user dismisses those; a later success clears retry state alone.
          queue.retry_error = null;
        }
      }
      await writeQueue(queue);
      return currentStatus(null, queue);
    }
    case "get_status":
      await reconcileGrants();
      return currentStatus();
    case "clear_notices": {
      const queue = await readQueue();
      queue.last_error = null;
      queue.overflow_count = 0;
      queue.rejected_count = 0;
      await writeQueue(queue);
      return currentStatus(null, queue);
    }
    case "ack_reenabled_origin": {
      // An older revocation must not delete a newly confirmed site. Clear it
      // only when this exact host revision is active and Chrome still grants
      // the origin. A fresh removal event queued after this message restores
      // the revocation marker.
      const lease = await readPolicy();
      if (!lease || lease.revision !== message.revision || !lease.capture_enabled ||
          !lease.sites.some((site) => site.origin === message.origin) ||
          await checkedGrant(message.origin) !== true) {
        return currentStatus();
      }
      const known = await readRevocations();
      await chrome.storage.local.set({
        [REVOCATIONS_KEY]: known.filter((origin) => origin !== message.origin),
      });
      return currentStatus();
    }
    case "ack_revocations": {
      const known = await readRevocations();
      const acknowledged = new Set(message.origins);
      await chrome.storage.local.set({
        [REVOCATIONS_KEY]: known.filter((origin) => !acknowledged.has(origin)),
      });
      return currentStatus();
    }
  }
}

function isWorkerRequest(value: unknown): value is WorkerRequest {
  return typeof value === "object" && value !== null && typeof (value as { kind?: unknown }).kind === "string";
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id ||
      !TRUSTED_PAGES.some((page) => sender.url === chrome.runtime.getURL(page))) {
    return false;
  }
  if (!isWorkerRequest(message)) {
    sendResponse({ ok: false, value: null, error: "Unknown worker request" } satisfies WorkerReply<null>);
    return false;
  }
  void serialize(() => handleMessage(message))
    .then((value) => sendResponse({ ok: true, value, error: null } satisfies WorkerReply<unknown>))
    .catch((error: unknown) =>
      sendResponse({ ok: false, value: null, error: describe(error) } satisfies WorkerReply<null>),
    );
  return true;
});

const onCommitted = (details: ChromeNavigationDetails): void => {
  void serialize(() => captureNavigation(details, false)).catch((error: unknown) => {
    console.warn(`${APP_NAME} navigation capture paused:`, describe(error));
  });
};

const onHistoryStateUpdated = (details: ChromeNavigationDetails): void => {
  void serialize(() => captureNavigation(details, true)).catch((error: unknown) => {
    console.warn(`${APP_NAME} SPA capture paused:`, describe(error));
  });
};

function registerNavigationListeners(): void {
  if (navigationReady || !chrome.webNavigation) return;
  try {
    chrome.webNavigation.onCommitted.addListener(onCommitted);
    chrome.webNavigation.onHistoryStateUpdated.addListener(onHistoryStateUpdated);
    navigationReady = true;
  } catch (error) {
    console.warn(`${APP_NAME} navigation listeners unavailable:`, describe(error));
  }
}

// Register synchronously whenever the optional API is present so MV3 can wake
// this worker for navigation. A fresh permission grant can expose it later.
registerNavigationListeners();

// Chrome reports a tab's title only for origins this extension was granted.
chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  const title = changeInfo.title;
  if (title === undefined) return;
  void serialize(() => patchTitle(tabId, title, tab)).catch((error: unknown) => {
    console.warn(`${APP_NAME} title update skipped:`, describe(error));
  });
});

chrome.permissions.onAdded.addListener((permissions) => {
  if (permissions.permissions?.includes("webNavigation")) registerNavigationListeners();
});

chrome.permissions.onRemoved.addListener(() => {
  void serialize(async () => {
    await ensureTrustedStorage();
    await reconcileGrants();
  }).catch((error: unknown) => {
    console.warn(`${APP_NAME} permission reconciliation failed:`, describe(error));
  });
});

void chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch((error: unknown) => {
  console.warn(`${APP_NAME} side panel setup failed:`, describe(error));
});

// The first note on a page the extension has no standing grant for starts
// from one of these triggers, which grant `activeTab` for the invoking tab
// (§5.3). The toolbar action's own grant is Chrome's, from declaring
// "action" and setPanelBehavior above; no extra listener is needed for it.
const NOTE_CONTEXT_MENU_ID = `${BINARY_NAME}-note`;

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: NOTE_CONTEXT_MENU_ID,
      title: `Add a note in ${APP_NAME}`,
      contexts: ["page"],
    });
  });
});

function openPanelForNote(tab: ChromeTab | undefined): void {
  if (tab?.id === undefined) return;
  // Open synchronously in this handler, before any await, so Chrome still
  // attributes the call to the user gesture that triggered it (§5.2, §5.3).
  void chrome.sidePanel.open({ tabId: tab.id }).catch((error: unknown) => {
    console.warn(`${APP_NAME} could not open the side panel for a note:`, describe(error));
  });
}

chrome.commands.onCommand.addListener((command, tab) => {
  if (command === "note") openPanelForNote(tab);
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === NOTE_CONTEXT_MENU_ID) openPanelForNote(tab);
});
