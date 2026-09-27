import { useState } from "react";
import { rebaseDraft } from "./draft.js";

export function useServerDraft(source, identity = "") {
  const [snapshot, setSnapshot] = useState(() => ({
    source, identity, base: source, draft: structuredClone(source),
  }));
  let current = snapshot;
  // Reconcile before children render, so a save cannot submit yesterday's
  // untouched fields for a frame after refreshed props arrive.
  if (snapshot.source !== source || snapshot.identity !== identity) {
    current = {
      source, identity, base: source,
      draft: snapshot.identity === identity
        ? rebaseDraft(snapshot.base, snapshot.draft, source)
        : structuredClone(source),
    };
    setSnapshot(current);
  }
  const setDraft = (value) => setSnapshot((previous) => ({
    ...previous,
    draft: typeof value === "function" ? value(previous.draft) : value,
  }));
  // Submitted fields stop being local overrides. The next server snapshot can
  // normalize them, while fields edited during the save remain in the draft.
  const markSaved = (submitted) => setSnapshot((previous) => ({ ...previous, base: submitted }));
  return [current.draft, setDraft, markSaved];
}
