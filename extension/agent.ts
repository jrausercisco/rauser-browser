// AI harness settings helpers. Pure: no chrome or DOM access, so the tests
// load this module directly. The host validates every value again (§7.3);
// these only let the settings page explain a problem before sending.
import type { AgentStatus, ConfigSnapshot, HarnessOffer } from "../protocol/ts/generated.js";

const MAX_HOSTNAME_BYTES = 253;
const MAX_LABEL_BYTES = 63;

export type DenylistPreview = { ok: true; normalized: string } | { ok: false; error: string };

/** Category examples the user may start from. Choosing one only fills the
 * input; nothing is ever added to the AI privacy list without an Add click. */
export const SUGGESTED_EXCLUSIONS: readonly { label: string; example: string }[] = Object.freeze([
  Object.freeze({ label: "Banking", example: "bank.example" }),
  Object.freeze({ label: "Email", example: "mail.example" }),
  Object.freeze({ label: "HR", example: "hr.example" }),
  Object.freeze({ label: "Health", example: "health.example" }),
]);

/** The normalized form the host expects: lowercase punycode, no scheme,
 * path, port, wildcard, empty label, or leading or trailing dot, with labels
 * of at most 63 bytes and at most 253 in all. IPv4 and bracketed IPv6 are
 * kept. */
export function previewDenylistEntry(raw: string): DenylistPreview {
  const value = raw.trim();
  if (!value) return { ok: false, error: "Enter a domain, such as bank.example." };
  if (/\s/.test(value)) return { ok: false, error: "A domain cannot contain spaces." };
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(value)) {
    return { ok: false, error: "Enter only the domain, without http:// or https://." };
  }
  if (/[/\\?#@%]/.test(value)) return { ok: false, error: "Enter only the domain, without a path or other URL parts." };
  if (value.includes("*")) return { ok: false, error: "Wildcards are not needed; subdomains are always excluded." };
  if (value.endsWith(".")) return { ok: false, error: "Remove the trailing dot." };
  if (value.startsWith(".")) return { ok: false, error: "Remove the leading dot; subdomains are always excluded." };
  if (value.startsWith("[")) {
    if (!/^\[[0-9a-f:.]+\]$/i.test(value)) return { ok: false, error: "Enter an IPv6 address as [address], without a port." };
  } else if (value.includes(":")) {
    return { ok: false, error: "Enter the domain without a port or scheme." };
  }
  let normalized: string;
  try {
    normalized = new URL(`http://${value}`).hostname;
  } catch {
    return { ok: false, error: "That is not a valid domain." };
  }
  if (!normalized || normalized.endsWith(".")) return { ok: false, error: "That is not a valid domain." };
  // URL parsing keeps empty and overlong labels; the host rejects them
  // (host/src/privacy.rs). Punycode is ASCII, so length is bytes.
  if (!normalized.startsWith("[")) {
    const labels = normalized.split(".");
    if (labels.some((label) => !label)) return { ok: false, error: "Remove the empty part between two dots." };
    if (labels.some((label) => label.length > MAX_LABEL_BYTES)) {
      return { ok: false, error: `Each part of a domain can be at most ${MAX_LABEL_BYTES} characters.` };
    }
    if (normalized.length > MAX_HOSTNAME_BYTES) {
      return { ok: false, error: `A domain can be at most ${MAX_HOSTNAME_BYTES} characters.` };
    }
  }
  return { ok: true, normalized };
}

export type DenylistEdit = { ok: true; list: string[] } | { ok: false; error: string };

/** A new list with the entry appended, or why it cannot be added. */
export function addDenylistEntry(list: readonly string[], raw: string): DenylistEdit {
  const preview = previewDenylistEntry(raw);
  if (!preview.ok) return preview;
  if (list.includes(preview.normalized)) {
    return { ok: false, error: `${preview.normalized} is already excluded.` };
  }
  return { ok: true, list: [...list, preview.normalized] };
}

/** Whether exclusion edits go to the host at once. Only setup's native
 * confirmation sets `agent_denylist_confirmed`; before it, edits stay on the
 * settings page and setup sends them. After it they are live even with no
 * harness: removing the harness keeps the confirmation, and the host still
 * asks before a removal (§7.3). */
export function denylistEditsLive(config: ConfigSnapshot | null): boolean {
  return config?.agent_denylist_confirmed === true;
}

/** DESIGN §14: Codex runs with the user's own HOME and CODEX_HOME. The host's
 * setup confirmation says the same (host/src/consent.rs CODEX_DISCLOSURE). */
export const CODEX_DISCLOSURE =
  "Codex runs with your normal Codex home and login, so your own Codex instructions (AGENTS.md) and skills can shape its answers. Claude Code is the recommended harness.";

/** The note under a detected harness: whether it can be set up and, for
 * Codex, what sharing the user's Codex home means. */
export function harnessOfferNote(offer: HarnessOffer): string {
  const ready = offer.offer_id !== null && offer.refusal === null;
  const state = ready ? "Ready to set up." : `Cannot be set up: ${offer.refusal ?? "detect again"}.`;
  return offer.adapter === "codex" ? `${state} ${CODEX_DISCLOSURE}` : state;
}

export function removeDenylistEntry(list: readonly string[], entry: string): string[] {
  return list.filter((value) => value !== entry);
}

/** What Detect harnesses reports for a ready offer. Set up harness needs a
 * notes folder, so without one this says to choose it first. */
export function harnessFoundText(name: string, version: string | null, hasNotesFolder: boolean): string {
  const found = `Found ${name}${version ? ` ${version}` : ""}.`;
  return hasNotesFolder
    ? `${found} Review it below, then choose Set up harness.`
    : `${found} Choose a notes folder first, then set up the harness.`;
}

/** The settings page's one-line AI state. Capture setup is described
 * separately (settings.ts setupProblem); this never blocks capture. */
export function agentStateText(status: AgentStatus | null): string {
  if (!status) return "AI harness status is unavailable.";
  const version = status.harness_version ? ` (version ${status.harness_version})` : "";
  switch (status.state) {
    case "ready":
      return `AI harness ready${version}.`;
    case "not_set_up":
      return "No AI harness is set up. AI commands are off.";
    case "denylist_unconfirmed":
      return "AI commands are off until harness setup confirms the AI privacy list.";
    case "harness_problem":
      return `AI commands are off${version}: ${status.message ?? "the harness needs setup again"}`;
  }
}
