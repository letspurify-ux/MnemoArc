import { test, expect, workspace, gate, addSession } from "./review-fixtures.js";

async function eventStream(page) {
  await page.addInitScript(() => {
    class Stream {
      constructor() {
        this.listeners = new Map();
        queueMicrotask(() => this.onopen?.());
        window.emitChange = (change) => this.listeners.get("changed")?.({ data: JSON.stringify(change) });
        window.failStream = () => this.onerror?.();
      }
      addEventListener(name, listener) { this.listeners.set(name, listener); }
      close() {}
    }
    window.EventSource = Stream;
  });
}

test("returning to a cached session displays history immediately and waits for fresh state before sending", async ({ page, request }) => {
  const firstBundle = { id: 1, messages: [{ role: "assistant", content: "캐시에서 바로 보이는 대화" }] };
  const { state, session } = await workspace({ page, request }, { bundles: [firstBundle] });
  await expect(page.getByText("캐시에서 바로 보이는 대화", { exact: true })).toBeVisible();
  const other = await addSession(page, state, session);
  other.bundles = [];
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(page.getByText("캐시에서 바로 보이는 대화", { exact: true })).toHaveCount(0);
  const pending = gate();
  session.revision++;
  session.bundles = [{ ...firstBundle, messages: [{ role: "assistant", content: "최신 응답으로 갱신된 대화" }] }];
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: session });
  });
  await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
  await pending.requested;
  try {
    await expect(page.getByText("캐시에서 바로 보이는 대화", { exact: true })).toBeVisible();
    await expect(page.getByText("세션을 불러오는 중…", { exact: true })).toHaveCount(0);
    await expect(page.getByRole("status")).toContainText("최신 내용을 확인하는 중");
    await page.getByRole("textbox", { name: "메시지", exact: true }).fill("후속 질문");
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
  } finally { pending.release(); }
  await expect(page.getByText("최신 응답으로 갱신된 대화", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
});

test("events from other sessions avoid selected detail reads and connected polling stays quiet", async ({ page, request }) => {
  await eventStream(page);
  await page.clock.install();
  const { state, session } = await workspace({ page, request });
  await page.clock.runFor(500);
  const other = { ...structuredClone(session), id: "background-session" };
  state.sessions.push({ ...state.sessions[0], id: other.id, title: "백그라운드 세션", revision: 10 });
  let detailReads = 0, stateReads = 0;
  page.on("request", (request) => {
    if (request.url().endsWith(`/api/sessions/${session.id}`)) detailReads++;
    if (request.url().endsWith("/api/state")) stateReads++;
  });
  await page.evaluate((session) => window.emitChange({ revision: 10, session, state: true }), other.id);
  await page.clock.runFor(250);
  await expect(page.locator(".session-button").filter({ hasText: "백그라운드 세션" })).toBeVisible();
  expect(detailReads).toBe(0);
  const sidebarReads = stateReads;
  for (let revision = 11; revision <= 30; revision++) {
    await page.evaluate(({ session, revision }) => window.emitChange({ revision, session, state: false }), { session: other.id, revision });
  }
  await page.clock.runFor(3500);
  expect(detailReads).toBe(0);
  expect(stateReads).toBe(sidebarReads);
  session.revision += 100;
  session.bundles = [{ id: 1, messages: [{ role: "assistant", content: "선택한 세션의 변화" }] }];
  await page.evaluate(({ id, revision }) => window.emitChange({ revision, session: id, state: false }), session);
  await page.clock.runFor(250);
  await expect(page.getByText("선택한 세션의 변화", { exact: true })).toBeVisible();
  expect(detailReads).toBe(1);
  expect(stateReads).toBe(sidebarReads);
  // A disconnected stream still reconciles all state on the ordinary poll.
  session.project.name = "연결 복구 중에도 갱신된 프로젝트";
  await page.evaluate(() => window.failStream());
  await page.clock.runFor(3200);
  await expect(page.locator(".session-heading h2")).toHaveText(session.project.name);
  expect(stateReads).toBeGreaterThan(sidebarReads);
});

test("connected sessions still detect external changes during periodic reconciliation", async ({ page, request }) => {
  await eventStream(page);
  await page.clock.install();
  const { session } = await workspace({ page, request });
  await page.clock.runFor(500);
  session.revision++;
  session.memories = [{ id: "M1", title: "외부 변경을 확인한 기억", summary: "검토 필요", status: "needs_review", revision: 1, tags: [] }];
  await page.clock.runFor(16000);
  await expect(page.getByRole("button", { name: /외부 변경을 확인한 기억/ })).toBeVisible();
});
