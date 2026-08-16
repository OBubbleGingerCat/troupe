import { defineConfig } from "@playwright/test";


function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (value === undefined || value.length === 0) {
    throw new Error(`missing required environment: ${name}`);
  }
  return value;
}


export default defineConfig({
  testDir: ".",
  testMatch: "trace.spec.ts",
  outputDir: requiredEnvironment("TROUPE_PERFETTO_UI_OUTPUT"),
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  workers: 1,
  reporter: [["line"]],
  timeout: 300_000,
  expect: { timeout: 30_000 },
  projects: [
    {
      name: "chromium",
      use: {
        browserName: "chromium",
        headless: true,
        launchOptions: {
          executablePath: requiredEnvironment("TROUPE_PERFETTO_UI_CHROMIUM"),
          args: [
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-domain-reliability",
            "--metrics-recording-only",
            "--no-first-run",
            "--host-resolver-rules=MAP * 0.0.0.0, EXCLUDE 127.0.0.1",
          ],
        },
      },
    },
  ],
});
