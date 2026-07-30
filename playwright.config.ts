import { defineConfig } from "@playwright/test";

// Single source of truth for the web-UI end-to-end suite. Playwright
// auto-discovers this root config, so both `npx playwright test` and
// `pnpm run test:e2e` run the same specs against the same deterministic,
// seeded Carrel web server — see `src-tauri/examples/web_e2e_server.rs` for
// the exact fixture set (130 books, known progress / zero-chapter / CBZ
// books) the specs in `e2e/` assert against.
//
// Playwright manages the harness lifecycle via `webServer` below: it
// builds+runs the example, polls `/api/health` until ready, and (outside
// CI, where `reuseExistingServer` is true) leaves an already-running
// instance alone so repeated local runs don't pay the cargo build cost.
const PORT = 7810;

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 60_000,
  expect: {
    timeout: 10_000,
  },
  reporter: [["html", { outputFolder: "./e2e/playwright-report", open: "never" }], ["list"]],
  outputDir: "./e2e/test-results",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    actionTimeout: 15_000,
    navigationTimeout: 30_000,
  },
  projects: [
    {
      name: "chromium",
      // channel:"chromium" runs the full Chromium binary's new headless mode
      // instead of the stripped chrome-headless-shell. The shell segfaulted
      // (SIGSEGV) intermittently on the Ubuntu CI runner once the offline-
      // mode service worker started intercepting book-route fetches
      // (respondWith on /api/books/{id}/... suite-wide), killing the shared
      // browser process and failing a random later test with
      // "browser has been closed". The full binary does not exhibit it.
      use: { browserName: "chromium", channel: "chromium" },
    },
  ],
  webServer: {
    command: "cargo run --quiet --example web_e2e_server",
    cwd: "src-tauri",
    url: `http://127.0.0.1:${PORT}/api/health`,
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
    env: { FOLIO_E2E_PORT: String(PORT) },
  },
});
