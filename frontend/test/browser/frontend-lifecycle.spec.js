import { test, expect, workspace, gate, addSession, waitForSnapshot } from "./review-fixtures.js";

test("chat text and request kind survive settings and project navigation", async ({ page, request }) => {
  await workspace({ page, request }, { has_task: true });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await page.getByLabel("요청 종류").selectOption("chat");
  await input.fill("설정을 확인하고 보낼 새 작업");
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByRole("button", { name: "채팅으로 돌아가기" }).click();
  await expect(input).toHaveValue("설정을 확인하고 보낼 새 작업");
  await expect(page.getByLabel("요청 종류")).toHaveValue("chat");
  await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리" }).click();
  await page.locator(".brand").click();
  await expect(input).toHaveValue("설정을 확인하고 보낼 새 작업");
});

test("each session retains its own unsent chat draft", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  await addSession(page, state, session);
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("첫 세션 초안");
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(input).toHaveValue("");
  await input.fill("둘째 세션 초안");
  await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
  await expect(input).toHaveValue("첫 세션 초안");
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(input).toHaveValue("둘째 세션 초안");
});

for (const edited of [false, true]) {
  test(`a late successful send after remount ${edited ? "preserves later edits" : "clears the submitted draft"}`, async ({ page, request }) => {
    await workspace({ page, request });
    const pending = gate();
    await page.route("**/api/sessions/*/run", async (route) => {
      pending.seen();
      await pending.held;
      await route.fulfill({ json: { started: true } });
    });
    const input = page.getByRole("textbox", { name: "메시지", exact: true });
    await input.fill("제출한 요청");
    await page.getByRole("button", { name: "메시지 보내기" }).click();
    await pending.requested;
    await page.getByRole("button", { name: "모든 설정" }).click();
    await page.getByRole("button", { name: "채팅으로 돌아가기" }).click();
    await expect(input).toHaveValue("제출한 요청");
    if (edited) await input.fill("화면을 다시 열고 쓴 초안");
    pending.release();
    await expect(page.getByRole("button", { name: "재개", exact: true })).toBeEnabled();
    await expect(input).toHaveValue(edited ? "화면을 다시 열고 쓴 초안" : "");
  });
}

test("failed sends retain drafts and release the shared lock after remount", async ({ page, request }) => {
  await workspace({ page, request });
  const pending = gate();
  await page.route("**/api/sessions/*/run", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ status: 503, json: { error: "전송 실패" } });
  });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("실패해도 남겨 둘 요청");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await pending.requested;
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByRole("button", { name: "채팅으로 돌아가기" }).click();
  pending.release();
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
  await expect(input).toHaveValue("실패해도 남겨 둘 요청");
  await expect(page.getByRole("alert")).toContainText("전송 실패");
});

test("unsent drafts protect browser exit even while settings are open", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("아직 보내지 않은 내용");
  await page.getByRole("button", { name: "모든 설정" }).click();
  const prevented = await page.evaluate(() => {
    const event = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(event);
    return event.defaultPrevented;
  });
  expect(prevented).toBe(true);
});

test("the detail panel starts closed on narrow screens and remains available on demand", async ({ page, request }) => {
  await page.setViewportSize({ width: 390, height: 740 });
  await workspace({ page, request });
  const toggle = page.getByRole("button", { name: "상세 패널 표시" });
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator(".inspector")).toHaveCount(0);
  await toggle.click();
  await expect(page.locator(".inspector")).toBeVisible();
  await toggle.click();
  await expect(page.locator(".inspector")).toHaveCount(0);
});

test("resizing a desktop chat to a narrow screen reveals the composer", async ({ page, request }) => {
  await workspace({ page, request });
  await expect(page.locator(".inspector")).toBeVisible();
  await page.setViewportSize({ width: 390, height: 740 });
  await expect(page.getByRole("button", { name: "상세 패널 표시" })).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator(".inspector")).toHaveCount(0);
});

test("resizing does not discard an unsaved session project", async ({ page, request }) => {
  await workspace({ page, request });
  await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
  const name = page.getByLabel("프로젝트 이름", { exact: true });
  await name.fill("보존할 프로젝트 초안");
  await page.setViewportSize({ width: 390, height: 740 });
  await expect(page.locator(".inspector")).toBeVisible();
  await expect(name).toHaveValue("보존할 프로젝트 초안");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "상세 패널 표시" }).click();
  await expect(name).toHaveValue("보존할 프로젝트 초안");
});

test("a recovered refresh clears its transient error but preserves action failures", async ({ page, request }) => {
  await page.clock.install();
  const { state } = await workspace({ page, request });
  let failRefresh = true;
  await page.route("**/api/state", (route) => route.fulfill(failRefresh
    ? { status: 503, json: { error: "일시적인 조회 오류" } }
    : { json: state }));
  await page.clock.runFor(3200);
  await expect(page.getByRole("alert")).toContainText("일시적인 조회 오류");
  failRefresh = false;
  await page.clock.runFor(3200);
  await expect(page.getByRole("alert")).toHaveCount(0);

  await page.route("**/api/sessions/*/workflow", (route) => route.fulfill({
    status: 409, json: { error: "작업 방식 저장 실패" },
  }));
  await page.getByLabel("작업 방식").selectOption("source_document");
  await expect(page.getByRole("alert")).toContainText("작업 방식 저장 실패");
  await page.clock.runFor(3200);
  await expect(page.getByRole("alert")).toContainText("작업 방식 저장 실패");
});

test("a pending run keeps all run controls locked after navigation", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request }, { has_task: true });
  await addSession(page, state, session);
  const pending = gate();
  await page.route("**/api/sessions/*/run", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { started: true } });
  });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("실행할 요청");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await pending.requested;
  try {
    await page.getByRole("button", { name: "모든 설정" }).click();
    await page.getByRole("button", { name: "채팅으로 돌아가기" }).click();
    await input.fill("다음 초안");
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
    await expect(page.getByRole("button", { name: "재개", exact: true })).toBeDisabled();
    await expect(page.getByRole("button", { name: "기억 정리", exact: true })).toBeDisabled();
    await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
    await input.fill("다른 세션의 초안");
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
  } finally {
    pending.release();
  }
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
});

test("resume and cleanup cannot start overlapping requests before acknowledgement", async ({ page, request }) => {
  await workspace({ page, request }, { has_task: true });
  const pending = gate();
  let requests = 0;
  await page.route("**/api/sessions/*/run", async (route) => {
    requests++;
    pending.seen();
    await pending.held;
    await route.fulfill({ json: { started: true } });
  });
  await page.getByRole("button", { name: "재개", exact: true }).click();
  await pending.requested;
  try {
    await expect(page.getByRole("button", { name: "재개", exact: true })).toBeDisabled();
    await expect(page.getByRole("button", { name: "기억 정리", exact: true })).toBeDisabled();
    expect(requests).toBe(1);
  } finally {
    pending.release();
  }
});

test("replacing an acknowledgement refresh does not release the send lock early", async ({ page, request }) => {
  const memories = [{ id: "M1", title: "선택할 기억", summary: "요약", status: "active", revision: 1, tags: [] }];
  const { state, session } = await workspace({ page, request }, { memories });
  const first = gate(), second = gate();
  let reads = 0;
  await page.route("**/api/sessions/*/memories/M1", (route) => route.fulfill({ json: { ...memories[0], body: "기억 본문", sources: [] } }));
  await page.route("**/api/sessions/*/run", async (route) => {
    await page.route("**/api/state", async (refresh) => {
      const pending = ++reads === 1 ? first : second;
      pending.seen();
      await pending.held;
      await refresh.fulfill({ json: state });
    });
    await route.fulfill({ json: { started: true } });
  });
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("첫 요청");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await first.requested;
  await input.fill("후속 초안");
  await page.getByRole("button", { name: /선택할 기억/ }).click();
  await second.requested;
  try {
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
  } finally {
    first.release();
    second.release();
  }
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
  await expect(input).toHaveValue("후속 초안");
});

test("failed workflow changes restore the latest server choice", async ({ page, request }) => {
  const { session } = await workspace({ page, request });
  const pending = gate();
  await page.route("**/api/sessions/*/workflow", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ status: 409, json: { error: "작업 방식 저장 실패" } });
  });
  await page.getByLabel("작업 방식").selectOption("source_document");
  await pending.requested;
  session.workflow_mode = "document_edit";
  session.revision++;
  await waitForSnapshot(page, session);
  pending.release();
  await expect(page.getByLabel("작업 방식")).toBeEnabled();
  await expect(page.getByLabel("작업 방식")).toHaveValue("document_edit");
});

test("failed tool changes restore the latest server selection", async ({ page, request }) => {
  const { session } = await workspace({ page, request }, { active_tools: [] });
  const pending = gate();
  await page.route("**/api/sessions/*/tools", async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ status: 409, json: { error: "도구 저장 실패" } });
  });
  await page.getByRole("tab", { name: "도구", exact: true }).click();
  await page.getByRole("checkbox", { name: "파일 읽기", exact: true }).check();
  await pending.requested;
  session.active_tools = ["file_list"];
  session.revision++;
  await waitForSnapshot(page, session);
  pending.release();
  await expect(page.getByRole("checkbox", { name: "파일 읽기", exact: true })).toBeEnabled();
  await expect(page.getByRole("checkbox", { name: "파일 목록", exact: true })).toBeChecked();
  await expect(page.getByRole("checkbox", { name: "파일 읽기", exact: true })).not.toBeChecked();
});
