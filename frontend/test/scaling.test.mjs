// Inputs that used to take seconds to minutes in the answer renderer or the
// clean export. Limits leave a wide margin over the measured linear time.
import { test } from "node:test";
import assert from "node:assert/strict";
import { unified } from "unified";
import remarkParse from "remark-parse";
import { REMARK_PLUGINS } from "../src/chat/math.js";
import { cleanDocument } from "../src/document-export.js";
import { collectMathSpans } from "../shared/math-spans.mjs";
import { executableMermaidSource } from "../shared/mermaid-math-spans.mjs";
import { cleanUrl, mermaidFetchesViaStyle } from "../src/chat/markdown.js";
import { parseChartBlock, splitBlock, toNumber } from "../src/chat/chart.js";

const processor = unified().use(remarkParse).use(REMARK_PLUGINS);
const render = (markdown) =>
  processor.runSync(processor.parse(markdown), markdown);
const within = (limit, run) => {
  const started = performance.now();
  const result = run();
  const elapsed = performance.now() - started;
  assert.ok(elapsed < limit, `took ${elapsed.toFixed(0)} ms`);
  return result;
};

test("a long run of backslashes is scanned once (was 35 s at 80,000)", () => {
  const source = "\\".repeat(80000);
  const tree = processor.parse(source);
  within(3000, () => collectMathSpans(tree, source, (value) => processor.parse(value)));
});

test("many paragraphs with dollars do not rescan every context", () => {
  within(5000, () => render("$ a\n\n".repeat(64000)));
});

test("tables with many definitions parse cells together (was 72 s)", () => {
  const table =
    "| h | i | j |\n| - | - | - |\n" + "| a | b | c |\n".repeat(1000);
  const definitions = Array.from(
    { length: 1000 },
    (_, i) => `[d${i}]: /u${i}`,
  ).join("\n");
  within(5000, () => render(`${table}\n${definitions}`));
});

test("serialized visualizations with many definitions are protected in one parse (was 3 s at 400)", () => {
  const definitions = Array.from(
    { length: 1000 },
    (_, i) => `[d${i}]: /u${i}`,
  ).join("\n");
  const cells =
    "| h | i |\n| - | - |\n" +
    Array.from({ length: 1000 }, (_, i) => `| \`chart\\nx,y\\na,${i}\` | b |`).join("\n");
  const paragraphs = Array.from(
    { length: 1000 },
    (_, i) => `\`mermaid\\nflowchart LR\\nA-->B${i}\``,
  ).join("\n\n");
  within(5000, () => render(`${cells}\n\n${definitions}`));
  within(5000, () => render(`${paragraphs}\n\n${definitions}`));
});

test("Mermaid comment masking is linear in blank lines", () => {
  const source = "%% note\n" + "\n".repeat(80000) + "flowchart LR";
  const masked = within(2000, () => executableMermaidSource(source));
  assert.equal(masked, "       \n" + "\n".repeat(80000) + "flowchart LR");
});

test("clean export stays linear on long citation lines and blank runs", () => {
  within(3000, () => cleanDocument("a.rs:1,".repeat(40000)));
  within(3000, () => cleanDocument("【a.rs:1】".repeat(8000)));
  assert.equal(
    within(3000, () => cleanDocument("see a.rs:1" + " ".repeat(40000) + "x")),
    "see x",
  );
  assert.equal(
    within(3000, () =>
      cleanDocument("(근거" + " ".repeat(40000) + "a.rs:1 ;  b.rs:2   )다음"),
    ),
    "다음",
  );
  assert.equal(
    within(3000, () => cleanDocument("`a.rs:1" + " ".repeat(40000) + "`")),
    "",
  );
});

test("Mermaid front matter is found without rescanning blank lines (was 2 s)", () => {
  const blank = "---" + "\n".repeat(40000) + "x";
  assert.equal(within(2000, () => mermaidFetchesViaStyle(blank)), false);
  assert.equal(within(2000, () => executableMermaidSource(blank)), blank);
  const front = "---\nconfig:\n  fontFamily: url(x)\n---\nflowchart LR";
  assert.equal(mermaidFetchesViaStyle(front), true);
  assert.equal(
    executableMermaidSource(front),
    front.slice(0, front.indexOf("flowchart")).replace(/[^\r\n]/g, " ") +
      "flowchart LR",
  );
});

test("chart lines with long blank or digit runs parse in linear time (was 1 s)", () => {
  const spaces = " ".repeat(80000);
  const block = `title: a${spaces}b\ntype: bar\n| a | b |\n|-|-|${spaces}x\n| x | 1${spaces}x |`;
  const { config } = within(1500, () => splitBlock(block));
  assert.equal(config.title, `a${spaces}b`);
  within(1500, () => parseChartBlock(block));
  assert.equal(within(1500, () => toNumber("1".repeat(80000) + "x")), null);
  assert.equal(toNumber("12.5 %"), 12.5);
});

test("serialized table detection and URL cleanup stay linear", () => {
  within(1500, () => render("a\\n" + " ".repeat(80000) + "x"));
  assert.equal(
    within(1500, () => cleanUrl(" http://a" + " ".repeat(80000) + "b \u0001")),
    "http://a" + " ".repeat(80000) + "b",
  );
});
