const equal = (left, right) =>
  left === right || JSON.stringify(left) === JSON.stringify(right);
const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);

// Move untouched fields to the latest server value, preserving local edits.
// Lists have no stable item IDs, so merging them by index could edit the wrong
// project/query after an insertion or deletion. Treat an edited list as a unit.
export function rebaseDraft(base, draft, incoming) {
  if (equal(base, draft)) return incoming;
  if (!object(base) || !object(draft) || !object(incoming)) return draft;
  return Object.fromEntries([...new Set([...Object.keys(incoming), ...Object.keys(draft)])]
    .map((key) => [key, rebaseDraft(base[key], draft[key], incoming[key])])
    .filter(([, value]) => value !== undefined));
}
