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

async function addEventSession(page, state, session) {
  const other = { ...structuredClone(session), id: "other-performance-session", revision: session.revision + 1 };
  state.sessions.push({ ...state.sessions[0], id: other.id, title: "다른 세션", revision: other.revision });
  await page.route(`**/api/sessions/${other.id}`, (route) => route.fulfill({ json: other }));
  await page.evaluate(({ id, revision }) => window.emitChange({ revision, session: id, state: true }), other);
  await expect(page.locator(".session-button").filter({ hasText: "다른 세션" })).toBeVisible();
  return other;
}

// The session stays read-only until fresh state arrives; no notice is shown.
const verifying = (page) => page.locator('.workarea[aria-busy="true"]');

test("a backend restart resets revision watermarks and ignores events from the previous instance", async ({ page, request }) => {
  await eventStream(page);
  const { state, session } = await workspace({ page, request });
  const previousInstance = state.server_instance || "before-restart";
  state.server_instance = previousInstance;
  state.revision = session.revision = 100;
  session.server_instance = previousInstance;
  state.sessions[0].revision = 100;
  const oldSnapshot = page.waitForResponse((response) => response.url().endsWith("/api/state"));
  await page.evaluate((server_instance) => window.emitChange({
    server_instance, revision: 100, session: null, state: true,
  }), previousInstance);
  await oldSnapshot;
  await expect(verifying(page)).toHaveCount(0);

  const fresh = {
    ...structuredClone(session), id: "after-restart-session", revision: 1,
    server_instance: "after-restart", bundles: [],
  };
  state.server_instance = fresh.server_instance;
  state.revision = 1;
  state.sessions = [{ ...state.sessions[0], id: fresh.id, revision: 1, title: "재시작 후 세션" }];
  await page.route(`**/api/sessions/${fresh.id}`, (route) => route.fulfill({ json: fresh }));
  await page.evaluate((server_instance) => window.emitChange({
    server_instance, revision: 0, session: null, state: true,
  }), fresh.server_instance);
  await expect(page.locator(".session-button").filter({ hasText: "재시작 후 세션" })).toBeVisible();
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("재시작 후 요청");
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
  await expect(verifying(page)).toHaveCount(0);

  await page.evaluate((server_instance) => window.emitChange({
    server_instance, revision: 200, session: null, state: true,
  }), previousInstance);
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
  await expect(verifying(page)).toHaveCount(0);
});

test("a detail response from the previous backend cannot replace a fresh session with the same ID", async ({ page, request }) => {
  await eventStream(page);
  const { state, session } = await workspace({ page, request }, {
    bundles: [{ id: 1, messages: [{ role: "assistant", content: "재시작 전 대화" }] }],
  });
  await expect(page.getByText("재시작 전 대화", { exact: true })).toBeVisible();
  const old = { ...structuredClone(session), revision: 100 };
  const first = gate();
  let reads = 0;
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    if (reads++ === 0) {
      first.seen();
      await first.held;
      await route.fulfill({ json: old });
    } else await route.fulfill({ json: session });
  });
  state.server_instance = session.server_instance = "restored-backend";
  state.revision = session.revision = state.sessions[0].revision = 1;
  session.bundles = [{ id: 1, messages: [{ role: "assistant", content: "재시작 후 대화" }] }];
  await page.evaluate((server_instance) => window.emitChange({
    server_instance, revision: 0, session: null, state: true,
  }), session.server_instance);
  await first.requested;
  try {
    await expect(page.getByText("재시작 전 대화", { exact: true })).toHaveCount(0);
  } finally { first.release(); }
  await expect(page.getByText("재시작 후 대화", { exact: true })).toBeVisible();
  await expect(page.getByText("재시작 전 대화", { exact: true })).toHaveCount(0);
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("보존된 세션에서 계속");
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
});

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
    await expect(verifying(page)).toBeVisible();
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

test("cached tools stay read-only until verification and later edits preserve the server selection", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request }, { active_tools: [] });
  await addSession(page, state, session);
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(verifying(page)).toHaveCount(0);
  const remote = await request.put(`/api/sessions/${session.id}/tools`, {
    headers: { "X-MnemoArc-Client": "web" }, data: { names: ["code_outline"] },
  });
  expect(remote.ok()).toBe(true);
  session.revision++;
  session.active_tools = ["code_outline"];
  const pending = gate();
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: session });
  });
  const written = gate();
  let submitted;
  await page.route(`**/api/sessions/${session.id}/tools`, async (route) => {
    submitted = route.request().postDataJSON().names;
    const response = await route.fetch();
    expect(response.ok()).toBe(true);
    session.active_tools = submitted;
    written.seen();
    await route.fulfill({ response });
  });
  await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
  await pending.requested;
  const toggle = page.getByRole("checkbox", { name: "소스 검색", exact: true });
  try {
    await expect(verifying(page)).toBeVisible();
    await page.getByRole("tab", { name: "도구", exact: true }).click();
    await expect(toggle).toBeDisabled();
    expect(submitted).toBeUndefined();
  } finally { pending.release(); }
  await expect(toggle).toBeEnabled();
  await expect(page.getByRole("checkbox", { name: "코드 구조 조회", exact: true })).toBeChecked();
  await toggle.check();
  await written.requested;
  const saved = await (await request.get(`/api/sessions/${session.id}`)).json();
  expect(saved.active_tools).toEqual(["code_outline", "source_search"]);
});

for (const notice of ["session", "global", "summary"]) {
  test(`${notice} revisions keep a late cached response read-only until the announced revision arrives`, async ({ page, request }) => {
    await eventStream(page);
    const { state, session } = await workspace({ page, request }, {
      bundles: [{ id: 1, messages: [{ role: "assistant", content: "캐시의 대화" }] }],
    });
    await addEventSession(page, state, session);
    await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
    await expect(verifying(page)).toHaveCount(0);
    const first = gate(), latest = gate();
    const old = structuredClone(session);
    old.revision += 10;
    old.bundles[0].messages[0].content = "늦게 도착한 대화";
    session.revision = old.revision + 1;
    session.bundles[0].messages[0].content = "알림까지 반영한 최신 대화";
    let reads = 0;
    await page.route(`**/api/sessions/${session.id}`, async (route) => {
      const wait = reads++ === 0 ? first : latest;
      wait.seen();
      await wait.held;
      await route.fulfill({ json: wait === first ? old : session });
    });
    const stateRead = gate();
    if (notice === "summary") {
      state.sessions.find((s) => s.id === session.id).revision = session.revision;
      await page.route("**/api/state", async (route) => {
        stateRead.seen();
        await route.fulfill({ json: state });
      });
      await page.evaluate(() => window.emitChange({ revision: 0, session: null, state: true }));
    }
    await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
    await first.requested;
    try {
      await page.getByRole("textbox", { name: "메시지", exact: true }).fill("후속 요청");
      if (notice === "summary") await stateRead.requested;
      else await page.evaluate(({ id, revision, notice }) => window.emitChange({
        revision, session: notice === "global" ? null : id, state: notice === "global",
      }), { ...session, notice });
      first.release();
      await latest.requested;
      await expect(verifying(page)).toBeVisible();
      await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeDisabled();
      await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
      await expect(page.getByRole("button", { name: "현재 세션에 적용", exact: true })).toBeDisabled();
    } finally { first.release(); latest.release(); }
    await expect(page.getByText("알림까지 반영한 최신 대화", { exact: true })).toBeVisible();
    await expect(verifying(page)).toHaveCount(0);
    await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
    await expect(page.getByRole("button", { name: "현재 세션에 적용", exact: true })).toBeEnabled();
    await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toHaveValue("후속 요청");
  });
}

test("another session's newer events do not block a cached selection from becoming ready", async ({ page, request }) => {
  await eventStream(page);
  const { state, session } = await workspace({ page, request });
  const other = await addEventSession(page, state, session);
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(verifying(page)).toHaveCount(0);
  const pending = gate();
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: session });
  });
  await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
  await pending.requested;
  try {
    await page.getByRole("textbox", { name: "메시지", exact: true }).fill("이 세션의 요청");
    await page.evaluate(({ id, revision }) => window.emitChange({
      revision: revision + 100, session: id, state: false,
    }), other);
  } finally { pending.release(); }
  await expect(page.getByRole("button", { name: "메시지 보내기" })).toBeEnabled();
  await expect(verifying(page)).toHaveCount(0);
});

test("session settings wait for cached verification while global-only saves remain available", async ({ page, request }) => {
  const { state, session } = await workspace({ page, request });
  await addSession(page, state, session);
  await page.locator(".session-button").filter({ hasText: "다른 세션" }).click();
  await expect(verifying(page)).toHaveCount(0);
  const pending = gate();
  session.revision++;
  session.config.model_context = 256000;
  await page.route(`**/api/sessions/${session.id}`, async (route) => {
    pending.seen();
    await pending.held;
    await route.fulfill({ json: session });
  });
  await page.locator(".session-button").filter({ hasText: "리뷰 세션" }).click();
  await pending.requested;
  try {
    await page.getByRole("button", { name: "모든 설정" }).click();
    const save = page.getByRole("button", { name: "설정 저장", exact: true });
    await expect(save).toBeDisabled();
    await page.getByLabel("현재 세션에도 적용").uncheck();
    await expect(save).toBeEnabled();
    await page.getByLabel("설정 적용 범위").selectOption("session");
    await expect(save).toBeDisabled();
    await page.getByLabel("모델 이름", { exact: true }).fill("보존할 설정 초안");
  } finally { pending.release(); }
  await expect(page.getByRole("button", { name: "설정 저장", exact: true })).toBeEnabled();
  await expect(page.getByLabel("모델 최대 컨텍스트", { exact: true })).toHaveValue("256000");
  await expect(page.getByLabel("모델 이름", { exact: true })).toHaveValue("보존할 설정 초안");
});
