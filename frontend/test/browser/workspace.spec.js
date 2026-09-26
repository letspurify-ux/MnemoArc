import { test, expect } from "@playwright/test";

test("configure entirely in UI, stream rich chat, switch/cancel sessions and retain RAM on reload", async ({
  page,
  request,
}) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto("/");
  await expect(
    page.getByRole("heading", { name: "맥락을 기억하고, 작업을 이어갑니다." }),
  ).toBeVisible();
  await page.screenshot({
    path: "test-artifacts/workspace.png",
    fullPage: true,
  });
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page
    .getByLabel("모델 이름", { exact: true })
    .fill("z-ai/glm-5.3-flash");
  await page.getByLabel("모델 최대 컨텍스트", { exact: true }).fill("128000");
  await page.getByLabel("API 키", { exact: true }).fill("browser-test-only");
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("설정을 저장했습니다");
  await page.getByRole("button", { name: "연결 확인", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("연결 확인 완료");
  await page.getByRole("button", { name: "기억과 보관" }).click();
  await page.getByLabel("전체 기억 보관량", { exact: true }).fill("20");
  await page.getByRole("button", { name: "실행과 예산" }).click();
  await page.getByLabel("동시 읽기 개수", { exact: true }).fill("2");
  await page.getByLabel("작성·검증 예산 비율", { exact: true }).fill("60");
  await page.getByLabel("검증 예산 비율", { exact: true }).fill("30");
  await page.getByLabel("동일 범위 반복 조회 제한", { exact: true }).fill("3");
  await page.getByLabel("진행 정체 감지 횟수", { exact: true }).fill("10");
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("설정을 저장했습니다");
  await page.screenshot({
    path: "test-artifacts/settings.png",
    fullPage: true,
  });
  const state = await (await request.get("/api/state")).json();
  expect(state.config.memory_bytes).toBe(20 * 1024 * 1024);
  expect(state.config.read_parallelism).toBe(2);
  expect(state.config.writing_reserve_ratio).toBe(0.6);
  expect(state.config.verification_reserve_ratio).toBe(0.3);
  expect(state.config.repeated_read_limit).toBe(3);
  expect(state.config.stall_round_limit).toBe(10);
  expect(JSON.stringify(state)).not.toContain("browser-test-only");
  await page.getByRole("button", { name: "채팅으로 돌아가기" }).click();
  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("프로젝트를 조사해줘");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.getByRole("button", { name: "■ 중지" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "조사 결과" })).toBeVisible();
  await expect(page.locator(".mermaid svg")).toBeVisible();
  await expect(page.locator(".chat-content table")).toBeVisible();
  await expect(page.locator(".chat-content .katex")).toHaveCount(1);
  await page.reload();
  await expect(page.getByRole("heading", { name: "조사 결과" })).toBeVisible();
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("느린 요청 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.locator(".thinking")).toContainText("모델 응답 대기 중");
  await expect(page.locator(".thinking")).toContainText("1번째 모델 호출");
  await expect(page.locator(".thinking")).toContainText(/1초/);
  await expect(
    page.getByText("천천히 조사하고 있습니다…", { exact: true }),
  ).toBeVisible();
  await expect(page.locator(".thinking")).toContainText("답변 생성 중");
  await page.getByRole("button", { name: /프로젝트를 조사해줘/ }).click();
  await expect(
    page.getByRole("button", { name: "● 다른 세션 작업 중" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "● 다른 세션 작업 중" }).click();
  await page.getByRole("button", { name: "■ 중지" }).click();
  await expect(page.locator(".status-pill")).toHaveText("중지됨");
  await expect(page.locator(".thinking")).toHaveCount(0);
  await expect(
    page.getByText("느린 요청 테스트", { exact: true }).last(),
  ).toBeVisible();
  await page.getByRole("tab", { name: "기억", exact: true }).click();
  await expect(page.getByText(/컨텍스트 예산은 기준 토크나이저/)).toBeVisible();
  await page.getByRole("tab", { name: "도구", exact: true }).click();
  await expect(
    page.getByText(/상대 경로는 프로젝트 루트 기준입니다/),
  ).toBeVisible();
  await expect(
    page.getByText(/경로 입력 없이 설정된 결과 문서를 조회합니다/),
  ).toBeVisible();
  await page.getByRole("checkbox", { name: "파일 읽기", exact: true }).check();
  await expect(
    page.getByRole("checkbox", { name: "파일 읽기", exact: true }),
  ).toBeChecked();
  await expect(
    page.getByRole("checkbox", { name: "문서 구조 조회", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: "문서 근거 점검", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: "심볼 검색", exact: true }),
  ).toBeVisible();
  expect(errors).toEqual([]);
});

test("ordered to-do list follows prerequisites and preserves running work on reload", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("할 일 목록 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  const plan = page.getByRole("region", { name: "할 일 목록" });
  await expect(plan.locator("li")).toHaveCount(4);
  await expect(plan.locator("li").nth(0)).toContainText("완료");
  await expect(plan.locator("li").nth(0)).toContainText("선행 근거 확인");
  await expect(plan.locator("[aria-current=step]")).toContainText("본문 초안 작성");
  await expect(plan.locator("li").nth(2)).toContainText("대기");
  await expect(plan.locator("li").nth(2)).toContainText("본문 내용 검증");
  await expect(plan.locator("li").nth(3)).toContainText("결과 검증");
  await expect(plan).not.toContainText("불필요 작업");
  await expect(plan).toContainText("남은 항목 3/100 · 누적 완료 1개");
  await expect(page.getByRole("button", { name: "■ 중지" })).toBeVisible();
  await page.reload();
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  await expect(plan.locator("[aria-current=step]")).toContainText("본문 초안 작성");
  await expect(
    page.getByRole("button", { name: "● 다른 세션 작업 중" }),
  ).toHaveCount(0);
  await page.screenshot({
    path: "test-artifacts/task-plan.png",
    fullPage: true,
  });
  await page.getByRole("button", { name: "■ 중지" }).click();
  await expect(page.locator(".status-pill")).toHaveText("중지됨");
});

test("completed to-dos return to missing acceptance criteria before final publication", async ({ page, request }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await page.getByRole("textbox", { name: "메시지", exact: true }).fill("완료 조건 검증 테스트");
  await page.getByRole("tab", { name: "진행", exact: true }).click();
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  const acceptance = page.getByRole("region", { name: "완료 조건 검증", exact: true });
  await expect(acceptance).toContainText("미충족");
  await expect(page.locator(".task-plan")).toContainText("누락된 예시를 답변에 추가합니다.");
  await expect(page.locator(".status-pill")).toHaveText("완료");
  await expect(acceptance).toContainText("모든 완료 조건의 검증을 통과했습니다.");
  await expect(page.locator(".chat-content")).toContainText("요약과 예시를 모두 작성했습니다.");
  await expect(page.locator(".chat-content")).not.toContainText("요약을 작성했습니다.");
  const state = await (await request.get("/api/state")).json();
  const item = state.sessions.find((s) => s.title === "완료 조건 검증 테스트");
  const detail = await (await request.get(`/api/sessions/${item.id}`)).json();
  expect(detail.completion_review.approved).toBe(true);
  expect(detail.completion_review.attempts).toBe(2);
  expect(detail.task.todos_completed_total).toBe(2);
  await page.screenshot({ path: "test-artifacts/completion-review.png", fullPage: true });
});

test("project folder picker, settings validation and narrow screen", async ({
  page,
}) => {
  await page.goto("/");
  await page
    .locator(".sidebar-bottom")
    .getByRole("button", { name: "프로젝트 관리", exact: true })
    .click();
  await page.getByRole("button", { name: "폴더 선택", exact: true }).click();
  await expect(
    page.getByRole("dialog", { name: "프로젝트 폴더 선택" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "이 폴더 선택", exact: true }).click();
  await page
    .getByLabel("프로젝트 이름", { exact: true })
    .fill("새 프로젝트 이름");
  await page
    .getByRole("button", { name: "프로젝트 저장", exact: true })
    .click();
  await expect(page.getByRole("status")).toContainText(
    "프로젝트를 저장했습니다",
  );
  await page.getByRole("button", { name: "모든 설정" }).click();
  await page.getByLabel("요청 컨텍스트 예산", { exact: true }).fill("10");
  await page.getByRole("button", { name: "설정 저장", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Context must leave space",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("button", { name: "메뉴 열기" })).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-artifacts/mobile-settings.png",
    fullPage: true,
  });
});

test("length-limited Mermaid answer continues as one rendered diagram", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await page
    .getByRole("textbox", { name: "메시지", exact: true })
    .fill("길이 이어받기 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(page.locator(".mermaid svg")).toHaveCount(1);
  await expect(page.locator(".thinking")).toHaveCount(0);
  const state = await (await request.get("/api/state")).json();
  const session = state.sessions.find(
    (s) => s.title === "길이 이어받기 테스트",
  );
  const detail = await (
    await request.get(`/api/sessions/${session.id}`)
  ).json();
  expect(detail.status).toBe("complete");
  expect(detail.usage.output).toBe(40);
  expect(detail.continuation_pending).toBe(false);
  const replies = detail.bundles
    .flatMap((b) => b.messages)
    .filter((m) => m.role === "assistant");
  expect(replies).toHaveLength(2);
  expect(replies[0].partial).toBe(true);
  expect(replies[1].continues_previous).toBe(true);
});

test("app exit can be cancelled and stops reconnecting after confirmation", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByText("로컬 에이전트 연결됨")).toBeVisible();
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "앱 종료", exact: true }).click();
  await expect(page.getByText("로컬 에이전트 연결됨")).toBeVisible();
  // The Rust launcher integration test covers actual server termination.
  await page.route("**/api/shutdown", (route) =>
    route.fulfill({ json: { stopping: true } }),
  );
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "앱 종료", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "앱 종료를 요청했습니다" }),
  ).toBeVisible();
  await expect(page.getByText("연결 복구 중…")).toHaveCount(0);
});

test("session window selects the workflow for the next request", async ({
  page,
  request,
}) => {
  await page.goto("/");
  const workflow = page.getByLabel("작업 방식");
  await page.getByLabel("요청 종류").selectOption("chat");
  await expect(workflow).toHaveValue("answer");
  // No automatic choice: the user picks one of the three workflows.
  await expect(workflow.locator("option")).toHaveText([
    "질문 답변",
    "소스 기반 문서 작성",
    "문서 편집",
  ]);
  await workflow.selectOption("source_document");
  const state = await (await request.get("/api/state")).json();
  const id = state.sessions[0].id;
  await expect
    .poll(async () => {
      const session = await (await request.get(`/api/sessions/${id}`)).json();
      return session.workflow_mode;
    })
    .toBe("source_document");
  await page.reload();
  await expect(page.getByLabel("작업 방식")).toHaveValue("source_document");
  await page.locator(".composer-wrap").screenshot({
    path: "test-artifacts/workflow-select-desktop.png",
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByLabel("작업 방식")).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-artifacts/workflow-select.png",
    fullPage: true,
  });
});
