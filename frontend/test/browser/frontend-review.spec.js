import { test, expect } from "@playwright/test";

const pageErrors = new WeakMap();
test.beforeEach(async ({ page }) => {
  const errors = [];
  pageErrors.set(page, errors);
  page.on("pageerror", (error) => errors.push(error.message));
});
test.afterEach(async ({ page }) => {
  expect(pageErrors.get(page)).toEqual([]);
});

// Keep review cases independent of the live provider and of other tests' sessions.
async function workspace({ page, request }, patch = {}) {
  const state = await (await request.get("/api/state")).json();
  const original = await (await request.get(`/api/sessions/${state.sessions[0].id}`)).json();
  const session = {
    ...original,
    status: "idle",
    has_task: false,
    bundles: [],
    previous: null,
    pruned_through: null,
    stream: "",
    error: null,
    run_history: [],
    memories: [],
    workflow_mode: "answer",
    ...patch,
    config: { ...original.config, model: "review-fixture", model_context: 128000 },
  };
  state.config = structuredClone(session.config);
  state.running = [];
  state.sessions = [{ ...state.sessions[0], status: "idle", title: "리뷰 세션" }];
  await page.route("**/api/state", (route) => route.fulfill({ json: state }));
  await page.route(`**/api/sessions/${session.id}`, (route) => route.fulfill({ json: session }));
  await page.goto(`/#${session.id}`);
  await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toBeVisible();
  return { state, session };
}

function gate() {
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  let seen;
  const requested = new Promise((resolve) => { seen = resolve; });
  return { held, release, seen, requested };
}

test("settings preserve local edits while incorporating refreshed server fields", async ({ page, request }) => {
  const { state } = await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("현재 세션에도 적용").uncheck();
  await page.getByLabel("모델 이름", { exact: true }).fill("local-model");
  state.config.model_context = 256000;
  state.config.database.host = "updated-server";
  state.config.projects[0].name = "다른 창에서 수정한 프로젝트";
  await expect(page.getByLabel("모델 최대 컨텍스트", { exact: true })).toHaveValue("256000");
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("local-model");
  let submitted;
  await page.route("**/api/settings", (route) => {
    submitted = route.request().postDataJSON().config;
    state.config = structuredClone(submitted);
    return route.fulfill({ json: { saved: true } });
  });
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect.poll(() => submitted?.model).toBe("local-model");
  expect(submitted.database.host).toBe("updated-server");
  expect(submitted.projects[0].name).toBe("다른 창에서 수정한 프로젝트");
  // Once saved, subsequent server edits must no longer be treated as our draft.
  await expect(page.getByRole("button", { name: "설정 저장", exact: true })).toBeEnabled();
  state.config.model = "newer-server-model";
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("newer-server-model");
});

test("an unedited project list follows external changes and keeps a valid selection", async ({ page, request }) => {
  const { state } = await workspace({ page, request });
  state.config.projects.push({ ...state.config.projects[0], name: "두 번째 프로젝트" });
  await expect(page.locator(".project-heading").filter({ hasText: "두 번째 프로젝트" })).toBeVisible();
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  await page.locator(".settings-tabs").getByRole("button", { name: "두 번째 프로젝트" }).click();
  state.config.projects = [{ ...state.config.projects[0], name: "외부에서 남긴 프로젝트" }];
  await expect(page.getByLabel("프로젝트 이름", { exact: true })).toHaveValue("외부에서 남긴 프로젝트");
});

test("session project edits do not overwrite refreshed untouched fields", async ({ page, request }) => {
  const { session } = await workspace({ page, request });
  await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
  await page.getByLabel("프로젝트 이름", { exact: true }).fill("로컬 프로젝트 이름");
  session.project.purpose = "다른 창에서 갱신한 작업 목적";
  session.revision++;
  await expect(page.getByLabel(/^작업 목적/)).toHaveValue("다른 창에서 갱신한 작업 목적");
  await expect(page.getByLabel("프로젝트 이름", { exact: true })).toHaveValue("로컬 프로젝트 이름");
  let submitted;
  await page.route("**/api/sessions/*/project", (route) => {
    submitted = route.request().postDataJSON();
    return route.fulfill({ json: { saved: true } });
  });
  await page.getByRole("button", { name: "현재 세션에 적용" }).click();
  await expect.poll(() => submitted?.purpose).toBe("다른 창에서 갱신한 작업 목적");
});

test("session navigation does not wait for a stalled previous session refresh", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  const other = { ...session, id: "second-review-session", project: { ...session.project, name: "다음 프로젝트" } };
  state.sessions.push({ ...state.sessions[0], id: other.id, title: "다음 세션" });
  await page.route(`**/api/sessions/${other.id}`, (route) => route.fulfill({ json: other }));
  const pending = gate();
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ status: 503, json: { error: "이전 세션의 늦은 오류" } });
  });
  await pending.requested;
  try {
    await page.locator(".session-button").filter({ hasText: "다음 세션" }).click();
    await expect(page.locator(".session-heading h2")).toHaveText("다음 프로젝트");
    await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toBeVisible();
  } finally {
    pending.release();
  }
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("folder picker confines keyboard focus and Escape restores its opener", async ({ page, request }) => {
  await workspace({ page, request });
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  const opener = page.getByRole("button", { name: "폴더 선택", exact: true });
  await opener.click();
  const picker = page.getByRole("dialog", { name: "프로젝트 폴더 선택" });
  await expect(picker).toBeVisible();
  await expect.poll(() => picker.evaluate((element) => element.contains(document.activeElement))).toBe(true);
  await expect(picker.getByRole("button", { name: "이 폴더 선택" })).toBeEnabled();
  await page.screenshot({ path: "test-artifacts/directory-dialog-desktop.png" });
  await page.setViewportSize({ width: 390, height: 680 });
  await page.screenshot({ path: "test-artifacts/directory-dialog-mobile.png" });
  const bounds = await picker.boundingBox();
  expect(bounds.x).toBeGreaterThanOrEqual(0);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(390);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(680);
  await picker.getByRole("button", { name: "폴더 선택 닫기" }).focus();
  await page.keyboard.press("Shift+Tab");
  await expect.poll(() => picker.evaluate((element) => element.contains(document.activeElement))).toBe(true);
  await page.keyboard.press("Escape");
  await expect(picker).toHaveCount(0);
  await expect(opener).toBeFocused();
});

test("session settings follow their own server snapshot when the scope changes", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("설정 적용 범위").selectOption("session");
  await page.getByLabel("모델 이름", { exact: true }).fill("local-session-model");
  state.config.model_context = 512000;
  session.config.model_context = 256000;
  session.revision++;
  await expect(page.getByLabel("모델 최대 컨텍스트", { exact: true })).toHaveValue("256000");
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("local-session-model");
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByLabel("설정 적용 범위").selectOption("global");
  await expect(page.getByLabel("모델 최대 컨텍스트", { exact: true })).toHaveValue("512000");
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue(state.config.model);
});

test("API key storage guidance follows the selected settings scope", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("이 기기에 키 저장").check();
  const guidance = page.locator(".credential-card small");
  await expect(guidance).toContainText("별도 설정 파일에 저장");

  await page.getByLabel("설정 적용 범위").selectOption("session");
  await expect(page.getByLabel("이 기기에 키 저장")).toHaveCount(0);
  await expect(guidance).toContainText("기기에 저장되지 않으며");
  await page.getByLabel("API 키", { exact: true }).fill("temporary-review-key");
  let submitted;
  await page.route("**/api/sessions/*/settings", (route) => {
    submitted = route.request().postDataJSON();
    return route.fulfill({ json: { pending: false } });
  });
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect.poll(() => submitted?.credential_mode).toBe("session");

  await page.getByLabel("설정 적용 범위").selectOption("global");
  await expect(page.getByLabel("이 기기에 키 저장")).not.toBeChecked();
});

test("an open folder picker follows a refreshed project root", async ({ page, request }) => {
  const { session } = await workspace({ page, request });
  await page.route("**/api/directories*", (route) => route.fulfill({ json: {
    path: new URL(route.request().url()).searchParams.get("path"), parent: null, directories: [],
  } }));
  await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
  await page.getByRole("button", { name: "폴더 선택", exact: true }).click();
  const picker = page.getByRole("dialog", { name: "프로젝트 폴더 선택" });
  await expect(picker.locator(".directory-path")).toHaveText(session.project.root);
  session.project.root = "/review/updated-project";
  session.revision++;
  await expect(picker.locator(".directory-path")).toHaveText(session.project.root);
  await expect(picker.getByRole("button", { name: "이 폴더 선택" })).toBeEnabled();
  await picker.getByRole("button", { name: "이 폴더 선택" }).click();
  await expect(page.getByRole("textbox", { name: /^소스 폴더/ })).toHaveValue(session.project.root);
});

test("a connection check is invalidated when untouched server settings change", async ({ page, request }) => {
  const { state } = await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  const pending = gate();
  await page.route("**/api/check", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { ok: true } });
  });
  await page.getByRole("button", { name: "연결 확인", exact: true }).click();
  await pending.requested;
  state.config.model = "changed-during-check";
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue(state.config.model);
  pending.release();
  await expect(page.getByRole("status")).toContainText("현재 설정으로 다시 연결을 확인하세요");
});

test("a stalled refresh times out and allows the next poll to recover", async ({ page, request }) => {
  await page.clock.install();
  const { session } = await workspace({ page, request });
  const pending = gate();
  let calls = 0;
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    if (++calls === 1) {
      pending.seen();
      await pending.held;
    }
    await route.fulfill({ json: session });
  });
  await page.clock.runFor(3200);
  await pending.requested;
  session.project.name = "응답 지연 후 복구된 프로젝트";
  try {
    await page.clock.runFor(15200);
    await expect.poll(() => calls).toBeGreaterThanOrEqual(2);
    await expect(page.locator(".session-heading h2")).toHaveText(session.project.name);
    await expect(page.getByRole("alert")).toHaveCount(0);
  } finally {
    pending.release();
  }
});

test("a delayed old page leaves a gap available for the next history request", async ({ page, request }) => {
  const bundle = (id) => ({ id, messages: [{ role: "user", content: `연속 기록 ${id}` }] });
  const { session } = await workspace({ page, request }, { bundles: [bundle(3), bundle(4)], previous: 3 });
  const pending = gate();
  await page.route("**/api/sessions/*?before=*", async (route) => {
    const older = { ...session, bundles: [bundle(1), bundle(2)], previous: null };
    pending.seen();
    await pending.held;
    await route.fulfill({ json: older });
  });
  await page.getByRole("button", { name: "이전 대화 더 보기" }).click();
  await pending.requested;
  session.bundles = [bundle(7), bundle(8)];
  session.previous = 7;
  session.revision++;
  await expect(page.locator(".chat-content")).toContainText("연속 기록 8");
  pending.release();
  const more = page.getByRole("button", { name: "이전 대화 더 보기" });
  await expect(more).toBeEnabled();
  let cursor;
  await page.route("**/api/sessions/*?before=*", (route) => {
    cursor = new URL(route.request().url()).searchParams.get("before");
    return route.fulfill({ json: { ...session, bundles: [3, 4, 5, 6].map(bundle), previous: 3 } });
  });
  await more.click();
  await expect.poll(() => cursor).toBe("7");
  await expect(page.locator(".chat-content")).toContainText("연속 기록 5");
});

test("run acknowledgement waits for refreshed state before another send is enabled", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  const refresh = gate();
  const running = gate();
  await page.route("**/api/sessions/*/run", async (route) => {
    await page.route(`**/api/sessions/${session.id}`, async (detail) => {
      refresh.seen();
      await refresh.held;
      await detail.fulfill({ json: session });
    });
    running.seen();
    await route.fulfill({ json: { started: true } });
  });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  const submit = page.getByRole("button", { name: "메시지 보내기" });
  await input.fill("첫 요청");
  await submit.click();
  await running.requested;
  await refresh.requested;
  await input.fill("다음 요청 초안");
  try {
    await expect(submit).toBeDisabled();
  } finally {
    state.running = [{ id: session.id, closing: false }];
    session.status = "running";
    refresh.release();
  }
  await expect(input).toHaveValue("다음 요청 초안");
});

test("clicking the selected session preserves the unsent message", async ({ page, request }) => {
  await workspace({ page, request });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("작성 중인 메시지");
  await page.locator(".session-row.active .session-button").click();
  await expect(input).toHaveValue("작성 중인 메시지");
});

test("the displayed question mode also applies to continuation phrases", async ({ page, request }) => {
  await workspace({ page, request }, { has_task: true });
  let submitted;
  await page.route("**/api/sessions/*/run", (route) => {
    submitted = route.request().postDataJSON();
    return route.fulfill({ json: { started: true } });
  });
  await expect(page.getByLabel("요청 종류")).toHaveValue("question");
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("계속");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect.poll(() => submitted?.action).toBe("question");
});

test("workflow changes finish before a message can start", async ({ page, request }) => {
  const { session } = await workspace({ page, request });
  const pending = gate();
  const runs = [];
  await page.route("**/api/sessions/*/workflow", async (route) => {
    pending.seen();
    await pending.held;
    session.workflow_mode = route.request().postDataJSON().workflow;
    await route.fulfill({ json: { saved: true } });
  });
  await page.route("**/api/sessions/*/run", (route) => {
    runs.push(session.workflow_mode);
    return route.fulfill({ json: { started: true } });
  });
  await page.getByLabel("작업 방식").selectOption("source_document");
  await pending.requested;
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("문서 작성 요청");
  try {
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
    await expect(page.getByRole("button", { name: "재개", exact: true })).toBeDisabled();
    await expect(page.getByRole("button", { name: "기억 정리", exact: true })).toBeDisabled();
    await input.press("Enter");
    expect(runs).toEqual([]);
  } finally {
    pending.release();
  }
  await expect(page.getByLabel("작업 방식")).toHaveValue("source_document");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect.poll(() => runs).toEqual(["source_document"]);
});

test("settings saved in flight do not mark later edits or keys as saved", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("현재 세션에도 적용").uncheck();
  const model = page.getByLabel("모델 이름", { exact: true });
  const key = page.getByLabel("API 키", { exact: true });
  await model.fill("submitted-model");
  await key.fill("submitted-test-key");
  const pending = gate();
  await page.route("**/api/settings", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { saved: true } });
  });
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await pending.requested;
  await expect(page.getByLabel("설정 적용 범위")).toBeDisabled();
  await model.fill("later-model");
  await key.fill("later-test-key");
  pending.release();
  await expect(page.getByRole("button", { name: "설정 저장", exact: true })).toBeEnabled();
  await expect(model).toHaveValue("later-model");
  await expect(key).toHaveValue("later-test-key");
  await expect(page.locator(".settings-actions")).toContainText("저장하지 않은 변경 사항");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.locator(".brand").click();
  await expect(model).toBeVisible();
});

test("project save preserves the dirty state of edits made during the request", async ({ page, request }) => {
  await workspace({ page, request });
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  const name = page.getByLabel("프로젝트 이름", { exact: true });
  await name.fill("submitted-project");
  const pending = gate();
  await page.route("**/api/settings", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { saved: true } });
  });
  await page.getByRole("button", { name: "프로젝트 저장", exact: true }).click();
  await pending.requested;
  await name.fill("later-project");
  pending.release();
  await expect(page.getByRole("button", { name: "프로젝트 저장", exact: true })).toBeEnabled();
  await expect(name).toHaveValue("later-project");
  await expect(page.getByText("저장하지 않은 변경 사항이 있습니다.")).toBeVisible();
});

test("a failed session creation retains the project draft", async ({ page, request }) => {
  await workspace({ page, request });
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  await page.getByLabel("프로젝트 이름", { exact: true }).fill("보존해야 할 프로젝트");
  await page.route("**/api/sessions", (route) => route.fulfill({
    status: 400, json: { error: "프로젝트 폴더를 확인하세요." },
  }));
  page.on("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "이 프로젝트로 새 세션" }).click();
  await expect(page.getByLabel("프로젝트 이름", { exact: true })).toHaveValue("보존해야 할 프로젝트");
  await expect(page.getByRole("alert").first()).toContainText("프로젝트 폴더를 확인하세요.");
});

test("a new session uses the unsaved project draft only after an accurate confirmation", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  await page.getByLabel("프로젝트 이름", { exact: true }).fill("초안 프로젝트");
  await page.getByLabel(/^결과 문서 경로/).fill("docs/draft-output.md");

  let submitted;
  const created = { ...session, id: "project-draft-session" };
  await page.route("**/api/sessions/project-draft-session", (route) => route.fulfill({ json: created }));
  await page.route("**/api/sessions", (route) => {
    submitted = route.request().postDataJSON().project;
    created.project = submitted;
    state.sessions.push({ ...state.sessions[0], id: created.id, project: submitted });
    return route.fulfill({ json: { id: created.id } });
  });

  let confirmation;
  page.once("dialog", async (dialog) => {
    confirmation = dialog.message();
    await dialog.dismiss();
  });
  await page.getByRole("button", { name: "이 프로젝트로 새 세션" }).click();
  expect(confirmation).toContain("새 세션에만 적용");
  expect(confirmation).toContain("프로젝트 목록 변경 사항은 저장되지 않습니다");
  expect(submitted).toBeUndefined();

  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "이 프로젝트로 새 세션" }).click();
  await expect.poll(() => submitted?.output).toBe("docs/draft-output.md");
  await expect(page.locator(".session-heading h2")).toHaveText("초안 프로젝트");
  expect(state.config.projects[0].name).not.toBe("초안 프로젝트");
});

test("a malformed successful API response does not discard a message", async ({ page, request }) => {
  await workspace({ page, request });
  await page.route("**/api/sessions/*/run", (route) => route.fulfill({
    status: 200, contentType: "text/html", body: "<html>temporary gateway error</html>",
  }));
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("실패하면 보존할 메시지");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.getByRole("alert")).toContainText("서버 응답을 읽지 못했습니다");
  await expect(input).toHaveValue("실패하면 보존할 메시지");
});

test("numeric fields stay empty while being edited", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  const budget = page.getByLabel("요청 컨텍스트 예산", { exact: true });
  await budget.fill("");
  await expect(budget).toHaveValue("");
  await page.getByRole("button", { name: /^데이터베이스/ }).click();
  const port = page.getByLabel("포트", { exact: true });
  await port.fill("");
  await expect(port).toHaveValue("");
});

test("an older history response cannot restore already pruned messages", async ({ page, request }) => {
  const bundle = (id) => ({ id, messages: [{ role: "user", content: `기록 ${id}` }] });
  const { session } = await workspace({ page, request }, { bundles: [bundle(3)], previous: 3 });
  const pending = gate();
  await page.route("**/api/sessions/*?before=*", async (route) => {
    pending.seen();
    const older = { ...session, bundles: [bundle(1), bundle(2)], previous: null };
    await pending.held;
    await route.fulfill({ json: older });
  });
  await page.getByRole("button", { name: "이전 대화 더 보기" }).click();
  await pending.requested;
  session.revision++;
  session.pruned_through = 2;
  session.previous = null;
  await expect(page.getByText("오래된 원문 일부가 보관 한도에 따라 정리되었습니다.", { exact: false })).toBeVisible();
  // Observe the merge itself, before a subsequent poll could hide resurrected rows.
  const refresh = gate();
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    await refresh.held;
    await route.fulfill({ json: session });
  });
  const response = page.waitForResponse((res) => res.url().includes("?before="));
  pending.release();
  await response;
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  try {
    await expect(page.getByRole("button", { name: "이전 대화 더 보기" })).toHaveCount(0);
    await expect(page.locator(".chat-content")).not.toContainText("기록 1");
    await expect(page.locator(".chat-content")).not.toContainText("기록 2");
    await expect(page.locator(".chat-content")).toContainText("기록 3");
  } finally {
    refresh.release();
  }
});

test("footnote navigation preserves the selected session URL", async ({ page, request }) => {
  const { session } = await workspace({ page, request }, { bundles: [{
    id: 1, messages: [{ role: "assistant", content: "본문[^1]\n\n[^1]: 각주 내용" }],
  }] });
  await page.locator(".chat-content a[data-footnote-ref]").click();
  await expect(page).toHaveURL(new RegExp(`#${session.id}$`));
  await page.locator(".chat-content a[data-footnote-backref]").click();
  await expect(page).toHaveURL(new RegExp(`#${session.id}$`));
});

test("memory selection ignores an older detail response and tracks revisions", async ({ page, request }) => {
  const memories = [1, 2].map((n) => ({ id: `M${n}`, title: `기억 ${n}`, summary: "내용", revision: 1, status: "active", tags: [] }));
  const { session } = await workspace({ page, request }, { memories });
  const pending = gate();
  await page.route("**/api/sessions/*/memories/*", async (route) => {
    const first = route.request().url().endsWith("/M1");
    const memory = { ...session.memories[first ? 0 : 1], body: first ? "이전 선택 내용" : `최신 선택 개정 ${session.memories[1].revision}`, sources: [] };
    if (first) { pending.seen(); await pending.held; }
    await route.fulfill({ json: memory });
  });
  await page.getByRole("button", { name: /기억 1/ }).click();
  await pending.requested;
  await page.getByRole("button", { name: /기억 2/ }).click();
  await expect(page.locator(".memory-detail")).toContainText("최신 선택 개정 1");
  pending.release();
  session.memories[1].revision = 2;
  session.revision++;
  await expect(page.locator(".memory-detail")).toContainText("최신 선택 개정 2");
  await expect(page.locator(".memory-detail")).not.toContainText("이전 선택 내용");
  session.memories = [];
  session.revision++;
  await expect(page.locator(".memory-detail")).toHaveCount(0);
});

test("unsaved session project edits survive a cancelled panel close", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
  const name = page.getByLabel("프로젝트 이름", { exact: true });
  await name.fill("세션 프로젝트 초안");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "상세 패널 표시" }).click();
  await expect(name).toHaveValue("세션 프로젝트 초안");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "모든 설정" }).click();
  await expect(name).toHaveValue("세션 프로젝트 초안");
});

test("changing the output path invalidates the displayed document", async ({ page, request }) => {
  const { session } = await workspace({ page, request });
  await page.route("**/api/sessions/*/output", (route) => route.fulfill({ json: { content: "이전 문서 본문", truncated: false } }));
  await page.getByRole("tab", { name: "문서", exact: true }).click();
  await page.getByRole("button", { name: "문서 불러오기" }).click();
  await expect(page.locator(".inspector")).toContainText("이전 문서 본문");
  session.project = { ...session.project, output: "changed.md" };
  session.revision++;
  await expect(page.locator(".inspector .directory-path")).toHaveText("changed.md");
  await expect(page.locator(".inspector")).not.toContainText("이전 문서 본문");
  await expect(page.getByRole("button", { name: "Markdown 내려받기" })).toHaveCount(0);
});

test("a save finishing after navigation does not clear a new editor's dirty state", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("현재 세션에도 적용").uncheck();
  await page.getByLabel("모델 이름", { exact: true }).fill("first-editor");
  const pending = gate();
  await page.route("**/api/settings", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { saved: true } });
  });
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await pending.requested;
  page.once("dialog", (dialog) => dialog.accept());
  await page.locator(".brand").click();
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("모델 이름", { exact: true }).fill("second-editor");
  const response = page.waitForResponse((res) => res.url().endsWith("/api/settings"));
  pending.release();
  await response;
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.locator(".brand").click();
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("second-editor");
});

test("late session creation does not discard edits on another page", async ({ page, request }) => {
  await workspace({ page, request });
  const pending = gate();
  await page.route("**/api/sessions", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { id: "new-session" } });
  });
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await pending.requested;
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("모델 이름", { exact: true }).fill("new-page-draft");
  const response = page.waitForResponse((res) => res.url().endsWith("/api/sessions"));
  pending.release();
  await response;
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("new-page-draft");
});

test("the old chat draft is locked during creation and restored on failure", async ({ page, request }) => {
  await workspace({ page, request });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("기존 세션의 초안");
  const pending = gate();
  await page.route("**/api/sessions", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ status: 400, json: { error: "세션 생성 실패" } });
  });
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await pending.requested;
  try {
    await expect(input).toBeDisabled();
    await expect(input).toHaveValue("기존 세션의 초안");
  } finally {
    pending.release();
  }
  await expect(input).toBeEnabled();
  await expect(input).toHaveValue("기존 세션의 초안");
  await expect(page.getByRole("alert")).toContainText("세션 생성 실패");
});

test("a removed session cannot send through the replacement session while it loads", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  const pending = gate();
  await page.route("**/api/sessions/replacement", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { ...session, id: "replacement" } });
  });
  state.sessions = [{ ...state.sessions[0], id: "replacement" }];
  await pending.requested;
  try {
    await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toHaveCount(0);
  } finally {
    pending.release();
  }
  await expect(page).toHaveURL(/#replacement$/);
  await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toBeVisible();
});

test("a failed workflow change restores the selection and releases send controls", async ({ page, request }) => {
  await workspace({ page, request });
  await page.route("**/api/sessions/*/workflow", (route) => route.fulfill({ status: 409, json: { error: "작업 방식 변경 실패" } }));
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("보존할 요청");
  await page.getByLabel("작업 방식").selectOption("source_document");
  await expect(page.getByRole("alert")).toContainText("작업 방식 변경 실패");
  await expect(page.getByLabel("작업 방식")).toHaveValue("answer");
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
});

test("settings report partial success when only session application fails", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("모델 이름", { exact: true }).fill("saved-globally");
  await page.route("**/api/settings", (route) => route.fulfill({ json: { saved: true } }));
  await page.route("**/api/sessions/*/settings", (route) => route.fulfill({ status: 409, json: { error: "세션 적용 실패" } }));
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("전체 기본 설정은 저장했지만 현재 세션에는 적용하지 못했습니다");
  await expect(page.getByRole("alert")).toContainText("세션 적용 실패");
  await expect(page.locator(".settings-actions")).toContainText("저장하지 않은 변경 사항");
});

test("a connection check only validates the configuration that was submitted", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("button", { name: "모든 설정" }).click();
  const pending = gate();
  await page.route("**/api/check", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { message: "OK" } });
  });
  await page.getByRole("button", { name: "연결 확인", exact: true }).click();
  await pending.requested;
  await page.getByLabel("모델 이름", { exact: true }).fill("unchecked-model");
  pending.release();
  await expect(page.getByRole("status")).toContainText("현재 설정으로 다시 연결을 확인하세요");
});
