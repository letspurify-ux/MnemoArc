import { test, expect } from "@playwright/test";

test("execution history survives another request and reload", async ({
  page,
  request,
}) => {
  const state = await (await request.get("/api/state")).json();
  const id = state.sessions[0].id;
  const configured = await request.put(`/api/sessions/${id}/settings`, {
    headers: { "x-mnemoarc-client": "web" },
    data: {
      config: {
        ...state.config,
        model: "gpt-4o",
        model_context: 128000,
        source_answer_review: false,
        completion_review_enabled: false,
      },
      api_key: "browser-test-only",
      credential_mode: "session",
    },
  });
  expect(configured.ok()).toBe(true);
  await page.goto("/");
  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("첫 번째 실행");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.locator(".status-pill")).toHaveText("완료");
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  const history = page.getByRole("region", { name: "실행 기록" });
  await expect(history.locator("details")).toHaveCount(1);
  await expect(history).toContainText("정상 완료");
  const first = (await (await request.get(`/api/sessions/${id}`)).json())
    .run_history[0];

  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("느린 요청 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.locator(".thinking")).toContainText("모델 응답 대기 중");
  await expect(history.locator("details")).toHaveCount(1);
  await page.getByRole("button", { name: "■ 중지" }).click();
  await expect(page.locator(".status-pill")).toHaveText("중지됨");
  await expect(history.locator("details")).toHaveCount(2);
  await expect(history.locator("details").first()).toContainText("실행 중지");
  const after = await (await request.get(`/api/sessions/${id}`)).json();
  expect(after.run_history[0]).toEqual(first);
  expect(after.run_history[1].reason).toBe("cancelled");

  await page.reload();
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  await expect(history.locator("details")).toHaveCount(2);
  await history.locator("summary").first().click();
  await expect(history).toContainText("마지막 단계");
  await expect(history).toContainText("사용량에 추정치가 포함");
  await page
    .locator(".inspector")
    .screenshot({ path: "test-artifacts/run-history.png" });
});
