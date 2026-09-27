import { test } from "node:test";
import assert from "node:assert/strict";
import { rebaseDraft } from "../src/draft.js";

test("server changes refresh untouched fields, including nested settings", () => {
  const base = { model: "old", database: { host: "old", port: 1521 }, projects: [{ name: "old" }] };
  const draft = { ...base, model: "mine", database: { ...base.database, port: "" } };
  const incoming = { model: "remote", database: { host: "remote", port: 1522 }, projects: [{ name: "remote" }] };
  const merged = rebaseDraft(base, draft, incoming);
  assert.deepEqual(merged, { model: "mine", database: { host: "remote", port: "" }, projects: [{ name: "remote" }] });
  assert.equal(base.model, "old");
  assert.equal(draft.database.host, "old");
});

test("edited lists remain intact after server insertions and removals", () => {
  const base = { queries: [{ id: "A" }, { id: "B" }] };
  const draft = { queries: [{ id: "A-edited" }, { id: "B" }] };
  const incoming = { queries: [{ id: "B" }] };
  assert.deepEqual(rebaseDraft(base, draft, incoming), draft);
  assert.deepEqual(rebaseDraft(base, structuredClone(base), incoming), incoming);
});

test("a saved baseline accepts normalization and preserves edits after submission", () => {
  const submitted = { model: "saved", root: "./src", purpose: "old" };
  const draft = { ...submitted, purpose: "later edit" };
  const incoming = { ...submitted, root: "/workspace/src" };
  assert.deepEqual(rebaseDraft(submitted, draft, incoming), { ...incoming, purpose: "later edit" });
});

test("new server fields and removed optional fields follow the server unless edited", () => {
  assert.deepEqual(rebaseDraft({ optional: null, removed: 1 }, { optional: "local", removed: 1 }, { optional: null, added: 2 }), {
    optional: "local", added: 2,
  });
});
