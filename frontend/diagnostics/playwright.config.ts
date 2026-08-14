import { defineConfig, devices } from "@playwright/test";


export default defineConfig({
  testDir: "./tests/e2e",
  outputDir: process.env.TROUPE_FRONTEND_TEST_OUTPUT ?? "test-results",
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  use: {
    trace: "retain-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
