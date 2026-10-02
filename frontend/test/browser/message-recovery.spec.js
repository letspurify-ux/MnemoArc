import { test, expect, workspace } from "./review-fixtures.js";

test("an older running server gives a restart message before enabling requests", async ({
  page,
  request,
}) => {
  const state = await (await request.get("/api/state")).json();
  delete state.api_version;
  state.running = null;
  let runs = 0;
  await page.route("**/api/state", (route) => route.fulfill({ json: state }));
  await page.route("**/api/sessions/*/run", (route) => {
    runs++;
    return route.fulfill({ json: { started: true } });
  });
  await page.goto("/");
  await expect(
    page.getByText(/실행 중인 서버와 화면 버전이 맞지 않습니다/),
  ).toBeVisible();
  await expect(
    page.getByRole("textbox", { name: "메시지", exact: true }),
  ).toHaveCount(0);
  expect(runs).toBe(0);
});

test("a server version change disables a cached session and keeps its unsent draft", async ({
  page,
  request,
}) => {
  const { state } = await workspace({ page, request });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("중지 후 수정할 요청 초안");
  delete state.api_version;
  await expect(
    page.getByText(/실행 중인 서버와 화면 버전이 맞지 않습니다/),
  ).toBeVisible();
  await expect(input).toHaveCount(0);
  state.api_version = 2;
  await expect(input).toBeVisible();
  await expect(input).toHaveValue("중지 후 수정할 요청 초안");
});

test("a failed change is identified separately from a question and can be resubmitted", async ({
  page,
  request,
}) => {
  const { session } = await workspace(
    { page, request },
    {
      workflow_mode: "source_document",
      status: "cancelled",
      has_task: true,
      run_history: [
        {
          id: "failed-change",
          request: "첫 장만 작성해줘",
          workflow: "message_routing",
          status: "blocked",
          reason: "message_routing_invalid",
          error: "message_routing_invalid: task preserved; retry this message",
          last_stage: "message_routing",
          started_at: "2026-10-02T09:00:00Z",
          ended_at: "2026-10-02T09:00:01Z",
          elapsed_ms: 1000,
          rounds: 3,
          input_tokens: 100,
          output_tokens: 20,
          token_limit: 10000,
          timeout_secs: 60,
          checkpoint_pending: false,
        },
      ],
    },
  );
  await expect(page.getByText(/요청 변경을 적용하지 못했습니다/)).toBeVisible();
  await expect(
    page.getByText("질문 답변을 완료하지 못했습니다", { exact: true }),
  ).toHaveCount(0);
  await expect(page.locator(".status-pill")).toHaveText("중지됨");
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  const history = page.getByRole("region", { name: "실행 기록" });
  await expect(history).toContainText("요청 판별 실패");
  await history.locator("summary").click();
  await expect(history).toContainText("요청 변경 확인");
  let sent;
  await page.route(`**/api/sessions/${session.id}/run`, (route) => {
    sent = route.request().postDataJSON();
    session.status = "running";
    session.activity = {
      stage: "message_routing",
      attempt: 2,
      started_at_ms: Date.now(),
    };
    return route.fulfill({ json: { started: true } });
  });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("첫 장만 남기도록 문서를 수정해줘.");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect
    .poll(() => sent)
    .toEqual({ text: "첫 장만 남기도록 문서를 수정해줘.", action: "message" });
  await expect(page.locator(".thinking")).toContainText(
    "요청의 작업 변경 사항 확인 중 · 2번째 시도",
  );
  session.status = "complete";
  session.activity = { stage: "idle" };
  session.run_history.push({
    ...session.run_history[0],
    id: "recovered-change",
    workflow: "source_document",
    status: "complete",
    reason: "complete",
    error: null,
  });
  await expect(page.locator(".status-pill")).toHaveText("완료");
  await expect(page.getByText(/요청 변경을 적용하지 못했습니다/)).toHaveCount(
    0,
  );
});
