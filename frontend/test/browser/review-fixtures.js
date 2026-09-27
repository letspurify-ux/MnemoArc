import { test as base, expect } from "@playwright/test";

export { expect };
export const test = base.extend({
  page: async ({ page }, use) => {
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await use(page);
    expect(errors).toEqual([]);
  },
});

export async function workspace({ page, request }, patch = {}) {
  const state = await (await request.get("/api/state")).json();
  const original = await (await request.get(`/api/sessions/${state.sessions[0].id}`)).json();
  const session = {
    ...original, status: "idle", has_task: false, bundles: [], previous: null,
    pruned_through: null, stream: "", error: null, run_history: [], memories: [],
    workflow_mode: "answer", ...patch,
    config: { ...original.config, model: "review-fixture", model_context: 128000 },
  };
  state.config = structuredClone(session.config);
  state.running = null;
  state.sessions = [{ ...state.sessions[0], status: "idle", title: "리뷰 세션" }];
  await page.route("**/api/state", (route) => route.fulfill({ json: state }));
  await page.route(`**/api/sessions/${session.id}`, (route) => route.fulfill({ json: session }));
  await page.goto(`/#${session.id}`);
  await expect(page.getByRole("textbox", { name: "메시지", exact: true })).toBeVisible();
  return { state, session };
}

export function gate() {
  let release, seen;
  const held = new Promise((resolve) => { release = resolve; });
  const requested = new Promise((resolve) => { seen = resolve; });
  return { held, release, seen, requested };
}

export async function addSession(page, state, session) {
  const other = { ...structuredClone(session), id: "other-review-session" };
  state.sessions.push({ ...state.sessions[0], id: other.id, title: "다른 세션" });
  await page.route(`**/api/sessions/${other.id}`, (route) => route.fulfill({ json: other }));
  await expect(page.locator(".session-button").filter({ hasText: "다른 세션" })).toBeVisible();
  return other;
}

export async function waitForSnapshot(page, session) {
  await page.waitForResponse(async (response) => response.url().endsWith(`/api/sessions/${session.id}`) &&
    (await response.json()).revision === session.revision);
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}
