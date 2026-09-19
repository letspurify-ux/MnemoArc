import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "test/browser",
  workers: 1,
  fullyParallel: false,
  timeout: 45000,
  outputDir: "test-artifacts",
  reporter: "list",
  use: {
    baseURL: "http://127.0.0.1:3099",
    headless: true,
    channel: "chrome",
    viewport: { width: 1440, height: 980 },
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "node test/browser-server.mjs",
    url: "http://127.0.0.1:3099/api/state",
    reuseExistingServer: false,
    timeout: 30000,
  },
});
