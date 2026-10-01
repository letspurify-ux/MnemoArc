import test from "node:test";
import assert from "node:assert/strict";
import { createSessionCache, changeRefresh, mergeRefresh } from "../src/session-cache.js";

test("recent sessions retain their loaded pages and evict the least recently used entry", () => {
  const cache = createSessionCache({ maxEntries: 2 });
  const first = { id: "a", revision: 1, bundles: [{ id: 1 }, { id: 2 }], previous: 1 };
  cache.set(first);
  cache.set({ id: "b", revision: 2 });
  assert.equal(cache.get("a"), first);
  cache.set({ id: "c", revision: 3 });
  assert.equal(cache.peek("b"), null);
  assert.equal(cache.peek("a").previous, 1);
  cache.set({ id: "a", revision: 0 });
  assert.equal(cache.peek("a"), first);
  cache.retain(["c"]);
  assert.equal(cache.peek("a"), null);
  cache.clear();
  assert.equal(cache.peek("c"), null);
});

test("the browser cache releases oversized and cumulatively large responses", () => {
  const cache = createSessionCache({ maxBytes: 600 });
  const session = (id, text) => ({ id, revision: 1, bundles: [{ messages: [{ content: text }] }] });
  cache.set(session("a", "a".repeat(100)));
  cache.set(session("b", "b".repeat(100)));
  assert.equal(cache.peek("a"), null);
  assert.equal(cache.peek("b").id, "b");
  cache.set(session("b", "b".repeat(1000)));
  assert.equal(cache.peek("b"), null);
});

test("other sessions do not trigger detail reads and accumulated changes keep both scopes", () => {
  const change = (session, state = false, revision = 10) => JSON.stringify({ session, state, revision });
  assert.deepEqual(changeRefresh(change("b"), "a"), { state: false, session: false });
  assert.deepEqual(changeRefresh(change("b", true), "a"), { state: true, session: false });
  assert.deepEqual(changeRefresh(change("a"), "a", 10), { state: false, session: false });
  assert.deepEqual(changeRefresh(change("a"), "a", 9), { state: false, session: true });
  assert.deepEqual(changeRefresh(change(null, true), "a"), { state: true, session: true });
  assert.deepEqual(changeRefresh("refresh", "a"), { state: true, session: true });
  assert.deepEqual(mergeRefresh(changeRefresh(change("b", true), "a"), changeRefresh(change("a"), "a")), { state: true, session: true });
});
