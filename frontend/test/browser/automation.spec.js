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
