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
  await page.getByLabel("모델 이름", { exact: true }).fill("z-ai/glm-5.3-flash");
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
  await page.getByLabel("반복 작업 중단 횟수", { exact: true }).fill("10");
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
  await expect(
    page.getByText("천천히 조사하고 있습니다…", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: /프로젝트를 조사해줘/ }).click();
  await expect(
    page.getByRole("button", { name: "● 다른 세션 작업 중" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "● 다른 세션 작업 중" }).click();
  await page.getByRole("button", { name: "■ 중지" }).click();
  await expect(page.locator(".status-pill")).toHaveText("중지됨");
  await expect(
    page.getByText("느린 요청 테스트", { exact: true }).last(),
  ).toBeVisible();
  await page.getByRole("tab", { name: "기억", exact: true }).click();
  await expect(page.getByText(/컨텍스트 예산은 기준 토크나이저/)).toBeVisible();
  await page.getByRole("tab", { name: "도구", exact: true }).click();
  await page.getByRole("checkbox", { name: "파일 읽기", exact: true }).check();
  await expect(
    page.getByRole("checkbox", { name: "파일 읽기", exact: true }),
  ).toBeChecked();
  await expect(page.getByRole("checkbox", { name: "문서 구조 조회", exact: true })).toBeVisible();
  await expect(page.getByRole("checkbox", { name: "문서 근거 점검", exact: true })).toBeVisible();
  await expect(page.getByRole("checkbox", { name: "심볼 검색", exact: true })).toBeVisible();
  expect(errors).toEqual([]);
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
