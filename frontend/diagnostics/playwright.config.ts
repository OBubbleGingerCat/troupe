import { defineConfig, devices } from "@playwright/test";


const systemChromium = process.env.TROUPE_PLAYWRIGHT_EXECUTABLE_PATH;

export default defineConfig({
  testDir: "./tests",
  testMatch: "e2e/**/*.spec.ts",
  outputDir: process.env.TROUPE_FRONTEND_TEST_OUTPUT ?? "test-results",
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  use: {
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        ...(systemChromium === undefined ? {} : {
          launchOptions: { executablePath: systemChromium, args: ["--no-sandbox"] },
        }),
      },
    },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
