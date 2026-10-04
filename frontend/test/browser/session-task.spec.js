import { test, expect } from "./fixtures.js";
const headers = { "x-mnemoarc-client": "web" };
const dialog = (page) => page.getByRole("dialog", { name: "새 세션 설정" });

for (const mode of ["answer", "source_document"]) {
  test(`creation fixes ${mode}, preserves top controls and reopens without a picker`, async ({
    page,
    request,
  }) => {
    const state = await (await request.get("/api/state")).json();
    await page.goto("/");
    await page.getByRole("button", { name: "새 세션", exact: true }).click();
    const label = mode === "answer" ? /일반 작업/ : /소스 기반 문서 작성/;
    await dialog(page).getByRole("radio", { name: label }).check();
    const output = await dialog(page)
      .getByLabel("결과 문서", { exact: true })
      .inputValue();
    expect(output).toBe(state.config.projects[0].output);
    await dialog(page)
      .getByRole("button", { name: "세션 시작", exact: true })
      .click();
    await expect(dialog(page)).toHaveCount(0);
    const id = (await page.evaluate(() => location.hash)).slice(1);
    const created = await (await request.get(`/api/sessions/${id}`)).json();
    expect(created.project.output).toBe(output);
    await expect(page.locator(".workflow-badge")).toHaveText(
      mode === "answer" ? "일반 작업" : "소스 기반 문서 작성",
    );
    await expect(page.getByLabel("요청 종류")).toHaveCount(0);
    await expect(
      page.locator(".composer-wrap").getByLabel("작업 방식"),
    ).toHaveCount(0);
    for (const label of ["재개", "기억 정리", "세션 닫기"])
      await expect(
        page
          .locator(".session-actions")
          .getByRole("button", { name: label, exact: true }),
      ).toBeVisible();
    const changed = await request.put(`/api/sessions/${id}/workflow`, {
      headers,
      data: { workflow: mode === "answer" ? "source_document" : "answer" },
    });
    expect(changed.status()).toBe(409);
    await page.reload();
    await expect(dialog(page)).toHaveCount(0);
    await expect(page.locator(".workflow-badge")).toHaveText(
      mode === "answer" ? "일반 작업" : "소스 기반 문서 작성",
    );
    await page.getByRole("tab", { name: "도구", exact: true }).click();
    await expect(
      page.getByRole("checkbox", { name: "파일 읽기", exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole("checkbox", { name: "문서 구조 조회", exact: true }),
    ).toBeVisible();
    await expect(
      page.getByRole("checkbox", { name: "문서 근거 점검", exact: true }),
    ).toHaveCount(mode === "answer" ? 0 : 1);
    await request.delete(`/api/sessions/${id}`, { headers });
  });
}

test("new sessions default to the saved project output across entry points and project changes", async ({
  page,
  request,
}) => {
  const initial = await (await request.get("/api/state")).json();
  const first = {
    ...initial.config.projects[0],
    output: "docs/프로젝트-결과.md",
  };
  const second = {
    ...first,
    id: "other-output-project",
    name: "다른 결과 문서 프로젝트",
    output: "reports/other.md",
  };
  const created = [];
  try {
    const saved = await request.put("/api/settings", {
      headers,
      data: { config: { ...initial.config, projects: [first, second] } },
    });
    expect(saved.ok(), await saved.text()).toBe(true);
    const original = await request.post("/api/sessions", {
      headers,
      data: { project: { ...first, output: "docs/session-override.md" } },
    });
    expect(original.ok()).toBe(true);
    const id = (await original.json()).id;
    created.push(id);
    await page.goto(`/#${id}`);
    await expect(page.locator(".session-heading h2")).toHaveText(first.name);

    await page.getByRole("button", { name: "새 세션", exact: true }).click();
    const output = dialog(page).getByLabel("결과 문서", { exact: true });
    await expect(output).toHaveValue(first.output);
    await output.fill("docs/custom.md");
    await dialog(page)
      .getByRole("radio", { name: /소스 기반 문서 작성/ })
      .check();
    await expect(output).toHaveValue("docs/custom.md");
    await dialog(page)
      .getByRole("combobox", { name: "프로젝트", exact: true })
      .selectOption(second.id);
    await expect(output).toHaveValue(second.output);
    await dialog(page)
      .getByRole("combobox", { name: "프로젝트", exact: true })
      .selectOption(first.id);
    await expect(output).toHaveValue(first.output);
    await output.fill("docs/new-session-only.md");
    await dialog(page)
      .getByRole("button", { name: "세션 시작", exact: true })
      .click();
    await expect(dialog(page)).toHaveCount(0);
    const otherId = (await page.evaluate(() => location.hash)).slice(1);
    created.push(otherId);
    expect(
      (await (await request.get(`/api/sessions/${otherId}`)).json()).project
        .output,
    ).toBe("docs/new-session-only.md");
    expect(
      (await (await request.get("/api/state")).json()).config.projects[0]
        .output,
    ).toBe(first.output);

    await page
      .getByRole("button", { name: `${second.name} 새 세션`, exact: true })
      .click();
    await expect(output).toHaveValue(second.output);
    await dialog(page)
      .getByRole("button", { name: "취소", exact: true })
      .click();
    await page.getByRole("button", { name: "새 세션", exact: true }).click();
    await expect(output).toHaveValue(first.output);
    await dialog(page)
      .getByRole("button", { name: "취소", exact: true })
      .click();

    await page
      .locator(".sidebar-bottom")
      .getByRole("button", { name: "프로젝트 관리", exact: true })
      .click();
    await page
      .getByRole("button", { name: "이 프로젝트로 새 세션", exact: true })
      .click();
    await expect(output).toHaveValue(first.output);
    await dialog(page)
      .getByRole("button", { name: "취소", exact: true })
      .click();
  } finally {
    for (const id of created)
      await request.delete(`/api/sessions/${id}`, { headers });
    const restored = await request.put("/api/settings", {
      headers,
      data: {
        config: { ...initial.config, projects: initial.config.projects },
      },
    });
    expect(restored.ok(), await restored.text()).toBe(true);
  }
});

test("an empty workspace starts through the same accessible creation dialog on mobile", async ({
  page,
  request,
}) => {
  const state = await (await request.get("/api/state")).json();
  for (const session of state.sessions)
    await request.delete(`/api/sessions/${session.id}`, { headers });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(
    page.getByRole("textbox", { name: "메시지", exact: true }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "새 세션 시작", exact: true }).click();
  await expect(dialog(page)).toBeVisible();
  await expect(
    dialog(page).getByLabel("결과 문서", { exact: true }),
  ).toHaveValue(state.config.projects[0].output);
  for (let index = 0; index < 12; index++) {
    await page.keyboard.press("Tab");
    expect(
      await page.evaluate(() =>
        document.querySelector("dialog").contains(document.activeElement),
      ),
    ).toBe(true);
  }
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.keyboard.press("Escape");
  await expect(dialog(page)).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "새 세션 시작", exact: true }),
  ).toBeFocused();
  expect((await (await request.get("/api/state")).json()).sessions).toEqual([]);
});

test("general initial and follow-up document work and collection execute without reviews", async ({
  page,
  request,
}) => {
  const state = await (await request.get("/api/state")).json();
  const saved = await request.put("/api/settings", {
    headers,
    data: {
      config: {
        ...state.config,
        model: "gpt-4o",
        model_context: 128000,
        source_document_review: true,
        completion_review_enabled: true,
      },
      api_key: "browser-test-only",
    },
  });
  expect(saved.ok()).toBe(true);
  await page.goto("/");
  await page.getByRole("button", { name: "새 세션", exact: true }).click();
  await dialog(page)
    .getByRole("radio", { name: /일반 작업/ })
    .check();
  await dialog(page)
    .getByRole("button", { name: "세션 시작", exact: true })
    .click();
  await expect(dialog(page)).toHaveCount(0);
  const id = (await page.evaluate(() => location.hash)).slice(1);
  const input = page.getByRole("textbox", { name: "메시지", exact: true });
  await input.fill("일반 문서 작업 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(
    page.getByText("일반 문서 작성 완료", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "재개", exact: true }),
  ).toBeDisabled();
  await input.fill("추가 자료 수집 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(
    page.getByText("후속 질문에서도 sample.rs:1-1 자료를 수집했습니다.", {
      exact: true,
    }),
  ).toBeVisible();
  await input.fill("일반 문서 수정 테스트");
  await page.getByRole("button", { name: "메시지 보내기" }).click();
  await expect(
    page.getByText("일반 문서 수정 완료", { exact: true }),
  ).toBeVisible();
  const detail = await (await request.get(`/api/sessions/${id}`)).json();
  expect(detail.current_goal).toBe("일반 문서 작업 테스트");
  expect(detail.run_history.map((run) => run.request)).toEqual([
    "일반 문서 작업 테스트",
    "추가 자료 수집 테스트",
    "일반 문서 수정 테스트",
  ]);
  expect(detail.document_review.attempts).toBe(0);
  expect(detail.completion_review.attempts).toBe(0);
  expect(detail.completion_review.required).toBe(false);
  expect(detail.document_review.pending).toBe(false);
  const output = await (await request.get(`/api/sessions/${id}/output`)).json();
  expect(output.content).toContain("후속 요청으로 추가한 내용");
  await request.delete(`/api/sessions/${id}`, { headers });
});
