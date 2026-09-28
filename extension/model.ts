import type { SiteConfig, VisitEvent, VisitOutcome } from "../protocol/ts/generated.js";

export const POLICY_LEASE_MS = 24 * 60 * 60 * 1_000;
export const MAX_QUEUED_VISITS = 256;
export const MAX_QUEUE_BYTES = 512 * 1_024;

export interface PolicyLease {
  revision: string;
  expires_at: number;
  capture_enabled: boolean;
  sites: SiteConfig[];
}

export interface QueuedVisit {
  event: VisitEvent;
  dedupe_key: string;
  attempts: number;
  retry_after: number;
}

export interface QueueState {
  version: 1;
  items: QueuedVisit[];
  overflow_count: number;
  rejected_count: number;
  last_error: string | null;
}

export interface WorkerStatus {
  queued: number;
  next_retry_at: number | null;
  overflow_count: number;
  rejected_count: number;
  last_error: string | null;
  policy_expires_at: number | null;
  revoked_origins: string[];
  pause_pending: boolean;
  pause_token: string | null;
  navigation_ready: boolean;
  locally_removed_sites: SiteConfig[];
}

export type WorkerRequest =
  | { kind: "install_policy"; lease: PolicyLease; resume_after_confirmation: boolean; resume_after_pause_token: string | null }
  | { kind: "suspend_policy" }
  | { kind: "pause_capture" }
  | { kind: "get_pending_ids" }
  | { kind: "discard_pending"; event_ids: string[] }
  | { kind: "remove_site"; site: SiteConfig }
  | { kind: "get_pending" }
  | { kind: "ack_visit"; event_id: string; outcome: VisitOutcome; reason: string | null }
  | { kind: "get_status" }
  | { kind: "ack_reenabled_origin"; origin: string; revision: string }
  | { kind: "ack_revocations"; origins: string[] };

export interface WorkerReply<T> {
  ok: boolean;
  value: T | null;
  error: string | null;
}

export function emptyQueue(): QueueState {
  return {
    version: 1,
    items: [],
    overflow_count: 0,
    rejected_count: 0,
    last_error: null,
  };
}

export function exactOriginPattern(origin: string): string {
  const url = new URL(origin);
  if (!isHttpUrl(url) || url.username || url.password || url.origin !== origin) {
    throw new Error("Enter an exact HTTP(S) site origin");
  }
  // Chrome treats an omitted port as a wildcard. Spell out default ports too,
  // so the browser grant matches the host's exact origin policy.
  const port = url.port || (url.protocol === "https:" ? "443" : "80");
  return `${url.protocol}//${url.hostname}:${port}/*`;
}

export function isHttpUrl(url: URL): boolean {
  return url.protocol === "http:" || url.protocol === "https:";
}

export function matchingSite(lease: PolicyLease, rawUrl: string): SiteConfig | null {
  if (!lease.capture_enabled || lease.expires_at <= Date.now()) return null;
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    return null;
  }
  if (!isHttpUrl(url) || url.username || url.password) return null;
  return (
    lease.sites.find((site) => siteMatchesUrl(site, url)) ?? null
  );
}

export function siteMatchesUrl(site: SiteConfig, url: URL): boolean {
  if (site.origin !== url.origin) return false;
  const prefix = site.path_prefix;
  if (prefix === "/") return true;
  if (prefix.endsWith("/")) return url.pathname.startsWith(prefix);
  return url.pathname === prefix || url.pathname.startsWith(`${prefix}/`);
}

export function isPolicyLease(value: unknown): value is PolicyLease {
  if (typeof value !== "object" || value === null) return false;
  const lease = value as Record<string, unknown>;
  return (
    typeof lease.revision === "string" &&
    lease.revision.length > 0 &&
    typeof lease.expires_at === "number" &&
    Number.isFinite(lease.expires_at) &&
    typeof lease.capture_enabled === "boolean" &&
    Array.isArray(lease.sites) &&
    lease.sites.every(
      (site: unknown) =>
        typeof site === "object" &&
        site !== null &&
        typeof (site as Record<string, unknown>).origin === "string" &&
        typeof (site as Record<string, unknown>).path_prefix === "string",
    )
  );
}

export function isQueuedVisit(value: unknown): value is QueuedVisit {
  if (typeof value !== "object" || value === null) return false;
  const item = value as Record<string, unknown>;
  const event = item.event;
  if (typeof event !== "object" || event === null) return false;
  const visit = event as Record<string, unknown>;
  return (
    typeof item.dedupe_key === "string" &&
    typeof item.attempts === "number" &&
    Number.isInteger(item.attempts) &&
    item.attempts >= 0 &&
    typeof item.retry_after === "number" &&
    Number.isFinite(item.retry_after) &&
    typeof visit.event_id === "string" &&
    typeof visit.url === "string" &&
    (visit.title === null || typeof visit.title === "string") &&
    typeof visit.occurred_at === "string" &&
    visit.incognito === false
  );
}
