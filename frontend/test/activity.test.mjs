import { test } from "node:test";
import assert from "node:assert/strict";
import { recentSteps, toolProgress } from "../src/activity.js";

const labels = { file_read: "파일 읽기", source_search: "소스 검색" };

test("a tool batch shows what each call reads or searches", () => {
  assert.equal(
    toolProgress(
      {
        tools: ["file_read", "source_search"],
        steps: [
          "파일 읽기 src/ui/menu.rs:100-299",
          '소스 검색 "fn show" src/ui/connection_dialog.rs',
        ],
      },
      labels,
    ),
    '파일 읽기 src/ui/menu.rs:100-299 · 소스 검색 "fn show" src/ui/connection_dialog.rs 실행 중',
  );
  // A runtime without summaries still shows the tool names.
  assert.equal(
    toolProgress({ tools: ["file_read", "task_plan"] }, labels),
    "파일 읽기 · task_plan 실행 중",
  );
});

test("finished steps keep their order and mark refused calls", () => {
  assert.deepEqual(
    recentSteps({
      recent: [
        { text: "문서 편집 절 추가: 메뉴 구조", status: "ok" },
        { text: "파일 읽기 src/ui/table.rs:184-189", status: "error" },
        { status: "ok" },
      ],
    }),
    [
      { text: "문서 편집 절 추가: 메뉴 구조", ok: true },
      { text: "파일 읽기 src/ui/table.rs:184-189", ok: false },
    ],
  );
  assert.deepEqual(recentSteps({ stage: "model" }), []);
  assert.deepEqual(recentSteps(undefined), []);
});
