// M1.5a probe recorder (DESIGN.md §12.2 step 6.1). It records what Chrome
// delivers to a worker with Brauser's permissions: webNavigation, idle,
// storage, alarms, activeTab, and contextMenus, but no "tabs". The fixture
// origin is a static host permission here, where Brauser uses an optional
// exact-origin grant; both give the worker host access to that origin.
//
// Every listener is registered synchronously at top level, so Chrome can wake
// a stopped worker for any of them. Each event becomes one record, {seq, bootId,
// at, kind, details, extra}, numbered and timestamped when it arrives. One
// promise chain then handles records in arrival order, like serialize() in
// extension/worker.ts. It updates the recorder's tab map and appends the record
// to the storage.local log, so no write is lost to a race.

const LOG_KEY = "log";
const MAP_KEY = "tabmap";
const bootedAt = Date.now();
const bootId = crypto.randomUUID();
const memory = [];
let seq = 0;
let chain = Promise.resolve();

function enqueue(task) {
  chain = chain.then(task).catch((error) => {
    // A failed write must not stop later records. Keep the error visible in
    // memory, where probes can see it.
    memory.push({ seq: -1, bootId, at: Date.now(), kind: "recorder.error", details: { message: String(error) }, extra: {} });
  });
  return chain;
}

async function append(entry) {
  const { [LOG_KEY]: log = [] } = await chrome.storage.local.get(LOG_KEY);
  log.push(entry);
  await chrome.storage.local.set({ [LOG_KEY]: log });
}

async function tabMap() {
  const { [MAP_KEY]: map = {} } = await chrome.storage.session.get(MAP_KEY);
  return map;
}

async function mapSet(tabId, url) {
  const map = await tabMap();
  map[tabId] = url;
  await chrome.storage.session.set({ [MAP_KEY]: map });
}

// prepare runs inside the chain before the append. It may return extra fields
// for the record, and it sees the map exactly as earlier events left it.
function record(kind, details, prepare = null) {
  const entry = { seq: seq++, bootId, at: Date.now(), kind, details, extra: {} };
  memory.push(entry);
  return enqueue(async () => {
    if (prepare) entry.extra = (await prepare()) ?? {};
    await append(entry);
  });
}

// The recorder's own tab-to-URL map, a stand-in for Brauser's §5.1 map. It is
// updated only from main-frame active commits and SPA route changes, so a
// probe can see what an at-event-time source lookup would have returned.
const mainFrame = (details) => details.frameId === 0;
const nav = chrome.webNavigation;
nav.onBeforeNavigate.addListener((details) => record("onBeforeNavigate", details));
nav.onCreatedNavigationTarget.addListener((details) => record("onCreatedNavigationTarget", details,
  async () => ({ sourceMapped: (await tabMap())[details.sourceTabId] ?? null })));
nav.onCommitted.addListener((details) => record("onCommitted", details, async () => {
  if (mainFrame(details) && details.documentLifecycle === "active") await mapSet(details.tabId, details.url);
}));
nav.onHistoryStateUpdated.addListener((details) => record("onHistoryStateUpdated", details, async () => {
  if (mainFrame(details)) await mapSet(details.tabId, details.url);
}));
nav.onReferenceFragmentUpdated.addListener((details) => record("onReferenceFragmentUpdated", details));
nav.onCompleted.addListener((details) => record("onCompleted", details));
nav.onErrorOccurred.addListener((details) => record("onErrorOccurred", details));
nav.onTabReplaced.addListener((details) => record("onTabReplaced", details));

// Without "tabs", tab objects carry url and title only for granted origins,
// so they are copied as delivered.
chrome.tabs.onActivated.addListener((info) => record("tabs.onActivated", info));
chrome.tabs.onCreated.addListener((tab) => record("tabs.onCreated", { tab }));
chrome.tabs.onRemoved.addListener((tabId, info) => record("tabs.onRemoved", { tabId, ...info }));
chrome.tabs.onReplaced.addListener((addedTabId, removedTabId) => record("tabs.onReplaced", { addedTabId, removedTabId }));
chrome.tabs.onUpdated.addListener((tabId, change, tab) => record("tabs.onUpdated", { tabId, change, tab }));
chrome.windows.onFocusChanged.addListener((windowId) => record("windows.onFocusChanged", { windowId }));
chrome.idle.onStateChanged.addListener((state) => record("idle.onStateChanged", { state }));
chrome.runtime.onStartup.addListener(() => record("runtime.onStartup", {}));
chrome.runtime.onInstalled.addListener((details) => record("runtime.onInstalled", details));

// The runner wakes a stopped worker with this message when a probe asks it to.
// It is not recorded, so waking adds nothing to the log.
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.probe !== "wake") return false;
  sendResponse({ bootId });
  return false;
});

record("worker.boot", { bootedAt });

// Chrome's idle calls throw synchronously for bad values, so report rather
// than throw, and let a probe record exactly what Chrome said.
function attempt(action) {
  try {
    return Promise.resolve(action()).then((value) => ({ ok: true, value: value ?? null }),
      (error) => ({ ok: false, error: String(error?.message ?? error) }));
  } catch (error) {
    return Promise.resolve({ ok: false, error: String(error?.message ?? error) });
  }
}

const tabSummary = (tab) => tab && {
  id: tab.id, windowId: tab.windowId, index: tab.index, active: tab.active,
  discarded: tab.discarded, status: tab.status, url: tab.url ?? null, title: tab.title ?? null,
};

// Helpers the runner and probes call through Runtime.evaluate.
globalThis.probe = {
  bootId,
  bootedAt,
  memory,
  flush: () => chain.then(() => memory.length),
  clearLog: () => enqueue(() => chrome.storage.local.set({ [LOG_KEY]: [] })).then(() => { memory.length = 0; }),
  tabMap,
  tabs: () => chrome.tabs.query({}).then((tabs) => tabs.map(tabSummary)),
  discard: (tabId) => attempt(() => chrome.tabs.discard(tabId).then(tabSummary)),
  permissions: () => chrome.permissions.getAll(),
  sessionSet: (items) => chrome.storage.session.set(items),
  sessionGet: (keys = null) => chrome.storage.session.get(keys),
  localSet: (items) => chrome.storage.local.set(items),
  localGet: (keys = null) => chrome.storage.local.get(keys),
  setIdleInterval: (seconds) => attempt(() => chrome.idle.setDetectionInterval(seconds)),
  queryIdle: (seconds) => attempt(() => chrome.idle.queryState(seconds)),
  attempt,
};
