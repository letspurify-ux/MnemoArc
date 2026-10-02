import { test } from "node:test";
import assert from "node:assert/strict";
import {
  displayPath,
  displayPathText,
  displayToolResult,
} from "../src/paths.js";

test("Windows drive and UNC paths are readable without changing other path namespaces", () => {
  assert.equal(
    displayPath(String.raw`\\?\C:\프로젝트\소스`),
    String.raw`C:\프로젝트\소스`,
  );
  assert.equal(displayPath("\\\\?\\z:\\"), "z:\\");
  assert.equal(
    displayPath(String.raw`\\?\UNC\server\share\프로젝트`),
    String.raw`\\server\share\프로젝트`,
  );
  assert.equal(
    displayPath(String.raw`\\?\unc\server\share`),
    String.raw`\\server\share`,
  );
  for (const path of [
    "/Users/developer/프로젝트",
    "docs/source-summary.md",
    String.raw`C:\프로젝트`,
    String.raw`\\server\share\프로젝트`,
    String.raw`\\?\Volume{abc}\프로젝트`,
    String.raw`\\.\C:\프로젝트`,
    String.raw`\\?\C:relative`,
    String.raw`\\?\UNC\server`,
  ])
    assert.equal(displayPath(path), path);
  assert.equal(displayPath(undefined), "");
});

test("embedded paths in diagnostics normalize without touching other namespaces or incomplete UNC prefixes", () => {
  const input = String.raw`읽기 실패: \\?\C:\프로젝트\결과.md; 다시 확인: \\?\UNC\server\share\문서.md`;
  assert.equal(
    displayPathText(input),
    String.raw`읽기 실패: C:\프로젝트\결과.md; 다시 확인: \\server\share\문서.md`,
  );
  for (const text of [
    String.raw`장치: \\?\Volume{abc}\문서.md`,
    String.raw`장치: \\.\C:\문서.md`,
    String.raw`잘못된 경로: \\?\C:relative`,
    String.raw`잘못된 경로: \\?\UNC\server`,
    "코드에 없는 일반 오류",
  ])
    assert.equal(displayPathText(text), text);
});

test("tool result presentation keeps original receipts, source contents and database rows intact", () => {
  const path = String.raw`\\?\C:\프로젝트\결과.md`;
  const network = String.raw`\\?\UNC\server\share\소스.md`;
  const sourceText = `const path = ${JSON.stringify(path)};`;
  const result = {
    status: "ok",
    error: `읽기 실패: ${path}`,
    data: {
      path,
      files: [path, network],
      source: { id: "S1", path: network, excerpt: sourceText },
      content: { text: sourceText, path },
      body: sourceText,
      query: sourceText,
      rows: [{ path, message: `원본 데이터 ${path}` }],
      hash: "unchanged",
      next_cursor: "opaque-cursor",
      scope: path,
      required_path: network,
      note: `대상 파일: ${path}`,
    },
  };
  const original = structuredClone(result);
  const displayed = displayToolResult(result);
  assert.notEqual(displayed, result);
  assert.equal(displayed.data.path, String.raw`C:\프로젝트\결과.md`);
  assert.equal(displayed.data.source.path, String.raw`\\server\share\소스.md`);
  assert.deepEqual(displayed.data.files, [
    String.raw`C:\프로젝트\결과.md`,
    String.raw`\\server\share\소스.md`,
  ]);
  assert.equal(displayed.error, String.raw`읽기 실패: C:\프로젝트\결과.md`);
  assert.equal(displayed.data.content, result.data.content);
  assert.equal(displayed.data.rows, result.data.rows);
  assert.equal(displayed.data.source.excerpt, sourceText);
  assert.equal(displayed.data.body, sourceText);
  assert.equal(displayed.data.query, sourceText);
  assert.equal(displayed.data.hash, "unchanged");
  assert.equal(displayed.data.next_cursor, "opaque-cursor");
  assert.equal(displayed.data.scope, String.raw`C:\프로젝트\결과.md`);
  assert.equal(
    displayed.data.required_path,
    String.raw`\\server\share\소스.md`,
  );
  assert.equal(displayed.data.note, String.raw`대상 파일: C:\프로젝트\결과.md`);
  assert.deepEqual(
    JSON.parse(JSON.stringify(displayed)).data.path,
    displayed.data.path,
  );
  assert.deepEqual(result, original);
  const unchanged = { status: "ok", data: { content: sourceText } };
  assert.equal(displayToolResult(unchanged), unchanged);
});
