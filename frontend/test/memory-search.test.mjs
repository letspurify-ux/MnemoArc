import { test } from "node:test";
import assert from "node:assert/strict";
import { memoryMatchesQuery } from "../src/memory-search.js";

test("memory path searches match drive and network paths as displayed or originally recorded", () => {
  for (const [path, readable] of [
    [String.raw`\\?\C:\프로젝트\소스`, String.raw`C:\프로젝트\소스`],
    [
      String.raw`\\?\UNC\server\share\프로젝트`,
      String.raw`\\server\share\프로젝트`,
    ],
  ]) {
    const memory = {
      id: "M1",
      key: "source",
      title: "경로 관련 발견",
      summary: `경로: ${path}`,
      tags: ["Windows"],
      status: "active",
      revision: 2,
    };
    const original = structuredClone(memory);
    assert.equal(memoryMatchesQuery(memory, readable), true);
    assert.equal(memoryMatchesQuery(memory, path), true);
    assert.equal(memoryMatchesQuery(memory, readable.toUpperCase()), true);
    assert.equal(memoryMatchesQuery(memory, "없는 경로"), false);
    assert.equal(memoryMatchesQuery(memory, "WINDOWS"), true);
    assert.equal(memoryMatchesQuery(memory, "source"), true);
    assert.equal(memoryMatchesQuery(memory, "M1"), true);
    assert.equal(memoryMatchesQuery(memory, ""), true);
    assert.deepEqual(memory, original);
  }
});

test("memory searches use literal text rather than JSON escapes and property names", () => {
  const memory = {
    title: '따옴표 "소스"',
    summary: "첫 줄\n둘째 줄",
    tags: [String.raw`폴더\하위`],
    revision: 1,
  };
  for (const query of ['"소스"', "첫 줄\n둘째 줄", String.raw`폴더\하위`]) {
    assert.equal(memoryMatchesQuery(memory, query), true);
  }
  assert.equal(memoryMatchesQuery(memory, "summary"), false);
  assert.equal(memoryMatchesQuery(memory, "title"), false);
});
