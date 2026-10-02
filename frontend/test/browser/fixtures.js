import { test as base, expect } from "@playwright/test";
export { expect };
export const test = base.extend({
  seedSession: [true, { option: true }],
  _session: [
    async ({ request, seedSession }, use) => {
      if (seedSession) {
        const state = await (await request.get("/api/state")).json();
        if (!state.sessions.length) {
          const created = await request.post("/api/sessions", {
            headers: { "x-mnemoarc-client": "web" },
            data: { project: state.config.projects[0], workflow: "answer" },
          });
          expect(created.ok()).toBe(true);
        }
      }
      await use();
    },
    { auto: true },
  ],
});
