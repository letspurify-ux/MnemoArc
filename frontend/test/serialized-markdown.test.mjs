// Answers that kept one extra level of JSON escaping (a literal \n) are
// restored before rendering. Links GFM finds in plain text are part of it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { unified } from "unified";
import remarkParse from "remark-parse";
import { REMARK_PLUGINS } from "../src/chat/math.js";

const processor = unified().use(remarkParse).use(REMARK_PLUGINS);
const render = (markdown) =>
  processor.runSync(processor.parse(markdown), markdown);
const paragraph = (markdown) => render(markdown).children[0].children;

test("a URL or email after an unmatched bracket does not break restoration", () => {
  // GFM rebuilds these links from text without source positions.
  for (const [markdown, url] of [
    ["참고 [https://example.com\\n다음 줄", "https://example.com"],
    ["문의 [admin@example.com\\n다음", "mailto:admin@example.com"],
  ]) {
    const [, link, rest] = paragraph(markdown);
    assert.equal(link.type, "link");
    assert.equal(link.url, url);
    assert.equal(rest.value.startsWith("\n"), true);
  }
});

test("a serialized line break ends a link found in plain text", () => {
  for (const [markdown, url] of [
    ["참고 https://example.com\\n다음 줄", "https://example.com"],
    ["admin@example.com\\n다음", "mailto:admin@example.com"],
  ]) {
    const link = paragraph(markdown).find((node) => node.type === "link");
    assert.equal(link.url, url);
  }
  const [first, second] = render("www.example.com/a\\n\\n다음").children;
  assert.equal(first.children[0].url, "http://www.example.com/a");
  assert.equal(second.children[0].value, "다음");
});

test("written link destinations stay as written", () => {
  const [link] = paragraph("[t](https://e.com/a\\nb)\\n다음");
  assert.equal(link.url, "https://e.com/a\\nb");
  const [angle] = paragraph("<https://example.com>\\n다음");
  assert.equal(angle.url, "https://example.com");
});
