import { test } from "node:test";
import assert from "node:assert/strict";
import { api } from "../src/api.js";
import { mergeSession, mergeOlder } from "../src/session-history.js";

const snapshot = (ids, extra = {}) => ({
  id: "session", revision: 1, previous: ids[0] > 1 ? ids[0] : null,
  pruned_through: null, bundles: ids.map((id) => ({ id, active: true })), ...extra,
});

test("successful HTTP responses still need valid JSON objects", async (t) => {
  for (const body of ["<html>gateway</html>", "", "null", "42", "[]", '"saved"']) {
    t.mock.method(globalThis, "fetch", async () => new Response(body));
    await assert.rejects(api("/settings"), /서버 응답을 읽지 못했습니다/);
    t.mock.restoreAll();
  }
  t.mock.method(globalThis, "fetch", async () => Response.json({ saved: true }));
  assert.deepEqual(await api("/settings"), { saved: true });
});

test("API errors preserve server messages, including null error responses", async (t) => {
  t.mock.method(globalThis, "fetch", async () => Response.json({ error: "설정 오류" }, { status: 400 }));
  await assert.rejects(api("/settings"), /설정 오류/);
  t.mock.restoreAll();
  t.mock.method(globalThis, "fetch", async () => Response.json(null, { status: 503 }));
  await assert.rejects(api("/settings"), /요청 실패 \(503\)/);
});

test("a stale server is detected before its state can enable task requests", async (t) => {
  for (const state of [{ running: null }, { running: [] }, { api_version: 1, running: [] }, { api_version: 2, running: [] }]) {
    t.mock.method(globalThis, "fetch", async () => Response.json(state));
    await assert.rejects(api("/state"), (error) =>
      error.code === "server_version_mismatch" && /앱을 종료한 뒤 다시 시작/.test(error.message),
    );
    t.mock.restoreAll();
  }
  const state = { api_version: 2, server_instance: "test-server", running: [], sessions: [] };
  t.mock.method(globalThis, "fetch", async () => Response.json(state));
  assert.deepEqual(await api("/state"), state);
});

test("late history pages never resurrect pruned bundles or cursors", () => {
  const live = snapshot([5, 6], { revision: 3, pruned_through: 4, previous: null });
  const older = snapshot([2, 3, 4], { previous: 2 });
  const merged = mergeOlder(live, older);
  assert.deepEqual(merged.bundles.map((b) => b.id), [5, 6]);
  assert.equal(merged.previous, null);
  assert.equal(merged.pruned_through, 4);
});

test("newer history pages prune already loaded rows but do not replace live state", () => {
  const live = snapshot([1, 2, 3, 4], { status: "running" });
  const older = snapshot([3], { revision: 2, pruned_through: 2, previous: null, status: "idle" });
  const merged = mergeOlder(live, older);
  assert.deepEqual(merged.bundles.map((b) => b.id), [3, 4]);
  assert.equal(merged.previous, null);
  assert.equal(merged.status, "running");
  assert.equal(merged.revision, live.revision);
  const staleRefresh = mergeSession(merged, snapshot([1, 2, 3, 4]));
  assert.deepEqual(staleRefresh.bundles.map((b) => b.id), [3, 4]);
  assert.equal(staleRefresh.pruned_through, 2);
});

test("overlapping history flags come from the newer snapshot and cursors only go backward", () => {
  const live = snapshot([3, 4], { revision: 2, previous: 3 });
  live.bundles[0].active = false;
  const older = snapshot([2, 3], { previous: 2 });
  const merged = mergeOlder(live, older);
  assert.equal(merged.bundles.find((b) => b.id === 3).active, false);
  assert.equal(merged.previous, 2);
  assert.equal(mergeOlder(merged, snapshot([3, 4])).previous, 2);
  const newer = snapshot([2, 3], { revision: 3 });
  assert.equal(mergeOlder(live, newer).bundles.find((b) => b.id === 3).active, true);
});

test("live refreshes retain loaded history, reject older snapshots and clear pruned cursors", () => {
  const loaded = snapshot([2, 3, 4]);
  const latest = snapshot([4, 5], { revision: 2, pruned_through: 1 });
  const merged = mergeSession(loaded, latest);
  assert.deepEqual(merged.bundles.map((b) => b.id), [2, 3, 4, 5]);
  assert.equal(merged.previous, null);
  assert.strictEqual(mergeSession(merged, loaded), merged);
  assert.strictEqual(mergeOlder(merged, { ...loaded, id: "another" }), merged);
});

test("a gap after a long refresh keeps an older-page cursor instead of hiding missing history", () => {
  const loaded = snapshot([1, 2]);
  const latest = snapshot([5, 6], { revision: 2 });
  const merged = mergeSession(loaded, latest);
  assert.deepEqual(merged.bundles.map((b) => b.id), [5, 6]);
  assert.equal(merged.previous, 5);
});

test("an old page cannot close the cursor across a gap introduced by a live refresh", () => {
  const latest = snapshot([7, 8], { revision: 3, previous: 7 });
  const older = snapshot([1, 2], { previous: null });
  const merged = mergeOlder(latest, older);
  assert.deepEqual(merged.bundles.map((b) => b.id), [7, 8]);
  assert.equal(merged.previous, 7);
  const filled = mergeOlder(merged, snapshot([3, 4, 5, 6], { revision: 3 }));
  assert.deepEqual(filled.bundles.map((b) => b.id), [3, 4, 5, 6, 7, 8]);
  assert.equal(filled.previous, 3);
  assert.equal(mergeOlder(filled, older).previous, null);
});

test("a completely pruned old page cannot hide retained but not yet loaded history", () => {
  const latest = snapshot([7, 8], { revision: 3, pruned_through: 5 });
  const merged = mergeOlder(latest, snapshot([1, 2], { previous: null }));
  assert.deepEqual(merged.bundles.map((b) => b.id), [7, 8]);
  assert.equal(merged.previous, 7);
});
