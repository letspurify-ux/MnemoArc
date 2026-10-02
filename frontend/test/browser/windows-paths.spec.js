import { test, expect, workspace } from "./review-fixtures.js";

for (const [kind, root, readable] of [
  ["drive", String.raw`\\?\C:\프로젝트`, String.raw`C:\프로젝트`],
  [
    "UNC",
    String.raw`\\?\UNC\server\share\프로젝트`,
    String.raw`\\server\share\프로젝트`,
  ],
]) {
  test(`${kind} paths stay readable throughout the UI and intact in file requests`, async ({
    page,
    request,
  }) => {
    const { state, session } = await workspace({ page, request });
    const output = `${root}\\결과.md`;
    const project = { ...state.config.projects[0], root, output };
    session.project = { ...project };
    state.config.projects = [project];
    state.sessions[0].project = project;
    await page.reload();

    await expect(page.locator(".session-heading p")).toHaveText(readable);
    await expect(page.locator(".session-heading p")).toHaveAttribute(
      "title",
      readable,
    );
    await expect(page.locator(".project-heading")).toHaveAttribute(
      "title",
      readable,
    );
    await page.getByRole("tab", { name: "문서", exact: true }).click();
    await expect(page.locator(".inspector .directory-path")).toHaveText(
      `${readable}\\결과.md`,
    );

    await page.getByRole("button", { name: "새 세션", exact: true }).click();
    const creation = page.getByRole("dialog", { name: "새 세션 설정" });
    await expect(creation.getByLabel("결과 문서", { exact: true })).toHaveValue(
      `${readable}\\결과.md`,
    );
    let created;
    await page.route("**/api/sessions", (route) => {
      created = route.request().postDataJSON().project;
      return route.fulfill({ status: 400, json: { error: "경로 전달 확인" } });
    });
    await creation
      .getByRole("button", { name: "세션 시작", exact: true })
      .click();
    await expect(creation.getByRole("alert")).toHaveText("경로 전달 확인");
    expect(created.root).toBe(root);
    expect(created.output).toBe(output);
    await creation.getByRole("button", { name: "취소", exact: true }).click();

    await page.getByRole("tab", { name: "프로젝트", exact: true }).click();
    const sourceInput = page.getByRole("textbox", { name: /^소스 폴더/ });
    await expect(sourceInput).toHaveValue(readable);
    await expect(page.getByLabel(/^결과 문서 경로/)).toHaveValue(
      `${readable}\\결과.md`,
    );
    const child = `${root}\\작업폴더`;
    const visited = [];
    await page.route("**/api/directories?*", (route) => {
      const path = new URL(route.request().url()).searchParams.get("path");
      visited.push(path);
      return route.fulfill({
        json:
          path === root
            ? {
                path: root,
                parent: null,
                directories: [{ name: "작업폴더", path: child }],
              }
            : { path: child, parent: root, directories: [] },
      });
    });
    await page.getByRole("button", { name: "폴더 선택", exact: true }).click();
    const picker = page.getByRole("dialog", { name: "프로젝트 폴더 선택" });
    await expect(picker.locator(".directory-path")).toHaveText(readable);
    await picker.getByRole("button", { name: /작업폴더/ }).click();
    await expect(picker.locator(".directory-path")).toHaveText(
      `${readable}\\작업폴더`,
    );
    await picker
      .getByRole("button", { name: "↑ 상위 폴더", exact: true })
      .click();
    await expect(picker.locator(".directory-path")).toHaveText(readable);
    await picker.getByRole("button", { name: /작업폴더/ }).click();
    await expect(picker.locator(".directory-path")).toHaveText(
      `${readable}\\작업폴더`,
    );
    await picker
      .getByRole("button", { name: "이 폴더 선택", exact: true })
      .click();
    await expect(sourceInput).toHaveValue(`${readable}\\작업폴더`);
    expect(visited).toEqual([root, child, root, child]);

    let saved;
    await page.route("**/api/sessions/*/project", (route) => {
      saved = route.request().postDataJSON();
      session.project = saved;
      session.revision++;
      return route.fulfill({ json: { saved: true } });
    });
    await page
      .getByRole("button", { name: "현재 세션에 적용", exact: true })
      .click();
    await expect.poll(() => saved?.root).toBe(child);
    expect(saved.output).toBe(output);
  });

  test(`${kind} diagnostics, tool paths and memory search use readable paths while originals remain exact`, async ({
    page,
    request,
  }) => {
    const path = `${root}\\결과.md`;
    const shown = `${readable}\\결과.md`;
    const sourceText = `const path = ${JSON.stringify(path)};`;
    const rawResult = {
      status: "ok",
      data: {
        path,
        source: { path, excerpt: sourceText },
        content: sourceText,
      },
    };
    const error = `file_access_error: ${path}`;
    const now = new Date().toISOString();
    const memory = {
      id: "M-path",
      title: `경로 관련 기억: ${path}`,
      summary: `경로: ${path}`,
      tags: [],
      status: "active",
      revision: 1,
    };
    const { session } = await workspace(
      { page, request },
      {
        status: "blocked",
        error,
        workflow_mode: "source_document",
        bundles: [
          {
            id: 1,
            messages: [
              { role: "user", content: `원래 입력: ${path}` },
              {
                role: "tool",
                name: "file_read",
                tool_call_id: "path-tool",
                content: JSON.stringify(rawResult),
              },
            ],
          },
        ],
        memories: [memory],
        run_history: [
          {
            id: "path-run",
            status: "blocked",
            reason: "file_access_error",
            error,
            workflow: "follow_up",
            started_at: now,
            ended_at: now,
            elapsed_ms: 0,
            last_stage: "tools",
            rounds: 1,
            input_tokens: 0,
            output_tokens: 0,
            token_limit: 1000,
            timeout_secs: 30,
          },
        ],
        completion_review: {
          required: true,
          checks: [
            {
              id: "C1",
              status: "unverified",
              criterion: "결과 문서",
              reason: `검증 필요: ${path}`,
              next_action: `다시 확인: ${path}`,
            },
          ],
        },
      },
    );

    await expect(page.locator(".row.user")).toHaveText(`원래 입력: ${path}`);
    await expect(page.locator(".conversation [role=alert]")).toHaveText(
      `file_access_error: ${shown}`,
    );
    await expect(page.locator(".conversation [role=status] p")).toHaveText(
      `file_access_error: ${shown}`,
    );
    const tool = page.locator(".tool-result");
    await tool.locator(":scope > summary").click();
    const displayed = JSON.parse(
      await tool.locator(":scope > pre").textContent(),
    );
    expect(displayed.data.path).toBe(shown);
    expect(displayed.data.source.path).toBe(shown);
    expect(displayed.data.source.excerpt).toBe(sourceText);
    expect(displayed.data.content).toBe(sourceText);
    await expect(tool.locator(".tool-result-raw pre")).toHaveCount(0);
    await tool.getByText("원문 JSON", { exact: true }).click();
    await expect(tool.locator(".tool-result-raw pre")).toBeVisible();
    expect(
      JSON.parse(await tool.locator(".tool-result-raw pre").textContent()),
    ).toEqual(rawResult);

    await expect(page.locator(".memory-card p")).toHaveText(`경로: ${shown}`);
    const search = page.getByRole("textbox", {
      name: "기억 검색",
      exact: true,
    });
    for (const query of [shown, path]) {
      await search.fill(query);
      await expect(page.locator(".memory-card")).toHaveCount(1);
    }
    await page.route(`**/api/sessions/${session.id}/memories/M-path`, (route) =>
      route.fulfill({
        json: {
          ...memory,
          body: "```js\n" + sourceText + "\n```",
          sources: [{ id: "S1", path, start_line: 1, excerpt: sourceText }],
        },
      }),
    );
    await page.locator(".memory-card").click();
    await expect(page.locator(".memory-detail pre code")).toHaveText(
      sourceText + "\n",
    );
    await expect(
      page.locator(".memory-detail .source-card code"),
    ).toContainText(shown);
    await expect(page.locator(".memory-detail .source-card p")).toHaveText(
      sourceText,
    );

    await page.getByRole("tab", { name: "진행", exact: true }).click();
    await page.locator(".run-record > summary").click();
    await expect(page.locator(".run-error")).toHaveText(
      `file_access_error: ${shown}`,
    );
    const review = page.getByRole("region", { name: "완료 조건 검증" });
    await expect(review).toContainText(`검증 필요: ${shown}`);
    await expect(review).toContainText(`다시 확인: ${shown}`);
    await expect(review).not.toContainText(root);

    await page.route("**/api/sessions/*/output", (route) =>
      route.fulfill({
        status: 400,
        json: { error: `파일을 열 수 없습니다: ${path}` },
      }),
    );
    await page.getByRole("tab", { name: "문서", exact: true }).click();
    await page
      .getByRole("button", { name: "문서 불러오기", exact: true })
      .click();
    await expect(page.locator(".app-error")).toContainText(
      `파일을 열 수 없습니다: ${shown}`,
    );
    expect(JSON.parse(session.bundles[0].messages[1].content)).toEqual(
      rawResult,
    );
    expect(session.error).toBe(error);
    expect(session.memories[0].summary).toBe(`경로: ${path}`);
  });
}
