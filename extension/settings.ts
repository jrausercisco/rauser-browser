// Host configuration state shared by the side panel and the settings page.
// Each page owns one session; the config mutation lock serializes changes
// across every open page, and the worker's policy lease tells a page when
// another page has saved a different revision.
import type { ConfigSnapshot, SiteConfig } from "../protocol/ts/generated.js";
import {
  POLICY_LEASE_MS,
  POLICY_STORAGE_KEY,
  isPolicyLease,
  type PolicyLease,
  type WorkerReply,
  type WorkerRequest,
  type WorkerStatus,
} from "./model.js";
import { finishHostPause, withLatestHostState } from "./coordination.js";
import { HostClient, HostError, PROTOCOL_VERSION, newRequestId } from "./native.js";

const CONFIG_MUTATION_LOCK = "rauser-config-permissions";

export function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) throw new Error(`Missing page element: ${id}`);
  return found as T;
}

export function describe(error: unknown): string {
  if (error instanceof HostError && error.code === "cancelled") {
    return "Canceled. No settings were changed.";
  }
  return error instanceof Error ? error.message : String(error);
}

export function sameSite(left: SiteConfig, right: SiteConfig): boolean {
  return left.origin === right.origin && left.path_prefix === right.path_prefix;
}

export async function withConfigMutationLock<T>(action: () => Promise<T>): Promise<T> {
  if (!navigator.locks?.request) {
    throw new Error("Chrome cannot coordinate configuration and permission changes on this page");
  }
  return navigator.locks.request(CONFIG_MUTATION_LOCK, { mode: "exclusive" }, action);
}

export async function worker<T>(request: WorkerRequest): Promise<T> {
  const reply = await chrome.runtime.sendMessage<WorkerReply<T>>(request);
  if (!reply || !reply.ok || reply.value === null) {
    throw new Error(reply?.error ?? "Extension worker did not respond");
  }
  return reply.value;
}

export function leaseFor(config: ConfigSnapshot, currentRevision: string): PolicyLease {
  return {
    revision: currentRevision,
    expires_at: Date.now() + POLICY_LEASE_MS,
    capture_enabled: config.capture_enabled,
    sites: config.sites,
  };
}

/** Why capture cannot run yet, or null once setup is complete. */
export function setupProblem(config: ConfigSnapshot | null, issue: string | null): string | null {
  if (issue) return `The host configuration needs repair: ${issue}`;
  if (!config) return null;
  if (!config.storage) return "No notes folder is chosen.";
  if (!config.sites.length) return "No sites are enabled for capture.";
  if (!config.capture_enabled) return "Capture is off.";
  return null;
}

export class ConfigSession {
  readonly host = new HostClient();
  config: ConfigSnapshot | null = null;
  revision: string | null = null;
  configIssue: string | null = null;
  status: WorkerStatus | null = null;
  // While this page pauses, the worker clears the lease before the host
  // commits. That null lease is this page's own write, not another page's.
  private pausing = 0;

  constructor(
    private readonly onChange: () => void,
    // Called whenever the host revision changes, from any page. Anything bound
    // to the previous revision, such as a folder selection, is now stale.
    private readonly onRevisionChange: () => void = () => undefined,
  ) {}

  get connected(): boolean {
    return this.config !== null && this.revision !== null;
  }

  hello() {
    return this.host.call({
      type: "hello", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "hello_result");
  }

  readHostConfig() {
    return this.host.call({
      type: "get_config", protocol_version: PROTOCOL_VERSION, request_id: newRequestId(),
    }, "config_result");
  }

  private setRevision(revision: string): void {
    const changed = this.revision !== null && this.revision !== revision;
    this.revision = revision;
    if (changed) this.onRevisionChange();
  }

  private setHostConfig(response: Awaited<ReturnType<ConfigSession["readHostConfig"]>>): void {
    this.config = response.config;
    this.setRevision(response.revision);
    this.configIssue = response.config_issue;
    this.onChange();
  }

  async applyHostConfig(response: Awaited<ReturnType<ConfigSession["readHostConfig"]>>): Promise<void> {
    this.setHostConfig(response);
    if (response.config_issue) await worker<WorkerStatus>({ kind: "suspend_policy" });
    else await this.installHostPolicy();
  }

  async installHostPolicy(
    resumeAfterConfirmation = false,
    resumeAfterPauseToken: string | null = null,
  ): Promise<void> {
    if (!this.config || !this.revision) return;
    if (!this.config.storage && this.config.capture_enabled) {
      await worker<WorkerStatus>({ kind: "suspend_policy" });
      return;
    }
    await worker<WorkerStatus>({
      kind: "install_policy",
      lease: leaseFor(this.config, this.revision),
      // A disabled config returned by the host proves that a pending local pause
      // reached the host and can now be cleared safely.
      resume_after_confirmation: resumeAfterConfirmation || !this.config.capture_enabled,
      resume_after_pause_token: resumeAfterPauseToken,
    });
  }

  async refreshStatus(): Promise<WorkerStatus> {
    this.status = await worker<WorkerStatus>({ kind: "get_status" });
    this.onChange();
    return this.status;
  }

  /** Read the host and publish its policy, then settle Chrome revocations. */
  async reload(): Promise<Awaited<ReturnType<ConfigSession["readHostConfig"]>>> {
    // Keep the host read and worker lease update together. Otherwise an older
    // page can reinstall a site after another page has removed it.
    const response = await withLatestHostState(
      withConfigMutationLock, () => this.readHostConfig(), (state) => this.applyHostConfig(state));
    await this.reconcileRevocations();
    await this.refreshStatus();
    return response;
  }

  async saveConfig(
    next: ConfigSnapshot,
    pickerToken: string | null,
    onCommitted?: () => void,
    resumeAfterConfirmation = false,
    resumeAfterPauseToken: string | null = null,
  ): Promise<void> {
    if (!this.revision) throw new Error("Host configuration has not loaded");
    const expectedRevision = this.revision;
    // The host shows the exact expansion. Its token binds this snapshot and revision.
    const confirmed = await this.host.call({
      type: "confirm_config",
      protocol_version: PROTOCOL_VERSION,
      request_id: newRequestId(),
      expected_revision: expectedRevision,
      config: next,
      picker_token: pickerToken,
    }, "config_confirmed", 5 * 60_000);
    const updated = await this.host.call({
      type: "update_config",
      protocol_version: PROTOCOL_VERSION,
      request_id: newRequestId(),
      expected_revision: expectedRevision,
      config: next,
      picker_token: pickerToken,
      consent_token: confirmed.consent_token,
    }, "config_updated");
    this.config = updated.config;
    this.setRevision(updated.revision);
    this.configIssue = null;
    onCommitted?.();
    this.onChange();
    await this.installHostPolicy(resumeAfterConfirmation, resumeAfterPauseToken);
    await this.refreshStatus();
  }

  async reconcileRevocations(): Promise<void> {
    if (!this.config || !this.revision) return;
    const state = await worker<WorkerStatus>({ kind: "get_status" });
    if (!state.revoked_origins.length) return;
    const revoked = new Set(state.revoked_origins);
    await withConfigMutationLock(async () => {
      // A different page may have committed since our previous read. Recompute
      // from the host's current revision while holding the same mutation lock.
      const latest = await this.readHostConfig();
      this.setHostConfig(latest);
      if (latest.config_issue) {
        await worker<WorkerStatus>({ kind: "suspend_policy" });
        throw new Error(latest.config_issue);
      }
      const sites = latest.config.sites.filter((site) => !revoked.has(site.origin));
      if (sites.length !== latest.config.sites.length) {
        await this.saveConfig({
          ...latest.config,
          sites,
          capture_enabled: latest.config.capture_enabled && sites.length > 0,
        }, null);
      } else {
        await this.installHostPolicy();
      }
      await worker<WorkerStatus>({ kind: "ack_revocations", origins: state.revoked_origins });
    });
    this.onChange();
  }

  /** Call while holding the config mutation lock. */
  async completePausedHostConfig(): Promise<void> {
    this.pausing += 1;
    try {
      await this.finishPause();
    } finally {
      this.pausing -= 1;
    }
  }

  private async finishPause(): Promise<void> {
    await finishHostPause(
      async () => {
        const latest = await this.readHostConfig();
        this.setHostConfig(latest);
        return latest;
      },
      async () => { await worker<WorkerStatus>({ kind: "pause_capture" }); },
      async (latest) => {
        await this.saveConfig({ ...latest.config, capture_enabled: false }, null);
      },
      async () => { await this.installHostPolicy(); },
      (error) => error instanceof HostError && error.code === "conflict",
    );
  }

  async pauseCapture(): Promise<void> {
    this.pausing += 1;
    try {
      // Stop local buffering immediately, even if the host is unavailable.
      await worker<WorkerStatus>({ kind: "pause_capture" });
      await withConfigMutationLock(() => this.completePausedHostConfig());
    } finally {
      this.pausing -= 1;
    }
  }

  /**
   * Call `onExternalChange` when the worker's lease no longer matches this
   * session, which means another page saved or paused the configuration.
   * Leases this session installs itself carry its own revision and are ignored.
   */
  watchPolicy(onExternalChange: () => void): void {
    chrome.storage.onChanged.addListener((changes, areaName) => {
      if (areaName !== "local" || !(POLICY_STORAGE_KEY in changes)) return;
      const lease = changes[POLICY_STORAGE_KEY]!.newValue;
      if (isPolicyLease(lease)) {
        if (lease.revision !== this.revision) onExternalChange();
      } else if (this.pausing === 0 && this.expectsLease()) {
        onExternalChange();
      }
    });
  }

  private expectsLease(): boolean {
    return this.config !== null && this.revision !== null && this.configIssue === null &&
      (this.config.storage !== null || !this.config.capture_enabled);
  }
}
