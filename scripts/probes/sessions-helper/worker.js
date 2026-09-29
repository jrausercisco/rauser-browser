// Sessions helper for the discard-restore probe. It holds the "sessions" and
// "tabs" permissions the recorder must not have, and does only what a user's
// "Reopen closed tab" does: close a tab, then restore the most recently closed
// one.

globalThis.helper = {
  reopen: async (tabId) => {
    await chrome.tabs.remove(tabId);
    // Chrome records the closed tab asynchronously.
    await new Promise((resolve) => setTimeout(resolve, 800));
    const session = await chrome.sessions.restore();
    return { tabId: session.tab?.id ?? null, url: session.tab?.url ?? null, window: session.window?.id ?? null };
  },
};
