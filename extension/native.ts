import type { ErrorCode, Request, Response } from "../protocol/ts/generated.js";

export const NATIVE_HOST_NAME = "com.rauser.browser";
export const PROTOCOL_VERSION: Request["protocol_version"] = 2;

export function newRequestId(): string {
  return crypto.randomUUID();
}

export class HostError extends Error {
  constructor(
    readonly code: ErrorCode | "disconnected" | "timeout" | "invalid_response",
    message: string,
  ) {
    super(message);
    this.name = "HostError";
  }
}

interface Pending {
  resolve(response: Response): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
}

const ERROR_CODES: ReadonlySet<string> = new Set([
  "invalid_request", "unsupported_protocol_version", "invalid_config", "not_configured",
  "unauthorized", "conflict", "message_too_large", "internal", "cancelled",
]);

function stringOrNull(value: unknown): boolean {
  return value === null || typeof value === "string";
}

function isConfig(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const config = value as Record<string, unknown>;
  const storage = config.storage;
  if (storage !== null) {
    if (typeof storage !== "object" || storage === null) return false;
    const record = storage as Record<string, unknown>;
    if (!["root", "profile", "log_dir", "pages_dir", "later_dir"]
      .every((key) => typeof record[key] === "string")) return false;
  }
  return typeof config.capture_enabled === "boolean" &&
    Array.isArray(config.sites) &&
    config.sites.every((site: unknown) =>
      typeof site === "object" && site !== null &&
      typeof (site as Record<string, unknown>).origin === "string" &&
      typeof (site as Record<string, unknown>).path_prefix === "string") &&
    Array.isArray(config.strip_params) &&
    config.strip_params.every((value: unknown) => typeof value === "string") &&
    typeof config.near_repeat_secs === "number" &&
    Number.isInteger(config.near_repeat_secs);
}

function isResponseShape(value: Record<string, unknown>): boolean {
  switch (value.type) {
    case "error":
      return typeof value.code === "string" && ERROR_CODES.has(value.code) &&
        typeof value.message === "string";
    case "hello_result":
      return typeof value.host_version === "string" &&
        typeof value.configured === "boolean" && stringOrNull(value.config_issue);
    case "config_result":
      return typeof value.revision === "string" && isConfig(value.config) &&
        stringOrNull(value.config_issue);
    case "config_updated":
      return typeof value.revision === "string" && isConfig(value.config);
    case "folder_chosen":
      return typeof value.path === "string" && typeof value.picker_token === "string";
    case "config_confirmed":
      return typeof value.consent_token === "string" && typeof value.summary === "string";
    case "visit_recorded":
      return typeof value.event_id === "string" &&
        ["persisted", "suppressed", "rejected", "retryable"].includes(String(value.outcome)) &&
        stringOrNull(value.reason) && stringOrNull(value.relative_path);
    case "page_note_result":
      return ["created", "already_present", "conflict", "created_with_warning"]
        .includes(String(value.outcome)) &&
        stringOrNull(value.relative_path) && stringOrNull(value.message);
    default:
      return false;
  }
}

export class HostClient {
  private readonly port: ChromePort;
  private readonly pending = new Map<string, Pending>();
  private closed = false;

  constructor() {
    this.port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    this.port.onMessage.addListener((message) => this.onMessage(message));
    this.port.onDisconnect.addListener(() => this.onDisconnect());
  }

  async call<K extends Exclude<Response["type"], "error">>(
    request: Request,
    expected: K,
    timeoutMs = 30_000,
  ): Promise<Extract<Response, { type: K }>> {
    const response = await this.send(request, timeoutMs);
    if (response.type === "error") {
      throw new HostError(response.code, response.message);
    }
    if (response.type !== expected) {
      throw new HostError(
        "invalid_response",
        `Native host returned ${response.type} instead of ${expected}`,
      );
    }
    return response as Extract<Response, { type: K }>;
  }

  disconnect(): void {
    if (this.closed) return;
    this.closed = true;
    this.port.disconnect();
    this.rejectAll(new HostError("disconnected", "Native host connection closed"));
  }

  private send(request: Request, timeoutMs: number): Promise<Response> {
    if (this.closed) {
      return Promise.reject(new HostError("disconnected", "Native host is not connected"));
    }
    return new Promise<Response>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(request.request_id);
        reject(new HostError("timeout", "Native host did not respond in time"));
      }, timeoutMs);
      this.pending.set(request.request_id, { resolve, reject, timer });
      try {
        this.port.postMessage(request);
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(request.request_id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  private onMessage(message: unknown): void {
    if (typeof message !== "object" || message === null) return;
    const record = message as Record<string, unknown>;
    if (typeof record.request_id !== "string") return;
    const pending = this.pending.get(record.request_id);
    if (!pending) return;
    this.pending.delete(record.request_id);
    clearTimeout(pending.timer);
    if (record.protocol_version !== PROTOCOL_VERSION || !isResponseShape(record)) {
      pending.reject(new HostError("invalid_response", "Native host protocol response is invalid"));
      return;
    }
    pending.resolve(message as Response);
  }

  private onDisconnect(): void {
    if (this.closed) return;
    this.closed = true;
    const detail = chrome.runtime.lastError?.message ?? "Native host disconnected";
    this.rejectAll(new HostError("disconnected", detail));
  }

  private rejectAll(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
}
