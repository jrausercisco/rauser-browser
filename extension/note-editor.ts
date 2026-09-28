// Page note editor state for the side panel (§5.3, §5.7). This module has no
// DOM or Chrome dependencies so the tests can drive it directly.
//
// Every load and save runs on one promise chain, so at most one host request
// is in flight and the note the editor is bound to cannot change under a
// running save. Each page change bumps a refresh token; a queued or in-flight
// switch whose token is no longer current does nothing further, so only the
// latest active tab's note is loaded into the editor.

/** Matches MAX_TITLE_BYTES in host/src/note.rs and the save_note schema. */
export const MAX_NOTE_TITLE_BYTES = 2_048;

/** Cuts `text` to at most `maxBytes` of UTF-8 without splitting a character. */
export function clampUtf8(text: string, maxBytes: number): string {
  let bytes = 0;
  let end = 0;
  for (const char of text) {
    const code = char.codePointAt(0)!;
    // A lone surrogate encodes as U+FFFD, which is also 3 bytes.
    const size = code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    if (bytes + size > maxBytes) break;
    bytes += size;
    end += char.length;
  }
  return text.slice(0, end);
}

/** A page whose note the panel should show, or null for no usable page. */
export type NotePage = { url: string; title: string } | null;

/** Text the host would not take, kept so the user can copy it back in. */
export interface UnsavedText {
  url: string;
  text: string;
  reason: string;
}

export interface NoteView {
  readBody(): string;
  writeBody(body: string): void;
  setEditable(editable: boolean): void;
  setStatus(message: string, warning?: boolean): void;
  showUnsaved(entries: readonly UnsavedText[]): void;
}

export interface NoteHost {
  load(url: string): Promise<{ exists: boolean; revision: string; body: string }>;
  save(request: { url: string; title: string; body: string; expected_revision: string }): Promise<
    { type: "note_saved"; revision: string } | { type: "note_conflict"; revision: string; body: string }
  >;
}

export class NoteEditor {
  private queue: Promise<void> = Promise.resolve();
  private pending = 0;
  private target: NotePage = null;
  private token = 0;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly unsaved: UnsavedText[] = [];
  // The note the editor is bound to. Only queued work changes these.
  private url: string | null = null;
  private title = "";
  private revision: string | null = null;
  private loaded = false;
  private loadFailed = false;
  private dirty = false;
  private saveError = "";

  constructor(
    private readonly view: NoteView,
    private readonly host: NoteHost,
    private readonly describe: (error: unknown) => string,
    private readonly debounceMs = 1_000,
  ) {}

  /** Resolves once all load and save work queued so far has finished. */
  idle(): Promise<void> {
    return this.queue;
  }

  /** Called on every edit of the note text. */
  edited(): void {
    if (!this.loaded) return;
    this.dirty = true;
    this.view.setStatus("");
    this.clearTimer();
    this.saveTimer = setTimeout(() => {
      this.saveTimer = null;
      void this.flush();
    }, this.debounceMs);
  }

  /** Saves pending edits now, after any load or save already running. */
  flush(): Promise<void> {
    this.clearTimer();
    return this.enqueue(() => this.saveNow());
  }

  /** Follows the active tab. Saves the note being left before loading the new
   * one, and retries a failed load when asked for the same page again. */
  showPage(page: NotePage): Promise<void> {
    if (page !== null) page = { url: page.url, title: clampUtf8(page.title, MAX_NOTE_TITLE_BYTES) };
    const samePage = page?.url === this.target?.url;
    this.target = page;
    if (samePage) {
      if (page !== null && this.url === page.url) this.title = page.title;
      const retry = page !== null && this.url === page.url && this.loadFailed;
      if (!retry) return this.queue;
    }
    const token = ++this.token;
    // Stop edits to the old page's text while its header is gone; what was
    // already typed is saved by the switch below.
    this.view.setEditable(false);
    this.clearTimer();
    return this.enqueue(() => this.switchTo(page, token));
  }

  /** Loads the current page's note again if the last load failed, for
   * example after the host configuration changed. */
  retry(): Promise<void> {
    return this.showPage(this.target);
  }

  private enqueue(work: () => Promise<void>): Promise<void> {
    // Start at once when nothing is running, so a flush from `pagehide` sends
    // its request before the panel disconnects from the host.
    this.pending += 1;
    const next = this.pending === 1 ? work() : this.queue.then(work);
    // Work handles its own errors; keep the chain alive regardless.
    this.queue = next.catch(() => undefined).finally(() => {
      this.pending -= 1;
    });
    return this.queue;
  }

  private clearTimer(): void {
    if (this.saveTimer !== null) {
      clearTimeout(this.saveTimer);
      this.saveTimer = null;
    }
  }

  private keepUnsaved(url: string, text: string, reason: string): void {
    if (text === "") return;
    this.unsaved.push({ url, text, reason });
    this.view.showUnsaved(this.unsaved);
  }

  private async switchTo(page: NotePage, token: number): Promise<void> {
    if (token !== this.token) return; // A newer page change will run next.
    await this.saveNow();
    if (this.dirty && this.url !== null) {
      // The save failed. Never drop the text: keep it for the user to copy.
      this.keepUnsaved(this.url, this.view.readBody(), this.saveError);
    }
    this.url = page?.url ?? null;
    // The tab's title may have changed while this switch waited its turn.
    this.title = page !== null && this.target?.url === page.url ? this.target.title : page?.title ?? "";
    this.revision = null;
    this.loaded = false;
    this.loadFailed = false;
    this.dirty = false;
    this.view.setEditable(false);
    this.view.writeBody("");
    this.view.setStatus("");
    if (page === null || token !== this.token) return;
    try {
      const response = await this.host.load(page.url);
      if (token !== this.token) return;
      this.revision = response.revision;
      this.view.writeBody(response.body);
      this.loaded = true;
      this.view.setEditable(true);
      this.view.setStatus(response.exists ? "Saved" : "");
    } catch (error) {
      if (token !== this.token) return;
      this.loadFailed = true;
      this.view.setStatus(`Could not load this page's note: ${this.describe(error)}`, true);
    }
  }

  private async saveNow(): Promise<void> {
    this.clearTimer();
    if (!this.dirty || !this.loaded || this.url === null || this.revision === null) return;
    const url = this.url;
    const body = this.view.readBody();
    this.dirty = false;
    this.view.setStatus("Saving…");
    let response: Awaited<ReturnType<NoteHost["save"]>>;
    try {
      response = await this.host.save({
        url, title: this.title, body, expected_revision: this.revision,
      });
    } catch (error) {
      this.dirty = true; // Retry on the next edit, tab change, or close.
      this.saveError = `Could not save: ${this.describe(error)}`;
      this.view.setStatus(this.saveError, true);
      return;
    }
    this.revision = response.revision;
    if (response.type === "note_saved") {
      // Edits typed during the save are still pending; their timer saves them.
      if (!this.dirty) this.view.setStatus("Saved");
      return;
    }
    // The note changed elsewhere since this panel last loaded it. Nothing was
    // written; show the newer note and keep everything typed, including edits
    // made while this save was in flight (§4.4, §5.3).
    this.clearTimer();
    this.dirty = false;
    this.keepUnsaved(url, this.view.readBody(), "This note changed elsewhere, so your edit was not saved.");
    this.view.writeBody(response.body);
    this.view.setStatus("Could not save; this note changed elsewhere.", true);
  }
}
