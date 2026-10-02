import { test, expect } from "./fixtures.js";

const headers = { "x-mnemoarc-client": "web" };
async function saveProjects(request, config, projects) {
  const response = await request.put("/api/settings", {
    headers,
    data: { config: { ...config, projects } },
  });
  expect(response.ok(), await response.text()).toBe(true);
}

test("same-folder projects show each session once through rename, reorder and deletion", async ({ page, request }) => {
  const initial = await (await request.get("/api/state")).json();
  const first = { ...initial.config.projects[0], id: "shared-source-first", name: "같은 소스 프로젝트 A" };
  const second = { ...first, id: "shared-source-second", name: "같은 소스 프로젝트 B" };
  const created = [];
  try {
    await saveProjects(request, initial.config, [first, second]);
    for (const project of [first, second]) {
      const response = await request.post("/api/sessions", { headers, data: { project } });
      expect(response.ok()).toBe(true);
      created.push((await response.json()).id);
    }
    await page.goto(`/#${created[0]}`);
    const group = (name) => page.locator(".project-group").filter({
      has: page.locator(".project-heading").filter({ hasText: name }),
    });
    await expect(group(first.name).locator(".session-row")).toHaveCount(1);
    await expect(group(second.name).locator(".session-row")).toHaveCount(1);
    await expect(page.locator(".session-row")).toHaveCount(initial.sessions.length + 2);

    await group(first.name).locator(".project-heading").click();
    await page.getByRole("button", { name: "세션 시작", exact: true }).click();
    await expect(page.getByRole("dialog", { name: "새 세션 설정" })).toHaveCount(0);
    await expect(group(first.name).locator(".session-row")).toHaveCount(2);
    const extra = (await page.evaluate(() => location.hash)).slice(1);
    created.push(extra);
    const detail = await (await request.get(`/api/sessions/${extra}`)).json();
    expect(detail.project.id).toBe(first.id);
    page.once("dialog", (dialog) => dialog.accept());
    await page.locator(".session-actions").getByRole("button", { name: "세션 닫기", exact: true }).click();
    await expect(group(first.name).locator(".session-row")).toHaveCount(1);
    await expect(group(second.name).locator(".session-row")).toHaveCount(1);

    const renamed = { ...first, name: "이름을 바꾼 프로젝트 A", output: "renamed-summary.md" };
    await saveProjects(request, initial.config, [second, renamed]);
    await expect(group(renamed.name).locator(".session-row")).toHaveCount(1);
    await expect(page.locator(".project-heading").first()).toContainText(second.name);
    await page.reload();
    await expect(group(renamed.name).locator(".session-row")).toHaveCount(1);
    await expect(page.locator(".session-row")).toHaveCount(initial.sessions.length + 2);

    await saveProjects(request, initial.config, [second]);
    await expect(page.locator(".project-group")).toHaveCount(1);
    await expect(group(second.name).locator(".session-row")).toHaveCount(1);
    await expect(page.locator(".session-list > .session-row")).toHaveCount(initial.sessions.length + 1);
    await expect(page.locator(".session-row")).toHaveCount(initial.sessions.length + 2);
  } finally {
    for (const id of created) await request.delete(`/api/sessions/${id}`, { headers });
    await saveProjects(request, initial.config, initial.config.projects);
  }
});

test("an unsaved same-folder project keeps its identity when added to saved projects", async ({ page, request }) => {
  const initial = await (await request.get("/api/state")).json();
  let created;
  try {
    await page.goto("/");
    await page.locator(".sidebar-bottom").getByRole("button", { name: "프로젝트 관리", exact: true }).click();
    await page.getByRole("button", { name: "＋ 프로젝트 추가", exact: true }).click();
    await page.getByLabel("프로젝트 이름", { exact: true }).fill("같은 소스의 새 프로젝트 초안");
    await page.getByRole("textbox", { name: /^소스 폴더/ }).fill(initial.config.projects[0].root);
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: "이 프로젝트로 새 세션", exact: true }).click();
    await page.getByRole("button", { name: "세션 시작", exact: true }).click();
    await expect(page.locator(".session-heading h2")).toHaveText("같은 소스의 새 프로젝트 초안");
    created = (await page.evaluate(() => location.hash)).slice(1);
    const detail = await (await request.get(`/api/sessions/${created}`)).json();
    expect(detail.project.id).toMatch(/^[0-9a-f-]{36}$/);
    expect(initial.config.projects.some((p) => p.id === detail.project.id)).toBe(false);
    await expect(page.locator(".session-list > .session-row")).toHaveCount(1);
    await expect(page.locator(".session-row")).toHaveCount(initial.sessions.length + 1);

    await saveProjects(request, initial.config, [...initial.config.projects, detail.project]);
    const group = page.locator(".project-group").filter({
      has: page.locator(".project-heading").filter({ hasText: detail.project.name }),
    });
    await expect(group.locator(".session-row")).toHaveCount(1);
    await expect(page.locator(".session-list > .session-row")).toHaveCount(0);
    await expect(page.locator(".session-row")).toHaveCount(initial.sessions.length + 1);
  } finally {
    if (created) await request.delete(`/api/sessions/${created}`, { headers });
    await saveProjects(request, initial.config, initial.config.projects);
  }
});
