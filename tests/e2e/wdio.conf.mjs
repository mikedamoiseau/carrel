import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(__dirname, "../..");

export const config = {
  runner: "local",
  port: 4444,
  // Group all specs so they run in a single session (single app launch).
  // Launching a new session per spec kills and restarts the app, which
  // causes the dev server connection to break on subsequent launches.
  specs: [
    [
      "./specs/smoke.mjs",
      "./specs/window.mjs",
      "./specs/navigation.mjs",
      "./specs/library.mjs",
      "./specs/settings.mjs",
      "./specs/section-toggles.mjs",
      "./specs/theme.mjs",
      "./specs/import-dialog.mjs",
      "./specs/accessibility.mjs",
    ],
  ],
  maxInstances: 1,
  capabilities: [
    {
      "tauri:options": {
        binary: resolve(projectRoot, "src-tauri/target/debug/carrel"),
      },
    },
  ],
  logLevel: "info",
  waitforTimeout: 10000,
  connectionRetryTimeout: 30000,
  connectionRetryCount: 3,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 60000,
  },
};
