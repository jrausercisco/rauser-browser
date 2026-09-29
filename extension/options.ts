import type {
  ConfigSnapshot, HarnessAdapter, HarnessOffer, SiteConfig, StorageConfig,
} from "../protocol/ts/generated.js";
import { exactOriginPattern, isHttpUrl, type WorkerStatus } from "./model.js";
import {
  ConfigSession,
  describe,
  element,
  sameSite,
  setupProblem,
  withConfigMutationLock,
  worker,
} from "./settings.js";
import { HostError, PROTOCOL_VERSION, newRequestId } from "./native.js";
import { APP_NAME } from "./brand.js";
import {
  SUGGESTED_EXCLUSIONS,
  addDenylistEntry,
  agentStateText,
  denylistEditsLive,
  harnessFoundText,
  harnessOfferNote,
  previewDenylistEntry,
  removeDenylistEntry,
} from "./agent.js";
import { forgetNoteOrigin, noteOrigins, pruneNoteOrigins } from "./grants.js";

const status = element<HTMLDivElement>("status");
const folderPath = element<HTMLOutputElement>("folder-path");
const chooseFolderButton = element<HTMLButtonElement>("choose-folder");
const siteUrl = element<HTMLInputElement>("site-url");
const sitePath = element<HTMLInputElement>("site-path");
const enableButton = element<HTMLButtonElement>("enable-site");
const pauseButton = element<HTMLButtonElement>("pause-capture");
const sitesList = element<HTMLUListElement>("sites-list");
const agentState = element<HTMLParagraphElement>("agent-state");
const detectButton = element<HTMLButtonElement>("detect-harnesses");
const offersList = element<HTMLUListElement>("harness-offers");
const envList = element<HTMLUListElement>("harness-env");
const summariesInput = element<HTMLInputElement>("summaries-dir");
const denylistInput = element<HTMLInputElement>("denylist-input");
const denylistPreview = element<HTMLOutputElement>("denylist-preview");
const denylistAddButton = element<HTMLButtonElement>("denylist-add");
const denylistSuggestions = element<HTMLDivElement>("denylist-suggestions");
const denylistList = element<HTMLUListElement>("denylist-list");
const setupButton = element<HTMLButtonElement>("setup-harness");
const removeHarnessButton = element<HTMLButtonElement>("remove-harness");

let picker: { path: string; token: string } | null = null;
let busy = false;
let reloadTimer: ReturnType<typeof setTimeout> | null = null;
let preflightVersion = 0;
let preflight: {
  origin: string;
  pattern: string;
  apiGranted: boolean;
  originGranted: boolean;
} | null = null;
// Harness offers are single-use and bound to the revision they were found at.
let offers: HarnessOffer[] = [];
let selectedOfferId: string | null = null;
const envChoices = new Map<string, boolean>();
// Until setup confirms the list, exclusion edits stay on this page and are
// sent with the setup request, which shows them in the native confirmation.
let pendingDenylist: string[] | null = null;
let summariesEdited = false;

// The host binds a folder selection and each harness offer to the revision it
// was made at, so any save, from this page or another, makes them unusable.
const session = new ConfigSession(renderConfig, () => {
  picker = null;
  clearOffers();
});

// A folder selection's token and harness offers live only in the host process
// that issued them. After that process exits, the next call starts a new one
// that would refuse them, so the folder must be chosen, or harnesses
// detected, again.
session.host.onHostExit(() => {
  const hadPicker = picker !== null;
  const hadOffers = offers.length > 0;
  if (!hadPicker && !hadOffers) return;
  picker = null;
  clearOffers();
  renderConfig();
  const lost = [
    ...(hadPicker ? ["the folder selection was lost. Choose the folder again."] : []),
    ...(hadOffers ? ["the detected harnesses were lost. Detect harnesses again."] : []),
  ];
  show(`The native host restarted, so ${lost.join(" Also, ")}`, true);
});

function show(message: string, warning = false): void {
  status.textContent = message;
  status.classList.toggle("warning", warning);
}

function showConfigState(): void {
  const problem = setupProblem(session.config, session.configIssue);
  if (session.configIssue) show(problem!, true);
  else if (problem) {
    show(`${problem} ${session.config?.storage ? "Enable" : "Choose a folder and enable"} a site to begin.`);
  }
  else show("Capture is enabled for the listed sites.");
}

/**
 * Whether a save must carry a fresh folder selection. The host keeps the
 * settings of a config whose folder is missing or changed, but it accepts no
 * save based on that folder until it is chosen again through the picker.
 */
function needsFolderPick(): boolean {
  return !session.config?.storage || session.configIssue !== null;
}

function updateControls(): void {
  const connected = session.connected;
  // Only a save with a new folder selection can succeed while the host
  // configuration needs repair, so Pause and Remove wait for that repair.
  const repairing = session.configIssue !== null;
  chooseFolderButton.disabled = !connected || busy;
  enableButton.disabled = !connected || busy || preflight === null ||
    (!picker && needsFolderPick());
  pauseButton.disabled = !connected || busy || repairing || session.config?.capture_enabled !== true;
  for (const button of sitesList.querySelectorAll("button")) {
    button.disabled = !connected || busy || repairing;
  }
  const idle = connected && !busy;
  detectButton.disabled = !idle || session.configIssue !== null;
  setupButton.disabled = !idle || !session.config?.storage || selectedOffer() === null;
  removeHarnessButton.disabled = !idle || !session.config?.agent;
  denylistAddButton.disabled = !idle || !addDenylistEntry(shownDenylist(), denylistInput.value).ok;
  for (const control of denylistList.querySelectorAll("button")) control.disabled = !idle;
  for (const input of offersList.querySelectorAll<HTMLInputElement>("input")) {
    input.disabled = !idle || input.dataset.ready !== "true";
  }
  for (const input of envList.querySelectorAll<HTMLInputElement>("input")) {
    input.disabled = !idle || input.dataset.required === "true";
  }
}

function harnessName(adapter: HarnessAdapter): string {
  return adapter === "claude_code" ? "Claude Code" : "Codex";
}

function clearOffers(): void {
  offers = [];
  selectedOfferId = null;
  envChoices.clear();
}

function selectedOffer(): HarnessOffer | null {
  return offers.find((offer) => offer.offer_id !== null && offer.refusal === null &&
    offer.offer_id === selectedOfferId) ?? null;
}

function selectOffer(offerId: string | null): void {
  selectedOfferId = offerId;
  envChoices.clear();
  // Credentials the host has are proposed; the user can untick any of them.
  for (const env of selectedOffer()?.env_optional ?? []) envChoices.set(env.name, env.present);
}

function denylistLive(): boolean {
  return denylistEditsLive(session.config);
}

function shownDenylist(): string[] {
  if (denylistLive()) return session.config!.agent_denylist;
  return pendingDenylist ?? session.config?.agent_denylist ?? [];
}

function renderAgent(): void {
  const config = session.config;
  const state = [agentStateText(session.agentStatus)];
  if (config?.agent) state.push(`Harness: ${config.agent.binary}.`);
  if (config?.storage?.summaries_dir) state.push(`Summaries folder: ${config.storage.summaries_dir}.`);
  agentState.textContent = state.join(" ");
  if (!summariesEdited) summariesInput.value = config?.storage?.summaries_dir ?? "summaries";

  offersList.replaceChildren();
  if (!offers.length) {
    const item = document.createElement("li");
    item.textContent = "Not detected yet.";
    offersList.append(item);
  }
  for (const [index, offer] of offers.entries()) {
    const ready = offer.offer_id !== null && offer.refusal === null;
    const item = document.createElement("li");
    item.dataset.adapter = offer.adapter;
    item.dataset.ready = String(ready);
    if (offer.real_path !== null) item.dataset.realPath = offer.real_path;
    const label = document.createElement("label");
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "harness-offer";
    radio.id = `harness-offer-${index}`;
    radio.value = offer.offer_id ?? "";
    radio.dataset.ready = String(ready);
    radio.checked = ready && offer.offer_id === selectedOfferId;
    radio.addEventListener("change", () => {
      selectOffer(offer.offer_id);
      renderConfig();
    });
    const version = offer.version ? ` ${offer.version}` : "";
    const runs = offer.real_path && offer.real_path !== offer.binary ? ` (runs ${offer.real_path})` : "";
    const text = document.createElement("span");
    text.textContent = `${harnessName(offer.adapter)}${version}: ${offer.binary}${runs}`;
    label.append(radio, text);
    item.append(label);
    const note = document.createElement("small");
    note.textContent = ` ${harnessOfferNote(offer)}`;
    item.append(note);
    offersList.append(item);
  }

  envList.replaceChildren();
  const offer = selectedOffer();
  if (!offer) {
    const item = document.createElement("li");
    item.textContent = "Detect a harness first.";
    envList.append(item);
  } else {
    const names = [
      ...offer.env_required.map((name) => ({ name, required: true, present: true })),
      ...offer.env_optional.map((env) => ({ name: env.name, required: false, present: env.present })),
    ];
    for (const env of names) {
      const item = document.createElement("li");
      const label = document.createElement("label");
      const box = document.createElement("input");
      box.type = "checkbox";
      box.dataset.env = env.name;
      box.dataset.required = String(env.required);
      box.checked = env.required || envChoices.get(env.name) === true;
      box.addEventListener("change", () => { envChoices.set(env.name, box.checked); });
      const text = document.createElement("span");
      text.textContent = env.required
        ? `${env.name} (always passed)`
        : `${env.name} (${env.present ? "set" : "not set"} in the native host)`;
      label.append(box, text);
      item.append(label);
      envList.append(item);
    }
  }

  denylistList.replaceChildren();
  const list = shownDenylist();
  if (!list.length) {
    const item = document.createElement("li");
    item.textContent = "No domains excluded.";
    denylistList.append(item);
  }
  for (const entry of list) {
    const item = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = entry;
    item.append(label);
    if (!denylistLive() && !(config?.agent_denylist.includes(entry) ?? false)) {
      const pending = document.createElement("small");
      pending.textContent = " Saved when you set up a harness.";
      item.append(pending);
    }
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "secondary";
    remove.textContent = "Remove";
    remove.setAttribute("aria-label", `Remove ${entry}`);
    remove.addEventListener("click", () => void removeExclusion(entry));
    item.append(remove);
    denylistList.append(item);
  }
}

function renderConfig(): void {
  const config = session.config;
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
      if (session.status?.locally_removed_sites.some((entry) => sameSite(entry, site))) {
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
  renderAgent();
  updateControls();
}

async function reloadConfig(): Promise<void> {
  await session.reload();
  void refreshPreflight();
  showConfigState();
  if (session.status?.pause_pending && session.config?.capture_enabled && !session.configIssue) {
    show("Capture is paused locally. Use Pause capture to finish saving this setting in the host.", true);
  }
}

// Another page saved a different revision. Wait for this page's own change
// to finish, then read the host again.
function scheduleReload(): void {
  if (reloadTimer !== null) return;
  reloadTimer = setTimeout(() => {
    reloadTimer = null;
    if (busy) {
      scheduleReload();
      return;
    }
    busy = true;
    updateControls();
    void reloadConfig()
      .catch((error: unknown) => show(`Could not refresh settings: ${describe(error)}`, true))
      .finally(() => {
        busy = false;
        updateControls();
      });
  }, 250);
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

/** The storage settings for a folder chosen through the native picker. */
function storageFor(root: string, current: StorageConfig | null): StorageConfig {
  return {
    root,
    profile: "neutral",
    log_dir: current?.log_dir ?? "log",
    pages_dir: current?.pages_dir ?? "pages",
    later_dir: current?.later_dir ?? "later",
    summaries_dir: current?.summaries_dir ?? null,
  };
}

async function chooseFolder(): Promise<void> {
  busy = true;
  updateControls();
  let selection: typeof picker = null;
  try {
    const response = await session.host.call({
      type: "choose_folder", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "folder_chosen", 5 * 60_000);
    selection = { path: response.path, token: response.picker_token };
    picker = selection;
    folderPath.textContent = response.path;
    updateControls();
    // Save the folder now, so setup is not lost with this page and the side
    // panel stops asking for one. Sites and the capture switch stay as they are.
    await withConfigMutationLock(() => session.saveConfig({
      ...session.config!, storage: storageFor(response.path, session.config!.storage),
    }, response.picker_token));
    picker = null;
    showConfigState();
  } catch (error) {
    if (selection && picker === selection && error instanceof HostError && error.code === "cancelled") {
      // A declined confirmation leaves the selection usable: the first site
      // enabled on this page saves it, behind that site's own confirmation.
      show(`Canceled. ${selection.path} was not saved; enabling a site saves it too.`);
    } else {
      show(describe(error), true);
      if (error instanceof HostError && error.code === "conflict") {
        picker = null;
        await reloadConfig().catch(() => undefined);
      }
    }
  } finally {
    busy = false;
    renderConfig();
  }
}

async function rollbackNewPermissions(grant: {
  origin: string; pattern: string; apiGranted: boolean; originGranted: boolean;
}): Promise<void> {
  if (grant.originGranted && grant.apiGranted) return;
  // Each grant removed here was absent before this click, so nothing saved
  // before it relied on it. That holds while the host configuration needs
  // repair too: a kept config lists its sites, and an unusable one has none
  // that capture could use.
  const latest = await session.readHostConfig();
  const removals: Promise<boolean>[] = [];
  if (!grant.originGranted && !latest.config.sites.some((site) => site.origin === grant.origin) &&
      !(await noteOrigins()).has(grant.origin)) {
    removals.push(chrome.permissions.remove({ origins: [grant.pattern] }));
  }
  if (!grant.apiGranted && latest.config.sites.length === 0) {
    removals.push(chrome.permissions.remove({ permissions: ["webNavigation"] }));
  }
  await Promise.all(removals);
}

function enableSite(): void {
  const currentConfig = session.config;
  if (busy || !currentConfig || !session.revision || !preflight) return;
  let parsed: ReturnType<typeof parseSite>;
  try {
    parsed = parseSite();
  } catch (error) {
    show(describe(error), true);
    return;
  }
  if (parsed.site.origin !== preflight.origin || parsed.pattern !== preflight.pattern) return;
  if (session.status?.locally_removed_sites.some((site) => sameSite(site, parsed.site))) {
    show("Finish removing this site from the host before enabling it again.", true);
    return;
  }
  if (!picker && needsFolderPick()) {
    show(session.configIssue
      ? "Choose the notes folder again to repair the host configuration."
      : "Choose a notes folder first.", true);
    return;
  }
  if (!navigator.locks?.request) {
    show("This Chrome build cannot coordinate configuration changes. Capture remains off.", true);
    return;
  }

  const initialPauseToken = session.status?.pause_token ?? null;

  // This call must run in the click stack. An await before it loses Chrome's
  // user gesture, which prevents the optional permission prompt.
  const granted = chrome.permissions.request({
    permissions: ["webNavigation"], origins: [parsed.pattern],
  });
  const previous = preflight;
  const selection = picker;
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
      // Chrome held no grant for this origin before the click, so any notes
      // entry for it is stale. The new grant belongs to logging.
      if (!previous.originGranted) await forgetNoteOrigin(parsed.site.origin);
      await withConfigMutationLock(async () => {
        // The permission request ran in the click gesture, before the lock.
        // Another page may have removed it while we waited for this lock.
        const stillGranted = await chrome.permissions.contains({
          permissions: ["webNavigation"], origins: [parsed.pattern],
        });
        if (!stillGranted) throw new Error("Chrome access changed during setup; click Enable again");
        const config = session.config!;
        const storage: StorageConfig = selection
          ? storageFor(selection.path, config.storage)
          : { ...config.storage!, profile: "neutral" };
        const sites = config.sites.filter((site) => !sameSite(site, parsed.site));
        sites.push(parsed.site);
        const next: ConfigSnapshot = { ...config, storage, sites, capture_enabled: true };
        await session.saveConfig(next, selection?.token ?? null, () => { committed = true; },
          true, initialPauseToken);
        if (session.revision) {
          await worker<WorkerStatus>({
            kind: "ack_reenabled_origin", origin: parsed.site.origin, revision: session.revision,
          });
        }
      });
      const workerState = await session.refreshStatus();
      if (workerState.pause_pending) {
        try {
          await withConfigMutationLock(async () => {
            // Another page may already have finished pausing while we
            // waited. Only finish a pause that is still pending.
            const latestState = await worker<WorkerStatus>({ kind: "get_status" });
            if (latestState.pause_pending) await session.completePausedHostConfig();
            await session.applyHostConfig(await session.readHostConfig());
          });
          show(session.config?.capture_enabled
            ? "Capture settings changed on another page. Review the current sites."
            : "Capture was paused on another page. Pending visits were cleared.",
          session.config?.capture_enabled === true);
        } catch (error) {
          show(`Capture is paused locally; the host pause needs attention: ${describe(error)}`, true);
        }
        return;
      }
      if (!workerState.navigation_ready) {
        show(`Chrome is restarting the extension to activate the new navigation permission. Reopen ${APP_NAME} settings and the side panel.`, true);
        setTimeout(() => chrome.runtime.reload(), 1_000);
        return;
      }
      show(`Capture enabled for ${parsed.site.origin}${parsed.site.path_prefix}. The side panel sends buffered visits.`);
    } catch (error) {
      show(`Setup failed: ${describe(error)}`, true);
      if (error instanceof HostError && error.code === "conflict") {
        picker = null;
        await reloadConfig().catch(() => undefined);
      }
    } finally {
      if (!committed && permissionGranted) {
        try {
          await withConfigMutationLock(() => rollbackNewPermissions(previous));
        } catch (error) {
          show(`Could not remove new Chrome access: ${describe(error)}`, true);
        }
      }
      busy = false;
      await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
      updateControls();
      void refreshPreflight();
    }
  })();
}

async function pauseCapture(): Promise<void> {
  if (!session.config || session.configIssue) return;
  busy = true;
  updateControls();
  try {
    await session.pauseCapture();
    show("Capture paused. Pending visits cleared.");
  } catch (error) {
    show(`Could not pause capture: ${describe(error)}`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
  }
}

async function removeSite(site: SiteConfig): Promise<void> {
  const baseConfig = session.config;
  if (busy || !baseConfig || !session.revision || session.configIssue ||
      !baseConfig.sites.some((entry) => sameSite(entry, site))) return;
  busy = true;
  updateControls();
  let locallyRemoved = false;
  let hostCommitted = false;
  let committedRevision: string | null = null;
  let hostFailure: string | null = null;
  let permissionFailure: string | null = null;
  let permissionRetained: string | null = null;
  let keptForNotes = false;
  let refreshAfter = false;
  try {
    // The worker serializes this with navigation capture and persists a local
    // block before the host update. A failed update stays blocked on reload.
    session.status = await worker<WorkerStatus>({ kind: "remove_site", site });
    locallyRemoved = true;
    renderConfig();
    await withConfigMutationLock(async () => {
      const sites = baseConfig.sites.filter((entry) => !sameSite(entry, site));
      try {
        await session.saveConfig({
          ...baseConfig,
          sites,
          capture_enabled: baseConfig.capture_enabled && sites.length > 0,
        }, null, () => {
          hostCommitted = true;
          committedRevision = session.revision;
        });
      } catch (error) {
        hostFailure = hostCommitted
          ? `The host saved removal, but the extension policy refresh failed: ${describe(error)}`
          : `Host update failed: ${describe(error)}`;
        if (error instanceof HostError && error.code === "conflict") {
          refreshAfter = true;
        }
      }
      if (hostCommitted && committedRevision !== null) {
        try {
          // Chrome permissions are shared by every path rule for an origin. A
          // second page may have committed another rule while this page was
          // refreshing its worker lease, so inspect the host again before
          // removing the shared grant.
          const latest = await session.readHostConfig();
          if (latest.config_issue) {
            await worker<WorkerStatus>({ kind: "suspend_policy" });
            session.config = latest.config;
            session.revision = latest.revision;
            session.configIssue = latest.config_issue;
            renderConfig();
            permissionRetained = "Chrome access was retained while the host configuration needs repair.";
          } else if (latest.revision !== committedRevision) {
            permissionRetained = "Chrome access was retained because the host configuration changed on another page. Review the current sites before removing it.";
            refreshAfter = true;
          } else if (!latest.config.sites.some((entry) => entry.origin === site.origin)) {
            // The side panel's notes rely on an origin grant it asked for.
            if ((await noteOrigins()).has(site.origin)) {
              keptForNotes = true;
            } else {
              await chrome.permissions.remove({ origins: [exactOriginPattern(site.origin)] });
            }
            if (latest.config.sites.length === 0) {
              await chrome.permissions.remove({ permissions: ["webNavigation"] });
            }
          }
        } catch (error) {
          permissionFailure = `Could not remove Chrome access: ${describe(error)}`;
        }
      }
    });
    if (refreshAfter) {
      await reloadConfig().catch((error: unknown) => {
        permissionRetained = `${permissionRetained ?? "Host policy refresh failed."} ${describe(error)}`;
      });
    }
    if (hostFailure || permissionFailure || permissionRetained) {
      const next = !hostFailure ? "" : hostCommitted
        ? " The host removal is saved. Reload this page to refresh its policy."
        : " The site remains locally off; use Remove again to finish the host update.";
      const grant = permissionFailure
        ? " Remove this origin from Chrome extension settings if it remains granted."
        : "";
      show(`${[hostFailure, permissionFailure, permissionRetained].filter(Boolean).join(" ")}${next}${grant}`, true);
    } else if (hostCommitted) {
      show(`Removed ${site.origin}${site.path_prefix}.${keptForNotes
        ? " Chrome access for this site is kept for its page notes."
        : ""}`);
    } else {
      show("The site is locally off; reload this page to confirm the host update.", true);
    }
  } catch (error) {
    show(locallyRemoved
      ? `The site is locally off, but removal needs attention: ${describe(error)}`
      : `Could not stop this site locally: ${describe(error)}`, true);
  } finally {
    busy = false;
    await session.refreshStatus().catch((error: unknown) => show(describe(error), true));
    updateControls();
    void refreshPreflight();
  }
}

async function detectHarnesses(): Promise<void> {
  if (busy || !session.connected) return;
  busy = true;
  updateControls();
  try {
    offers = await session.discoverHarnesses();
    const ready = offers.find((offer) => offer.offer_id !== null && offer.refusal === null);
    selectOffer(ready?.offer_id ?? null);
    if (ready) {
      show(harnessFoundText(harnessName(ready.adapter), ready.version, !!session.config?.storage));
    } else {
      show(offers.length
        ? "No harness found here can be set up yet; see the reasons below."
        : "No supported harness was found on the native host's PATH.", true);
    }
  } catch (error) {
    clearOffers();
    show(`Could not detect harnesses: ${describe(error)}`, true);
    if (error instanceof HostError && error.code === "conflict") await reloadConfig().catch(() => undefined);
  } finally {
    busy = false;
    renderConfig();
  }
}

async function setupHarness(): Promise<void> {
  const offer = selectedOffer();
  if (busy || !offer?.offer_id) return;
  if (!session.config?.storage) {
    show("Choose a notes folder first.", true);
    return;
  }
  const summaries = summariesInput.value.trim();
  if (!summaries) {
    show("Enter a summaries folder name.", true);
    return;
  }
  const envNames = offer.env_optional.filter((env) => envChoices.get(env.name)).map((env) => env.name);
  const denylist = [...shownDenylist()];
  busy = true;
  updateControls();
  try {
    await withConfigMutationLock(() =>
      session.setupHarness(offer.offer_id!, envNames, denylist, summaries));
    pendingDenylist = null;
    summariesEdited = false;
    show(`${harnessName(offer.adapter)} is set up. ${agentStateText(session.agentStatus)}`);
  } catch (error) {
    show(`Harness setup did not finish: ${describe(error)}`, true);
    if (error instanceof HostError && error.code === "conflict") await reloadConfig().catch(() => undefined);
  } finally {
    // The host spends an offer on every attempt, including a cancel.
    clearOffers();
    busy = false;
    renderConfig();
  }
}

async function removeHarness(): Promise<void> {
  if (busy || !session.config?.agent) return;
  busy = true;
  updateControls();
  try {
    await withConfigMutationLock(() => session.saveConfig({ ...session.config!, agent: null }, null));
    show("Harness removed. AI commands are off.");
  } catch (error) {
    show(`Could not remove the harness: ${describe(error)}`, true);
    if (error instanceof HostError && error.code === "conflict") await reloadConfig().catch(() => undefined);
  } finally {
    busy = false;
    renderConfig();
  }
}

/** The host decides whether a change needs its dialog: none to add an
 * exclusion, one to remove it. */
async function saveDenylist(list: string[], done: string): Promise<void> {
  busy = true;
  updateControls();
  try {
    await withConfigMutationLock(() =>
      session.saveConfig({ ...session.config!, agent_denylist: list }, null));
    show(done);
  } catch (error) {
    show(`Could not change AI privacy exclusions: ${describe(error)}`, true);
    if (error instanceof HostError && error.code === "conflict") await reloadConfig().catch(() => undefined);
  } finally {
    busy = false;
    renderConfig();
  }
}

async function addExclusion(): Promise<void> {
  if (busy || !session.connected) return;
  const edit = addDenylistEntry(shownDenylist(), denylistInput.value);
  if (!edit.ok) {
    show(edit.error, true);
    return;
  }
  const entry = edit.list[edit.list.length - 1]!;
  denylistInput.value = "";
  denylistPreview.textContent = "";
  if (denylistLive()) {
    await saveDenylist(edit.list, `AI commands now exclude ${entry} and its subdomains.`);
  } else {
    pendingDenylist = edit.list;
    show(`${entry} will be excluded; harness setup asks you to confirm the list.`);
    renderConfig();
  }
}

async function removeExclusion(entry: string): Promise<void> {
  if (busy || !session.connected) return;
  const list = removeDenylistEntry(shownDenylist(), entry);
  if (denylistLive()) {
    await saveDenylist(list, `AI commands may now read pages on ${entry}.`);
  } else {
    pendingDenylist = list;
    show(`${entry} removed from the list harness setup will confirm.`);
    renderConfig();
  }
}

function refreshDenylistPreview(): void {
  const raw = denylistInput.value;
  if (!raw.trim()) {
    denylistPreview.textContent = "";
  } else {
    const preview = previewDenylistEntry(raw);
    denylistPreview.textContent = preview.ok
      ? `Excludes ${preview.normalized} and its subdomains.`
      : preview.error;
  }
  updateControls();
}

for (const suggestion of SUGGESTED_EXCLUSIONS) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "secondary";
  button.textContent = `${suggestion.label}, e.g. ${suggestion.example}`;
  // Only fills the box; the user edits it and chooses Add.
  button.addEventListener("click", () => {
    denylistInput.value = suggestion.example;
    refreshDenylistPreview();
    denylistInput.focus();
  });
  denylistSuggestions.append(button);
}

detectButton.addEventListener("click", () => void detectHarnesses());
setupButton.addEventListener("click", () => void setupHarness());
removeHarnessButton.addEventListener("click", () => void removeHarness());
denylistAddButton.addEventListener("click", () => void addExclusion());
denylistInput.addEventListener("input", refreshDenylistPreview);
denylistInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") void addExclusion();
});
summariesInput.addEventListener("input", () => { summariesEdited = true; });
chooseFolderButton.addEventListener("click", () => void chooseFolder());
enableButton.addEventListener("click", enableSite);
pauseButton.addEventListener("click", () => void pauseCapture());
siteUrl.addEventListener("input", () => void refreshPreflight());
sitePath.addEventListener("input", () => void refreshPreflight());
// Enable's rollback compares against this page's last permission read, so
// every grant change, from this page or elsewhere, refreshes it.
chrome.permissions.onAdded.addListener(() => void refreshPreflight());
chrome.permissions.onRemoved.addListener(() => {
  void refreshPreflight();
  void pruneNoteOrigins().catch(() => undefined);
});
session.watchPolicy(scheduleReload);
window.addEventListener("pagehide", () => {
  if (reloadTimer !== null) clearTimeout(reloadTimer);
  session.host.disconnect();
});

void (async () => {
  busy = true;
  updateControls();
  try {
    await pruneNoteOrigins().catch(() => undefined);
    let hello: Awaited<ReturnType<ConfigSession["hello"]>>;
    try {
      hello = await session.hello();
    } catch (error) {
      show(`Native host unavailable: ${describe(error)}. Install or register the development host, then reload this page.`, true);
      return;
    }
    try {
      await reloadConfig();
      if (hello.config_issue) show(hello.config_issue, true);
    } catch (error) {
      // The host answered, so this is a settings problem, not a missing host.
      if (session.configIssue) showConfigState();
      else show(`Could not load settings: ${describe(error)}. Reload this page to try again.`, true);
    }
  } finally {
    busy = false;
    await session.refreshStatus().catch(() => undefined);
    updateControls();
  }
})();
