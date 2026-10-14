import { test } from "node:test";
import assert from "node:assert/strict";
import { toolProgress } from "../src/activity.js";

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
  // A large parallel batch names three and counts the rest.
  assert.equal(
    toolProgress(
      { steps: ["읽기 a", "읽기 b", "읽기 c", "읽기 d", "읽기 e"] },
      labels,
    ),
    "읽기 a · 읽기 b · 읽기 c 외 2건 실행 중",
  );
  // A malformed call with no name leaves no empty entry.
  assert.equal(toolProgress({ tools: [""] }, labels), "도구 실행 중");
  // A runtime without summaries still shows the tool names.
  assert.equal(
    toolProgress({ tools: ["file_read", "task_plan"] }, labels),
    "파일 읽기 · task_plan 실행 중",
  );
});

test("each tool keeps its own label, since the settings list toggles by it", async () => {
  const { toolLabels } = await import("../src/api.js");
  const labels = Object.values(toolLabels);
  assert.equal(new Set(labels).size, labels.length);
});
