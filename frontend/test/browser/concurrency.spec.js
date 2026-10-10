import { test, expect } from "./fixtures.js";

test("parallel sessions survive reload and allow independent cancellation and closing", async ({ page, request }) => {
  const initial = await (await request.get("/api/state")).json();
  const headers = { "x-mnemoarc-client": "web" };
  const ids = [];
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    const saved = await request.put("/api/settings", { headers, data: { config: {
      ...initial.config, model: "concurrency-fixture", model_context: 128000,
      max_concurrent_sessions: 2,
    } } });
    expect(saved.ok()).toBe(true);
    for (let i = 0; i < 3; i++) {
      const created = await (await request.post("/api/sessions", { headers, data: { project: initial.config.projects[0] } })).json();
      ids.push(created.id);
    }
    await page.goto(`/#${ids[0]}`);
    for (let i = 0; i < 2; i++) {
      if (i > 0) await page.locator(".session-button").nth(initial.sessions.length + i).click();
      await page.getByRole("textbox", { name: "메시지", exact: true }).fill(`동시 실행 테스트 ${i + 1}`);
      await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
      await page.getByRole("button", { name: "메시지 보내기" }).click();
      await expect(page.getByText(`동시 실행 테스트 ${i + 1} 응답 중`, { exact: true })).toBeVisible();
    }
    expect((await (await request.get("/api/state")).json()).running).toHaveLength(2);
    await page.reload();
    await expect(page.getByText("동시 실행 테스트 2 응답 중", { exact: true })).toBeVisible();
    await expect(page.getByText("동시 실행 테스트 1 응답 중", { exact: true })).toHaveCount(0);
    await page.locator("summary.running-link").click();
    await expect(page.locator(".running-list")).toContainText("동시 실행 테스트 1");
    await page.screenshot({ path: "test-artifacts/concurrent-sessions.png", fullPage: true });

    await page.locator(".session-button").nth(initial.sessions.length + 2).click();
    await page.getByRole("textbox", { name: "메시지", exact: true }).fill("한도가 풀리면 보낼 초안");
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
    await expect(page.locator(".capacity-note")).toContainText("동시 실행 한도");
    await expect(page.locator(".workflow-badge")).toBeVisible();
    await page.locator("summary.running-link").click();
    await expect(page.locator(".running-list button")).toHaveCount(2);
    await page.locator(".running-list button").filter({ hasText: "동시 실행 테스트 1" }).click();
    await page.getByRole("button", { name: "■ 중지" }).click();
    await expect(page.locator(".status-pill")).toHaveText("중지됨");
    await expect.poll(async () => (await (await request.get("/api/state")).json()).running.map((run) => run.id)).toEqual([ids[1]]);
    // Return using the sidebar so the unsent draft stays in this tab.
    await page.locator(".session-button").filter({ hasText: "새 대화" }).last().click();
    await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toHaveValue("한도가 풀리면 보낼 초안");
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
    await page.locator("summary.running-link").click();
    await page.locator(".running-list button").filter({ hasText: "동시 실행 테스트 2" }).click();
    await expect(page.getByText("동시 실행 테스트 2 응답 중", { exact: true })).toBeVisible();
    page.once("dialog", (dialog) => dialog.accept());
    await page.locator(".session-actions").getByRole("button", { name: "세션 닫기", exact: true }).click();
    await expect.poll(async () => (await (await request.get("/api/state")).json()).running).toEqual([]);
    expect((await request.get(`/api/sessions/${ids[1]}`)).status()).toBe(404);
    expect(errors).toEqual([]);
  } finally {
    for (const id of ids) await request.delete(`/api/sessions/${id}`, { headers });
    await request.put("/api/settings", { headers, data: { config: initial.config } });
  }
});
