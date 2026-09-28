// Origins the side panel asked Chrome for so notes can read a tab's URL and
// title (DESIGN.md §5.3). Logging uses the same exact-origin grants, so the
// settings page checks this list before revoking a removed site's origin.
import { BINARY_NAME, storageKey } from "./brand.js";
import { exactOriginPattern } from "./model.js";

const NOTE_ORIGINS_KEY = storageKey("note_origins_v1");
const NOTE_ORIGINS_LOCK = `${BINARY_NAME}-note-origins`;

async function readNoteOrigins(): Promise<string[]> {
  const value = (await chrome.storage.local.get(NOTE_ORIGINS_KEY))[NOTE_ORIGINS_KEY];
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];
}

// The panel and settings page may both edit the list; keep each edit atomic.
async function updateNoteOrigins(change: (origins: string[]) => Promise<string[]> | string[]): Promise<void> {
  const edit = async () => {
    const current = await readNoteOrigins();
    const next = await change(current);
    if (next.length !== current.length || next.some((origin, index) => origin !== current[index])) {
      await chrome.storage.local.set({ [NOTE_ORIGINS_KEY]: next });
    }
  };
  if (navigator.locks?.request) await navigator.locks.request(NOTE_ORIGINS_LOCK, { mode: "exclusive" }, edit);
  else await edit();
}

export async function noteOrigins(): Promise<ReadonlySet<string>> {
  return new Set(await readNoteOrigins());
}

/** Call after Chrome grants `origin` for a note. */
export function recordNoteOrigin(origin: string): Promise<void> {
  return updateNoteOrigins((origins) => origins.includes(origin) ? origins : [...origins, origin]);
}

/** Call when a later grant for `origin` is made for logging, not notes. */
export function forgetNoteOrigin(origin: string): Promise<void> {
  return updateNoteOrigins((origins) => origins.filter((entry) => entry !== origin));
}

/**
 * Drop origins Chrome no longer grants, for example after the user removed
 * one in Chrome's extension settings. A stale entry would otherwise keep a
 * later logging grant for the same origin when its site is removed.
 */
export function pruneNoteOrigins(): Promise<void> {
  return updateNoteOrigins(async (origins) => {
    const granted = await Promise.all(origins.map(async (origin) => {
      try {
        return await chrome.permissions.contains({ origins: [exactOriginPattern(origin)] });
      } catch {
        return true; // Keep the entry when Chrome cannot say.
      }
    }));
    return origins.filter((_origin, index) => granted[index]);
  });
}
