// The lock covers both reading host state and publishing the worker lease.
// Locking only the write lets an older panel restore a removed site.
export async function withLatestHostState<T>(
  lock: <R>(action: () => Promise<R>) => Promise<R>,
  read: () => Promise<T>,
  apply: (state: T) => Promise<void>,
): Promise<T> {
  return lock(async () => {
    const state = await read();
    await apply(state);
    return state;
  });
}

export async function finishHostPause<T extends { config: { capture_enabled: boolean }; config_issue: string | null }>(
  read: () => Promise<T>,
  pauseLocally: () => Promise<void>,
  saveDisabled: (state: T) => Promise<void>,
  installDisabled: (state: T) => Promise<void>,
  isConflict: (error: unknown) => boolean,
): Promise<void> {
  // A competing panel may have enabled capture while the native consent UI
  // was open. Re-read the host after entering the shared lock, then retry a
  // stale revision instead of leaving only the worker paused.
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const state = await read();
    await pauseLocally();
    if (state.config_issue) throw new Error(state.config_issue);
    if (!state.config.capture_enabled) {
      await installDisabled(state);
      return;
    }
    try {
      await saveDisabled(state);
      return;
    } catch (error) {
      if (!isConflict(error) || attempt === 2) throw error;
    }
  }
}
