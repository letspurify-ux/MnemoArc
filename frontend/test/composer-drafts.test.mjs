import { test } from "node:test";
import assert from "node:assert/strict";
import { createComposerDrafts } from "../src/composer-drafts.js";

test("drafts are independent by session and stable between edits", () => {
  const drafts = createComposerDrafts();
  assert.strictEqual(drafts.get("A"), drafts.get("A"));
  drafts.update("A", { text: "첫 요청" });
  drafts.update("B", { text: "다음 질문" });
  const first = drafts.get("A");
  assert.deepEqual(first, { text: "첫 요청" });
  drafts.update("A", { text: "첫 요청" });
  assert.strictEqual(drafts.get("A"), first);
  assert.equal(drafts.get("B").text, "다음 질문");
});

test("a successful send clears only its own unedited draft", () => {
  const drafts = createComposerDrafts();
  drafts.update("A", { text: "보낼 내용" });
  const sent = drafts.get("A");
  drafts.update("B", { text: "다른 세션" });
  drafts.clearIfUnchanged("A", sent);
  assert.deepEqual(drafts.get("A"), { text: "" });
  assert.equal(drafts.get("B").text, "다른 세션");
});

test("an old acknowledgement cannot erase a newer edit even if its text matches", () => {
  const drafts = createComposerDrafts();
  drafts.update("A", { text: "같은 요청" });
  const sent = drafts.get("A");
  drafts.update("A", { text: "중간 편집" });
  drafts.update("A", { text: "같은 요청" });
  drafts.clearIfUnchanged("A", sent);
  assert.deepEqual(drafts.get("A"), { text: "같은 요청" });
});

test("closed sessions are discarded and late acknowledgements cannot revive them", () => {
  const drafts = createComposerDrafts();
  let changes = 0;
  const unsubscribe = drafts.subscribe(() => changes++);
  drafts.update("A", { text: "삭제할 초안" });
  const sent = drafts.get("A");
  drafts.update("B", { text: "  " });
  assert.equal(drafts.hasText(), true);
  drafts.retain(["B"]);
  assert.equal(drafts.hasText(), false);
  assert.equal(drafts.get("A").text, "");
  assert.equal(changes, 3);
  drafts.clearIfUnchanged("A", sent);
  assert.equal(changes, 3);
  unsubscribe();
  drafts.update("B", { text: "남은 초안" });
  assert.equal(changes, 3);
});
