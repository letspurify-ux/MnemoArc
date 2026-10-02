const EMPTY = Object.freeze({ text: "" });

// Keep session drafts in memory across chat mounts. Only the composer listens
// to edits, so typing does not rerender the whole workspace. Snapshot identity
// protects edits made after a send, including edits in a remounted chat.
export function createComposerDrafts() {
  const drafts = new Map(),
    listeners = new Set();
  const notify = () => listeners.forEach((listener) => listener());
  const get = (id) => drafts.get(id) || EMPTY;
  return {
    get,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    update(id, patch) {
      const previous = get(id),
        next = { text: patch.text ?? previous.text };
      if (previous.text === next.text) return;
      drafts.set(id, next);
      notify();
    },
    clearIfUnchanged(id, submitted) {
      if (drafts.get(id) !== submitted) return;
      drafts.delete(id);
      notify();
    },
    retain(ids) {
      const remaining = new Set(ids);
      let changed = false;
      for (const id of drafts.keys()) {
        if (!remaining.has(id)) {
          drafts.delete(id);
          changed = true;
        }
      }
      if (changed) notify();
    },
    hasText() {
      return [...drafts.values()].some((draft) => draft.text.trim());
    },
  };
}
