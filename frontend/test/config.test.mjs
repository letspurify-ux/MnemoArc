import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fieldKeys, parseValue, inputValue } from "../src/fields.js";
import {
  linkTarget,
  imageTarget,
  mdUrlTransform,
} from "../src/chat/markdown.js";
import { parseChartBlock, sliceSafe } from "../src/chat/chart.js";

test("resumed task text never joins a truncated follow-up answer", async () => {
  const { continuationMessages } = await import("../src/chat/continuation.js");
  const question = {
    id: 1,
    messages: [
      {
        role: "assistant",
        content: "Question answer",
        partial: true,
        follow_up: true,
      },
    ],
  };
  const next = {
    id: 2,
    messages: [
      {
        role: "assistant",
        content: "Original task continuation",
        continues_previous: true,
      },
    ],
  };
  assert.equal(continuationMessages([question, next]).messages.length, 2);
  const streamed = continuationMessages(
    [question],
    "Original task stream",
    true,
  );
  assert.equal(streamed.messages.length, 1);
  assert.equal(streamed.streamText, "Original task stream");
});

test("every public Rust setting has a UI editor", async () => {
  const code = await readFile(
    new URL("../../src/config.rs", import.meta.url),
    "utf8",
  );
  const block = code.split("pub struct Config {")[1].split("\n}")[0];
  const keys = [...block.matchAll(/pub (\w+):/g)]
    .map((m) => m[1])
    .filter((k) => k !== "api_key");
  assert.deepEqual([...fieldKeys, "projects"].sort(), keys.sort());
  assert.equal(new Set(fieldKeys).size, fieldKeys.length);
});
test("UI units and optional values round trip without silent zeroes", () => {
  for (const [value, type] of [
    [16777216, "mib"],
    [8192, "kib"],
    [0.8, "percent"],
    [64000, "number"],
  ]) {
    assert.equal(parseValue(inputValue(value, type), type), value);
  }
  assert.equal(parseValue("", "optionalNumber"), null);
  assert.equal(parseValue("", "number"), "");
  assert.equal(parseValue("", "optional"), null);
});
test("reused chat preserves unicode and blocks executable links", () => {
  assert.equal(sliceSafe("a😀b", 2), "a");
  assert.equal(
    linkTarget(mdUrlTransform("javascript:alert(1)", "href")).url,
    "",
  );
  assert.notEqual(imageTarget("https://example.com/image.png").kind, "image");
  assert.equal(parseChartBlock("not a chart").ok, false);
});

test("length continuation joins Markdown and streaming without losing the prefix", async () => {
  const { continuationMessages } = await import("../src/chat/continuation.js");
  const first = {
    id: 1,
    messages: [
      { role: "assistant", content: "```mermaid\nA -->", partial: true },
    ],
  };
  const last = {
    id: 2,
    messages: [
      { role: "assistant", content: " B\n```", continues_previous: true },
    ],
  };
  const complete = continuationMessages([first, last], "", false);
  assert.equal(complete.messages.length, 1);
  assert.equal(complete.messages[0].content, "```mermaid\nA --> B\n```");
  assert.equal(complete.messages[0].partial, false);
  const live = continuationMessages([first], " B", true);
  assert.equal(live.messages.length, 0);
  assert.equal(live.streamText, "```mermaid\nA --> B");
  assert.equal(first.messages[0].content, "```mermaid\nA -->");
  const stopped = continuationMessages([first], "", true);
  assert.equal(stopped.messages[0].content, first.messages[0].content);
});
