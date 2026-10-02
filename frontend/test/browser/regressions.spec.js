import { test, expect } from "./fixtures.js";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";

test("project edits require confirmation before leaving", async ({ page }) => {
  await page.goto("/");
  await page
    .locator(".sidebar-bottom")
    .getByRole("button", { name: "프로젝트 관리" })
    .click();
  const name = page.getByLabel("프로젝트 이름");
  const original = await name.inputValue();
  await name.fill("unsaved-project-review");
  const settings = page.getByRole("button", { name: "모든 설정" });

  page.once("dialog", (dialog) => dialog.dismiss());
  await settings.click();
  await expect(name).toHaveValue("unsaved-project-review");

  page.once("dialog", (dialog) => dialog.accept());
  await settings.click();
  await expect(page.getByRole("heading", { name: "설정" })).toBeVisible();
  await page
    .locator(".sidebar-bottom")
    .getByRole("button", { name: "프로젝트 관리" })
    .click();
  await expect(page.getByLabel("프로젝트 이름")).toHaveValue(original);
});

test("folder picker ignores an older directory response", async ({ page, request }) => {
  const state = await (await request.get("/api/state")).json();
  const root = state.config.projects[0].root;
  const first = join(root, "a-picker-review");
  const second = join(root, "b-picker-review");
  await Promise.all([
    mkdir(first, { recursive: true }),
    mkdir(second, { recursive: true }),
  ]);
  await page.goto("/");
  await page
    .locator(".sidebar-bottom")
    .getByRole("button", { name: "프로젝트 관리" })
    .click();

  let releaseFirst;
  let sawFirst;
  const held = new Promise((resolve) => { releaseFirst = resolve; });
  const firstRequested = new Promise((resolve) => { sawFirst = resolve; });
  await page.route("**/api/directories?*", async (route) => {
    const path = new URL(route.request().url()).searchParams.get("path");
    if (path === first) {
      sawFirst();
      await held;
    }
    await route.continue();
  });
  await page.getByRole("button", { name: "폴더 선택" }).click();
  await page.getByRole("button", { name: /a-picker-review/ }).click();
  await firstRequested;
  await page.getByRole("button", { name: /b-picker-review/ }).click();
  const currentPath = page.locator(".modal .directory-path");
  await expect(currentPath).toHaveText(second);
  const olderResponse = page.waitForResponse((response) =>
    new URL(response.url()).searchParams.get("path") === first,
  );
  releaseFirst();
  await olderResponse;
  await page.evaluate(() => new Promise((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(resolve)),
  ));
  await expect(currentPath).toHaveText(second);
  await page.getByRole("button", { name: "이 폴더 선택" }).click();
  await expect(page.getByLabel("소스 폴더")).toHaveValue(second);
});

test("session connection check uses the session endpoint", async ({ page, request }) => {
  const state = await (await request.get("/api/state")).json();
  const id = state.sessions[0].id;
  await page.goto("/");
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("설정 적용 범위").selectOption("session");
  await page.route(`**/api/sessions/${id}/check`, (route) =>
    route.fulfill({ json: { message: "OK" } }),
  );
  const checked = page.waitForRequest((pending) =>
    pending.url().endsWith(`/api/sessions/${id}/check`),
  );
  await page.getByRole("button", { name: "연결 확인" }).click();
  await checked;
  await expect(page.getByRole("status")).toContainText("연결 확인 완료");
});

test("stale refresh does not replace a newly created session", async ({ page, request }) => {
  await page.goto("/");
  const stale = await (await request.get("/api/state")).json();
  const firstProject = stale.config.projects[0];
  const initialId = stale.sessions[0].id;
  const existingCount = stale.sessions.filter(
    (session) => session.project.root === firstProject.root,
  ).length;

  let releaseState;
  let sawState;
  let deliveredState;
  const held = new Promise((resolve) => { releaseState = resolve; });
  const stateRequested = new Promise((resolve) => { sawState = resolve; });
  const stateDelivered = new Promise((resolve) => { deliveredState = resolve; });
  let intercept = true;
  await page.route("**/api/state", async (route) => {
    if (intercept) {
      intercept = false;
      sawState();
      await held;
      await route.fulfill({ json: stale, headers: { "X-Review-Stale": "yes" } });
      deliveredState();
    } else {
      await route.continue();
    }
  });
  const external = await request.post("/api/sessions", {
    data: { project: firstProject },
    headers: { "X-MnemoArc-Client": "web" },
  });
  expect(external.ok()).toBe(true);
  const externalId = (await external.json()).id;
  await stateRequested;
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await page.getByRole("button", { name: "세션 시작", exact: true }).click();
  await expect.poll(async () => {
    const id = await page.evaluate(() => location.hash.slice(1));
    return id && id !== initialId && id !== externalId ? id : null;
  }).not.toBeNull();
  const createdId = await page.evaluate(() => location.hash.slice(1));
  releaseState();
  // Navigation may abort the obsolete fetch, which has no response event.
  // Wait for the fixture to finish delivering it before checking selection.
  await stateDelivered;
  await page.evaluate(() => new Promise((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(resolve)),
  ));

  const rows = page.locator(".project-group .session-row");
  await expect(rows).toHaveCount(existingCount + 2);
  await expect(rows.last()).toHaveClass(/active/);
  await expect(page).toHaveURL(new RegExp(`#${createdId}$`));

  for (const id of [createdId, externalId]) {
    const deleted = await request.delete(`/api/sessions/${id}`, {
      headers: { "X-MnemoArc-Client": "web" },
    });
    expect(deleted.ok()).toBe(true);
  }
});
