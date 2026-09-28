import type { ConfigSnapshot, SiteConfig, StorageConfig } from "../protocol/ts/generated.js";
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
import { forgetNoteOrigin, noteOrigins, pruneNoteOrigins } from "./grants.js";

const status = element<HTMLDivElement>("status");
const folderPath = element<HTMLOutputElement>("folder-path");
const chooseFolderButton = element<HTMLButtonElement>("choose-folder");
const siteUrl = element<HTMLInputElement>("site-url");
const sitePath = element<HTMLInputElement>("site-path");
const enableButton = element<HTMLButtonElement>("enable-site");
const pauseButton = element<HTMLButtonElement>("pause-capture");
const sitesList = element<HTMLUListElement>("sites-list");

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

// The host binds a folder selection to the revision it was chosen at, so any
// save, from this page or another, makes a pending selection unusable.
const session = new ConfigSession(renderConfig, () => { picker = null; });

function show(message: string, warning = false): void {
  status.textContent = message;
  status.classList.toggle("warning", warning);
}

function showConfigState(): void {
  const problem = setupProblem(session.config, session.configIssue);
  if (session.configIssue) show(problem!, true);
  else if (problem) show(`${problem} Choose a folder and enable a site to begin.`);
  else show("Capture is enabled for the listed sites.");
}

function updateControls(): void {
  const connected = session.connected;
  chooseFolderButton.disabled = !connected || busy;
  enableButton.disabled = !connected || busy || preflight === null ||
    (!picker && !session.config?.storage);
  pauseButton.disabled = !connected || busy || session.config?.capture_enabled !== true;
  for (const button of sitesList.querySelectorAll("button")) {
    button.disabled = !connected || busy;
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
  updateControls();
}

async function reloadConfig(): Promise<void> {
  await session.reload();
  void refreshPreflight();
  showConfigState();
  if (session.status?.pause_pending && session.config?.capture_enabled) {
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

async function chooseFolder(): Promise<void> {
  busy = true;
  updateControls();
  try {
    const response = await session.host.call({
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
  origin: string; pattern: string; apiGranted: boolean; originGranted: boolean;
}): Promise<void> {
  if (grant.originGranted && grant.apiGranted) return;
  const latest = await session.readHostConfig();
  if (latest.config_issue) {
    throw new Error("The host configuration needs repair; review Chrome access in extension settings");
  }
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
  if (!picker && !currentConfig.storage) {
    show("Choose a notes folder first.", true);
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
          ? {
              root: selection.path,
              profile: "neutral",
              log_dir: config.storage?.log_dir ?? "log",
              pages_dir: config.storage?.pages_dir ?? "pages",
              later_dir: config.storage?.later_dir ?? "later",
            }
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
  if (!session.config) return;
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
  if (busy || !baseConfig || !session.revision ||
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
