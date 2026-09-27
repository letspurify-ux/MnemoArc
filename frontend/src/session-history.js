// Both live snapshots and older pages can arrive after history has been pruned.
// A cursor names the first loaded bundle; no older bundle survives at/below it
// when pruning has reached the preceding ID.
const cursorAfterPruning = (cursor, pruned) =>
  cursor != null && cursor > (pruned || 0) + 1 ? cursor : null;

export function mergeSession(current, next) {
  if (!current || current.id !== next.id) return next;
  if (next.revision < current.revision) return current;
  const pruned = Math.max(current.pruned_through || 0, next.pruned_through || 0);
  const latest = next.bundles.filter((bundle) => bundle.id > pruned);
  let before = current.bundles.filter(
    (bundle) => bundle.id < (latest[0]?.id || 0) && bundle.id > pruned,
  );
  // A background tab can miss more than one page of new bundles. Preserve only
  // a contiguous history; otherwise the latest page's cursor must fill the gap.
  if (before.length && before.at(-1).id + 1 !== latest[0]?.id) before = [];
  return {
    ...next,
    bundles: [...before, ...latest],
    previous: cursorAfterPruning(
      before.length ? current.previous : next.previous,
      pruned,
    ),
    pruned_through: pruned || null,
  };
}

export function mergeOlder(current, older) {
  if (!current || current.id !== older.id) return current;
  const pruned = Math.max(current.pruned_through || 0, older.pruned_through || 0);
  const live = current.bundles.filter((bundle) => bundle.id > pruned);
  const history = older.bundles.filter((bundle) => bundle.id > pruned);
  // A live refresh may move past the page requested earlier. Its cursor only
  // describes that old range; using it across a gap hides the missing history.
  if (!history.length || (live.length && history.at(-1).id + 1 < live[0].id)) {
    return {
      ...current,
      bundles: live,
      previous: cursorAfterPruning(current.previous, pruned),
      pruned_through: pruned || null,
    };
  }
  const byId = new Map();
  // Overlapping bundles take flags/content from the newer snapshot.
  const pages = older.revision > current.revision
    ? [current.bundles, older.bundles] : [older.bundles, current.bundles];
  for (const page of pages) {
    for (const bundle of page) {
      if (bundle.id > pruned) byId.set(bundle.id, bundle);
    }
  }
  const previous = current.previous == null || older.previous == null
    ? null : Math.min(current.previous, older.previous);
  return {
    ...current,
    bundles: [...byId.values()].sort((a, b) => a.id - b.id),
    previous: cursorAfterPruning(previous, pruned),
    pruned_through: pruned || null,
  };
}
