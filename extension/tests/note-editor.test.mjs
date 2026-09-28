import assert from "node:assert/strict";
import test from "node:test";

// Run after build:extension.
const { MAX_NOTE_TITLE_BYTES, NoteEditor, clampUtf8 } = await import("../dist/note-editor.js");

// Pending autosave timers must not keep the test process alive.
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (callback, ms, ...args) => {
  const timer = realSetTimeout(callback, ms, ...args);
  timer.unref();
  return timer;
};

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

// Lets queued promise callbacks run.
async function settle() {
  for (let index = 0; index < 20; index += 1) await Promise.resolve();
}

function harness(debounceMs = 60_000) {
  const view = {
    body: "",
    editable: false,
    status: "",
    warning: false,
    unsaved: [],
    readBody() { return this.body; },
    writeBody(body) { this.body = body; },
    setEditable(editable) { this.editable = editable; },
    setStatus(message, warning = false) {
      this.status = message;
      this.warning = warning;
    },
    showUnsaved(entries) { this.unsaved = entries.map((entry) => ({ ...entry })); },
  };
  const calls = [];
  const host = {
    load(url) {
      const pending = deferred();
      calls.push({ kind: "load", url, ...pending });
      return pending.promise;
    },
    save(request) {
      const pending = deferred();
      calls.push({ kind: "save", ...request, ...pending });
      return pending.promise;
    },
  };
  const editor = new NoteEditor(view, host, (error) => error.message, debounceMs);
  // Answers the oldest unanswered host call of `kind`.
  async function next(kind) {
    await settle();
    const call = calls.find((item) => item.kind === kind && !item.answered);
    assert.ok(call, `expected a ${kind} call`);
    call.answered = true;
    return call;
  }
  function type(text) {
    view.body = text;
    editor.edited();
  }
  return { view, calls, editor, next, type };
}

const page = (url, title = url) => ({ url, title });
const loaded = (body, revision = `rev-${body}`) => ({ exists: body !== "", revision, body });

async function open(h, url, body) {
  const done = h.editor.showPage(page(url));
  (await h.next("load")).resolve(loaded(body));
  await done;
}

test("a stale tab switch cannot bind the editor to a page that is no longer active", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("p edited");
  void h.editor.showPage(page("https://a.test/"));
  const pSave = await h.next("save");
  // While P's save is in flight the user moves on to A and then B.
  const final = h.editor.showPage(page("https://b.test/"));
  pSave.resolve({ type: "note_saved", revision: "p2" });
  (await h.next("load")).resolve(loaded("b"));
  await final;
  assert.deepEqual(h.calls.filter((call) => call.kind === "load").map((call) => call.url),
    ["https://p.test/", "https://b.test/"]);
  assert.equal(h.view.body, "b");
  h.type("b edited");
  const save = h.editor.flush();
  const bSave = await h.next("save");
  assert.equal(bSave.url, "https://b.test/");
  assert.equal(bSave.expected_revision, "rev-b");
  bSave.resolve({ type: "note_saved", revision: "b2" });
  await save;
});

test("a late load result for an abandoned page is ignored", async () => {
  const h = harness();
  void h.editor.showPage(page("https://a.test/"));
  const aLoad = await h.next("load");
  const final = h.editor.showPage(page("https://b.test/"));
  aLoad.resolve(loaded("a"));
  await settle();
  // Under B's header, A's note must never become editable, even briefly.
  assert.equal(h.view.editable, false);
  assert.equal(h.view.body, "");
  (await h.next("load")).resolve(loaded("b"));
  await final;
  assert.equal(h.view.body, "b");
  assert.equal(h.view.editable, true);
});

test("switching to an unusable page drops an in-flight load", async () => {
  const h = harness();
  void h.editor.showPage(page("https://a.test/"));
  const aLoad = await h.next("load");
  const final = h.editor.showPage(null);
  assert.equal(h.view.editable, false);
  aLoad.resolve(loaded("a"));
  await final;
  assert.equal(h.view.body, "");
  assert.equal(h.view.editable, false);
});

test("text that fails to save before a tab switch is kept for the user", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("p unsaved");
  const failed = h.editor.flush();
  (await h.next("save")).reject(new Error("Native host is not connected"));
  await failed;
  assert.equal(h.view.warning, true);
  const switched = h.editor.showPage(page("https://a.test/"));
  (await h.next("save")).reject(new Error("Native host is not connected"));
  (await h.next("load")).resolve(loaded("a"));
  await switched;
  assert.equal(h.view.body, "a");
  assert.deepEqual(h.view.unsaved, [{
    url: "https://p.test/",
    text: "p unsaved",
    reason: "Could not save: Native host is not connected",
  }]);
});

test("edits typed during a save are saved once it finishes", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("one");
  void h.editor.flush();
  const first = await h.next("save");
  h.type("one two");
  const second = h.editor.flush();
  await settle();
  assert.equal(h.calls.filter((call) => call.kind === "save").length, 1, "saves never overlap");
  first.resolve({ type: "note_saved", revision: "r1" });
  const followUp = await h.next("save");
  assert.equal(followUp.body, "one two");
  assert.equal(followUp.expected_revision, "r1");
  followUp.resolve({ type: "note_saved", revision: "r2" });
  await second;
  assert.equal(h.view.status, "Saved");
});

test("a save that outlasts the debounce does not claim Saved while edits are pending", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("one");
  void h.editor.flush();
  const first = await h.next("save");
  h.type("one two");
  first.resolve({ type: "note_saved", revision: "r1" });
  await settle();
  assert.notEqual(h.view.status, "Saved");
});

test("a tab switch waits for the in-flight save and saves later edits", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("one");
  void h.editor.flush();
  const first = await h.next("save");
  h.type("one two");
  const switched = h.editor.showPage(page("https://a.test/"));
  first.resolve({ type: "note_saved", revision: "r1" });
  const tail = await h.next("save");
  assert.equal(tail.url, "https://p.test/");
  assert.equal(tail.body, "one two");
  tail.resolve({ type: "note_saved", revision: "r2" });
  (await h.next("load")).resolve(loaded("a"));
  await switched;
  assert.deepEqual(h.view.unsaved, []);
});

test("a conflict keeps edits typed during the save and does not save the server body back", async () => {
  const h = harness(0);
  await open(h, "https://p.test/", "p");
  h.type("mine");
  void h.editor.flush();
  const save = await h.next("save");
  h.type("mine and more");
  save.resolve({ type: "note_conflict", revision: "theirs-rev", body: "theirs" });
  await h.editor.idle();
  await new Promise((resolve) => setTimeout(resolve, 5));
  await settle();
  assert.equal(h.view.body, "theirs");
  assert.deepEqual(h.view.unsaved.map((entry) => entry.text), ["mine and more"]);
  assert.equal(h.calls.filter((call) => call.kind === "save").length, 1);
});

test("a flush with nothing running sends its save before returning", async () => {
  const h = harness();
  await open(h, "https://p.test/", "p");
  h.type("closing");
  // The pagehide handler disconnects from the host right after this call.
  void h.editor.flush();
  assert.equal(h.calls.filter((call) => call.kind === "save").length, 1);
});

test("a failed load is retried for the same page", async () => {
  const h = harness();
  void h.editor.showPage(page("https://p.test/"));
  (await h.next("load")).reject(new Error("No notes folder is chosen."));
  await h.editor.idle();
  assert.equal(h.view.editable, false);
  // A title update for the same page reloads the failed note.
  const again = h.editor.showPage(page("https://p.test/", "New title"));
  (await h.next("load")).resolve(loaded("p"));
  await again;
  assert.equal(h.view.body, "p");
  assert.equal(h.view.editable, true);
  // Once loaded, a repeat of the same page does not reload it.
  await h.editor.showPage(page("https://p.test/"));
  await h.editor.retry();
  await settle();
  assert.equal(h.calls.filter((call) => call.kind === "load").length, 2);
});

test("retry reloads the note after a failed load", async () => {
  const h = harness();
  void h.editor.showPage(page("https://p.test/"));
  (await h.next("load")).reject(new Error("not configured"));
  await h.editor.idle();
  const retried = h.editor.retry();
  (await h.next("load")).resolve(loaded("p"));
  await retried;
  assert.equal(h.view.editable, true);
});

test("long page titles are clamped to the host's byte limit", async () => {
  const h = harness();
  await open(h, "https://p.test/", "");
  void h.editor.showPage(page("https://p.test/", "界".repeat(1_000)));
  h.type("x");
  void h.editor.flush();
  const save = await h.next("save");
  assert.ok(new TextEncoder().encode(save.title).length <= MAX_NOTE_TITLE_BYTES);
  assert.equal(save.title, "界".repeat(682));
  save.resolve({ type: "note_saved", revision: "r" });
});

test("clampUtf8 never splits a character", () => {
  assert.equal(clampUtf8("abc", 2), "ab");
  assert.equal(clampUtf8("aé", 2), "a");
  assert.equal(clampUtf8("a😀", 4), "a");
  assert.equal(clampUtf8("a😀", 5), "a😀");
  assert.equal(clampUtf8("short", 2_048), "short");
});
