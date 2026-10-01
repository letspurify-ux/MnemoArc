// Retain a few recently viewed sessions without retaining an unbounded chat
// history in the browser. The budget counts strings without serializing them.
function retainedBytes(value) {
  if (typeof value === "string") return 16 + value.length * 2;
  if (value === null || typeof value !== "object") return 8;
  let size = 32;
  for (const [key, child] of Object.entries(value)) size += key.length * 2 + retainedBytes(child);
  return size;
}

export function createSessionCache({ maxEntries = 4, maxBytes = 8 * 1024 * 1024 } = {}) {
  const entries = new Map();
  let bytes = 0;
  const remove = (id) => {
    const old = entries.get(id);
    if (old) { bytes -= old.bytes; entries.delete(id); }
  };
  return {
    peek: (id) => entries.get(id)?.session || null,
    get(id) {
      const entry = entries.get(id);
      if (!entry) return null;
      entries.delete(id);
      entries.set(id, entry);
      return entry.session;
    },
    set(session) {
      if (!session?.id || (entries.get(session.id)?.session.revision ?? -1) > session.revision) return;
      remove(session.id);
      const size = retainedBytes(session);
      if (size > maxBytes || maxEntries < 1) return;
      while (entries.size >= maxEntries || bytes + size > maxBytes) remove(entries.keys().next().value);
      entries.set(session.id, { session, bytes: size });
      bytes += size;
    },
    retain(ids) {
      const keep = new Set(ids);
      for (const id of entries.keys()) if (!keep.has(id)) remove(id);
    },
    clear() { entries.clear(); bytes = 0; },
  };
}

export const FULL_REFRESH = { state: true, session: true };
export const NO_REFRESH = { state: false, session: false };
export const mergeRefresh = (first, second) => ({ state: first.state || second.state, session: first.session || second.session });

export function parseChange(data) {
  try {
    const change = JSON.parse(data);
    if (!Number.isFinite(change.revision) || typeof change.state !== "boolean" ||
        (change.session !== null && typeof change.session !== "string")) return null;
    return change;
  } catch { return null; }
}

export function changeRefresh(data, selected, revision = 0) {
  const change = parseChange(data);
  return change
    ? { state: change.state, session: change.session === null || (change.session === selected && change.revision > revision) }
    : FULL_REFRESH;
}
