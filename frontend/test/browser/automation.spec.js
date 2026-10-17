import { test, expect } from "./fixtures.js";

test("automatic improvement settings and a project's own prompt save through the UI", async ({
  page,
  request,
}) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto("/");
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByRole("button", { name: "자동 문서 개선" }).click();
  await page.getByRole("checkbox", { name: "자동 문서 개선 사용" }).check();
  await page.getByLabel("자동 개선 기본 프롬프트", { exact: true }).fill("기본 개선 프롬프트");
  await page.getByLabel("작업 없는 기간 (일)", { exact: true }).fill("3");
  await expect(page.getByText("아직 자동 실행 기록이 없습니다")).toBeVisible();
  await page.getByRole("checkbox", { name: "현재 세션에도 적용" }).uncheck();
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "설정을 저장했습니다" })).toBeVisible();
  await page.screenshot({ path: "test-artifacts/automation-settings.png", fullPage: true });
  // The session scope shows the app-wide values, read-only, not its own copy.
  await page.getByLabel("설정 적용 범위").selectOption("session");
  const enabled = page.getByRole("checkbox", { name: "자동 문서 개선 사용" });
  await expect(enabled).toBeChecked();
  await expect(enabled).toBeDisabled();
  await page.getByLabel("설정 적용 범위").selectOption("global");

  await page
    .locator(".sidebar-bottom")
    .getByRole("button", { name: "프로젝트 관리", exact: true })
    .click();
  const prompt = page.getByLabel("자동 개선 프롬프트", { exact: true });
  await expect(prompt).toHaveAttribute("placeholder", "기본 개선 프롬프트");
  await page.getByRole("checkbox", { name: "유휴 시간 자동 문서 개선" }).check();
  await prompt.fill("이 프로젝트 전용 프롬프트");
  await page.getByRole("button", { name: "프로젝트 저장", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("프로젝트를 저장했습니다");
  await page.screenshot({ path: "test-artifacts/automation-project.png", fullPage: true });

  const state = await (await request.get("/api/state")).json();
  expect(state.config.automation).toMatchObject({
    enabled: true,
    prompt: "기본 개선 프롬프트",
    stale_days: 3,
  });
  expect(state.config.projects[0]).toMatchObject({
    auto_improve: true,
    auto_prompt: "이 프로젝트 전용 프롬프트",
  });
  expect(state.automation.runs).toEqual([]);

  // Leave the shared fixture server as the other tests expect it.
  await page.getByRole("checkbox", { name: "유휴 시간 자동 문서 개선" }).uncheck();
  await prompt.fill("");
  await page.getByRole("button", { name: "프로젝트 저장", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("프로젝트를 저장했습니다");
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByRole("button", { name: "자동 문서 개선" }).click();
  await page.getByRole("checkbox", { name: "자동 문서 개선 사용" }).uncheck();
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "설정을 저장했습니다" })).toBeVisible();
  expect(errors).toEqual([]);
});

test("an automatic session is never selected on the user's behalf", async ({
  page,
  request,
}) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const state = await (await request.get("/api/state")).json();
  const auto = {
    id: "automatic-session",
    project: state.config.projects[0],
    status: "running",
    title: "자동 개선 프롬프트",
    workflow_mode: "source_document",
    automatic: true,
    memory_count: 0,
    revision: state.revision,
  };
  state.sessions = [auto];
  state.running = [{ id: auto.id, closing: false, automatic: true }];
  let opened = 0;
  await page.route("**/api/state", (route) => route.fulfill({ json: state }));
  await page.route(`**/api/sessions/${auto.id}*`, (route) => {
    opened++;
    return route.fulfill({ status: 404, json: { error: "세션을 찾을 수 없습니다." } });
  });
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "첫 작업을 시작해 보세요" })).toBeVisible();
  await expect(page.getByRole("button", { name: /자동 개선 프롬프트/ })).toContainText("자동 개선 ·");
  expect(page.url()).not.toContain(auto.id);
  expect(opened).toBe(0);
  expect(errors).toEqual([]);
});

// An automatic session closes by itself when its run ends; closing one from
// outside the page changes the selection the same way.
async function selectThenCloseOutside(page, request, edit) {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const headers = { "x-mnemoarc-client": "web" };
  const { config } = await (await request.get("/api/state")).json();
  const created = await request.post("/api/sessions", {
    headers,
    data: { project: config.projects[0], workflow: "answer" },
  });
  expect(created.ok()).toBe(true);
  const { id } = await created.json();
  await page.goto(`/#${id}`);
  await page.getByRole("button", { name: "모든 설정" }).click();
  await edit();
  expect((await request.delete(`/api/sessions/${id}`, { headers })).ok()).toBe(true);
  await expect.poll(() => page.evaluate(() => location.hash)).not.toBe(`#${id}`);
  return { config, errors };
}

test("unsaved app-wide settings survive the selected session closing on its own", async ({
  page,
  request,
}) => {
  const count = page.getByLabel("최대 기억 개수", { exact: true });
  const { errors } = await selectThenCloseOutside(page, request, async () => {
    await page.getByRole("button", { name: "기억과 보관" }).click();
    await count.fill("777");
  });
  await expect(count).toHaveValue("777");
  await expect(page.getByText("저장하지 않은 변경 사항", { exact: true })).toBeVisible();
  expect(errors).toEqual([]);
});

test("edits for a session that closed on its own fall back to app-wide settings", async ({
  page,
  request,
}) => {
  const scope = page.getByLabel("설정 적용 범위");
  const count = page.getByLabel("최대 기억 개수", { exact: true });
  const { config, errors } = await selectThenCloseOutside(page, request, async () => {
    await scope.selectOption("session");
    await page.getByRole("button", { name: "기억과 보관" }).click();
    await count.fill("555");
  });
  await expect(scope).toHaveValue("global");
  await expect(
    page.getByRole("status").filter({ hasText: "전체 기본 설정으로 전환했습니다" }),
  ).toBeVisible();
  await expect(count).toHaveValue(String(config.memory_count));
  expect(errors).toEqual([]);
});
