import { availableParallelism } from "node:os";

import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "e2e",
  fullyParallel: true,
  workers: Math.max(1, Math.floor(availableParallelism() / 3)),
  forbidOnly: true,
  retries: 0,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  reporter: [["list"]],
  use: { trace: "retain-on-failure" },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
  ],
});
