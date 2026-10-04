import { test } from "node:test";
import assert from "node:assert/strict";
import { cleanDocument } from "../src/document-export.js";

test("clean exports remove citation notation and wrappers while keeping prose and tables", () => {
  const input = [
    "# 사용자 안내",
    "",
    "설정을 저장합니다. (근거: `frontend/src/App.jsx:12-18`, **src/web.rs:20–30**)",
    "작업 src/한글.rs:1, 5-8 중에도 사용할 수 있습니다.",
    "메뉴를 엽니다. [출처: src/menu.ts#L10-L20]",
    "",
    "| 항목 | 설명 |",
    "| --- | --- |",
    "| 저장 | 내용을 보관합니다. (`src/save.rs:1-4`) |",
    "",
  ].join("\n");
  assert.equal(
    cleanDocument(input),
    [
      "# 사용자 안내",
      "",
      "설정을 저장합니다.",
      "작업 중에도 사용할 수 있습니다.",
      "메뉴를 엽니다.",
      "",
      "| 항목 | 설명 |",
      "| --- | --- |",
      "| 저장 | 내용을 보관합니다. |",
      "",
    ].join("\n"),
  );
  assert.equal(cleanDocument("본문. ([`src/a.rs:1-2`])"), "본문.");
});

test("citation links disappear and descriptive source links retain their label", () => {
  assert.equal(
    cleanDocument(
      "[설정 **저장**](src/settings.ts#L10-L20)을 확인합니다. ([src/a.rs:1-2](src/a.rs#L1-L2))",
    ),
    "설정 **저장**을 확인합니다.",
  );
  assert.equal(
    cleanDocument("[설정 저장][source]\n\n[source]: src/a.rs#L1-L2\n"),
    "설정 저장\n\n\n",
  );
  assert.equal(
    cleanDocument("[파일](src/file.rs) 및 [웹](https://example.org/a.rs#L1)"),
    "[파일](src/file.rs) 및 [웹](https://example.org/a.rs#L1)",
  );
});

test("normal code, indented examples, HTML and URLs survive exactly", () => {
  const examples = [
    "`load('src/a.rs:1-2')`",
    "",
    "```rust",
    "// Example: src/a.rs:1-2",
    "```",
    "",
    "- ```text",
    "  src/a.rs:1-2",
    "  ```",
    "",
    "> ~~~text",
    "> src/a.rs:1-2",
    "> ~~~",
    "",
    "    src/a.rs:1-2",
    "",
    "<!-- src/a.rs:1-2 -->",
    "",
    "https://example.org/a.rs:1-2 https://example.org/a.rs#L1-L2",
    "",
  ].join("\n");
  assert.equal(cleanDocument(examples), examples);
  assert.equal(
    cleanDocument(`본문 (src/b.rs:2-3).\n\n${examples}`),
    `본문.\n\n${examples}`,
  );
});

test("Mermaid labels lose citations and citation-only line breaks without changing the graph", () => {
  const input =
    '```mermaid\nflowchart LR\n  A["설정<br/>src/a.rs:1-2"] --> B["저장 (src/b.rs#L3-L4)"]\n```\n';
  assert.equal(
    cleanDocument(input),
    '```mermaid\nflowchart LR\n  A["설정"] --> B["저장"]\n```\n',
  );
  assert.equal(
    cleanDocument('```mermaid\nA["시작\\nsrc/a.rs:1"]\n```'),
    '```mermaid\nA["시작"]\n```',
  );
});

test("documents without citations and line endings are preserved", () => {
  for (const text of [
    "",
    "\uFEFF# 안내\r\n\r\n일반 문서.\r\n",
    "원문 \u0000 값",
    "본문 `src/a.rs`와 12:30 시각.",
  ])
    assert.equal(cleanDocument(text), text);
  assert.equal(
    cleanDocument("# 안내\r\n본문. (`src/a.rs:1-2`)\r\n"),
    "# 안내\r\n본문.\r\n",
  );
});
