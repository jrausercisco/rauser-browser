import type {
  ErrorCode, PROTOCOL_VERSION as GENERATED_PROTOCOL_VERSION, Request, Response,
} from "../protocol/ts/generated.js";
import { NATIVE_HOST_NAME } from "./brand.js";
import { isResponseShape } from "./protocol-shape.js";

// A value import would pull the generated module out of the flat dist layout;
// this annotation fails typecheck whenever the generated version changes.
export const PROTOCOL_VERSION: typeof GENERATED_PROTOCOL_VERSION = 4;

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

/** A version mismatch is the one error a host out of step with this build
 * must still be able to report, whatever `protocol_version` it carries. */
function isVersionMismatch(value: Record<string, unknown>): boolean {
  return value.type === "error" && value.code === "unsupported_protocol_version" &&
    typeof value.message === "string";
}

/**
 * One page's connection to the native host. Chrome runs one host process per
 * port, and the port closes for good when that process exits, so the next
 * call after a disconnect opens a new port. Only {@link disconnect} closes the
 * client permanently.
 */
export class HostClient {
  private port: ChromePort | null = null;
  private readonly pending = new Map<string, Pending>();
  private readonly listeners: Array<() => void> = [];
  private readonly exitListeners: Array<() => void> = [];
  private disposed = false;
  // Whether the current port has delivered any frame.
  private answered = false;
  // False once a port closed before its host answered anything, which means
  // Chrome could not start the host or it failed at once. Set again when a
  // later port's host answers.
  private reachable = true;

  constructor() {
    this.connect();
  }

  /**
   * Whether calls can reach a host: the current port is open, or the last
   * host answered before it exited and the next call will start another.
   */
  get connected(): boolean {
    return !this.disposed && (this.port !== null || this.reachable);
  }

  /** Call `listener` whenever {@link connected} may have changed. */
  onStateChange(listener: () => void): void {
    this.listeners.push(listener);
  }

  /**
   * Call `listener` when a host process's port closes on its own. Any state
   * that process held, such as a folder selection token, is gone; the next
   * call starts a new process.
   */
  onHostExit(listener: () => void): void {
    this.exitListeners.push(listener);
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

  /** Like {@link call}, but the host may legitimately answer with any of
   * several response types (for example `save_note`'s `note_saved` or
   * `note_conflict`). */
  async callAny<K extends Exclude<Response["type"], "error">>(
    request: Request,
    expected: readonly K[],
    timeoutMs = 30_000,
  ): Promise<Extract<Response, { type: K }>> {
    const response = await this.send(request, timeoutMs);
    if (response.type === "error") {
      throw new HostError(response.code, response.message);
    }
    if (!(expected as readonly string[]).includes(response.type)) {
      throw new HostError(
        "invalid_response",
        `Native host returned ${response.type} instead of ${expected.join(" or ")}`,
      );
    }
    return response as Extract<Response, { type: K }>;
  }

  disconnect(): void {
    if (this.disposed) return;
    this.disposed = true;
    const port = this.port;
    this.port = null;
    port?.disconnect();
    this.rejectAll(new HostError("disconnected", "Native host connection closed"));
    this.notify();
  }

  private connect(): ChromePort {
    const port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    this.port = port;
    this.answered = false;
    // Frames and disconnects from an earlier port belong to requests that
    // were already rejected; they must not settle requests on this one.
    port.onMessage.addListener((message) => {
      if (this.port === port) this.onMessage(message);
    });
    port.onDisconnect.addListener(() => {
      if (this.port === port) this.onDisconnect();
    });
    return port;
  }

  private send(request: Request, timeoutMs: number): Promise<Response> {
    if (this.disposed) {
      return Promise.reject(new HostError("disconnected", "Native host is not connected"));
    }
    return new Promise<Response>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(request.request_id);
        reject(new HostError("timeout", "Native host did not respond in time"));
      }, timeoutMs);
      this.pending.set(request.request_id, { resolve, reject, timer });
      try {
        (this.port ?? this.connect()).postMessage(request);
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
    this.answered = true;
    if (!this.reachable) {
      this.reachable = true;
      this.notify();
    }
    // The host answers a frame it could not read, such as an oversized one,
    // with an empty request_id, since it cannot tell which request it was.
    if (record.request_id === "") {
      if (record.type === "error" && record.protocol_version === PROTOCOL_VERSION &&
          isResponseShape(record)) {
        this.rejectAll(new HostError(record.code as ErrorCode, record.message as string));
      }
      return;
    }
    const pending = this.pending.get(record.request_id);
    if (!pending) return;
    this.pending.delete(record.request_id);
    clearTimeout(pending.timer);
    if (isVersionMismatch(record)) {
      pending.resolve(message as Response);
      return;
    }
    if (record.protocol_version !== PROTOCOL_VERSION || !isResponseShape(record)) {
      pending.reject(new HostError("invalid_response", "Native host protocol response is invalid"));
      return;
    }
    pending.resolve(message as Response);
  }

  private onDisconnect(): void {
    // A host that answered and then exited is replaced on the next call. A
    // port that closed before any answer means the host could not start.
    this.reachable = this.answered;
    this.port = null;
    const detail = chrome.runtime.lastError?.message ?? "Native host disconnected";
    this.rejectAll(new HostError("disconnected", detail));
    for (const listener of this.exitListeners) listener();
    this.notify();
  }

  private rejectAll(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }

  private notify(): void {
    for (const listener of this.listeners) listener();
  }
}
