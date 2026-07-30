import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toCarrelError } from "../lib/errors";
import type { UpdateCheck, UpdateModalState } from "../components/UpdateModal";

const TOGGLE_KEY = "update_check_on_startup";

/** Interpret the stored toggle value: null (unset) → enabled by default.
 * Exported + unit-tested; also reused by SettingsPanel's load. */
export function startupCheckEnabled(v: string | null): boolean {
  return v === null || v === "true";
}

/** Read the toggle; a read failure → disabled (no automatic check). */
async function startupEnabled(): Promise<boolean> {
  try {
    return startupCheckEnabled(await invoke<string | null>("get_setting_value", { key: TOGGLE_KEY }));
  } catch {
    return false;
  }
}

export function useUpdateCheck(onboardingActive: boolean) {
  const [modal, setModal] = useState<UpdateModalState | null>(null);
  const mode = useRef<null | "auto" | "manual">(null); // in-flight mode
  const mounted = useRef(true);
  const gen = useRef(0); // bumped on close(); invalidates in-flight runCheck presentations
  const prevOnboarding = useRef(onboardingActive);
  const onboardingActiveRef = useRef(onboardingActive);

  useEffect(() => {
    onboardingActiveRef.current = onboardingActive;
  }, [onboardingActive]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const present = useCallback((s: UpdateModalState) => {
    if (mounted.current) setModal(s);
  }, []);

  const runCheck = useCallback(
    async (requested: "auto" | "manual") => {
      // A request is already running: upgrade auto→manual presentation; else dedupe.
      if (mode.current !== null) {
        if (mode.current === "auto" && requested === "manual") {
          mode.current = "manual";
          present({ status: "loading" });
        }
        return; // the single in-flight request will present per mode.current
      }
      mode.current = requested;
      const myGen = ++gen.current;
      if (requested === "manual" && gen.current === myGen) present({ status: "loading" });
      try {
        const data = await invoke<UpdateCheck>("check_for_update");
        // Auto path gates on the toggle; re-check mode.current after EACH await
        // so a manual upgrade that arrives during the toggle read isn't lost.
        if (mode.current === "auto") {
          const enabled = await startupEnabled();
          if (mode.current === "auto") {
            if (enabled && data.update_available && gen.current === myGen) present({ status: "available", data });
            return;
          }
        }
        // Manual (initial, or upgraded from auto at any point above): always present.
        if (gen.current === myGen) {
          present(
            data.update_available
              ? { status: "available", data }
              : { status: "uptodate", data },
          );
        }
      } catch (err) {
        if (mode.current === "manual" && gen.current === myGen) {
          present({ status: "error", rateLimited: toCarrelError(err).message === "rate_limited" });
        }
        // auto: swallow
      } finally {
        mode.current = null;
      }
    },
    [present],
  );

  // Manual trigger while the window is already open.
  useEffect(() => {
    const unlisten = listen("check-update-open", async () => {
      if (onboardingActiveRef.current) return; // leave the flag set; consumed on completion
      try {
        const pending = await invoke<boolean>("take_pending_manual_update_check");
        if (pending) runCheck("manual");
      } catch {
        /* ignore transient consume failure */
      }
    });
    return () => {
      unlisten.then((fn) => fn()).catch(() => {});
    };
  }, [runCheck]);

  // Gated consume: on mount if onboarding already inactive, and on every
  // active→inactive transition (e.g. onboarding re-run from Settings). The
  // backend tokens make repeat runs safe: take_startup_update_check grants
  // once per process; take_pending_manual_update_check is atomic.
  useEffect(() => {
    prevOnboarding.current = onboardingActive;
    if (onboardingActive) return;
    (async () => {
      try {
        const pending = await invoke<boolean>("take_pending_manual_update_check");
        if (pending) {
          await invoke<boolean>("take_startup_update_check"); // consume so auto won't double-fire
          runCheck("manual");
          return;
        }
        const startupTurn = await invoke<boolean>("take_startup_update_check");
        if (startupTurn && (await startupEnabled())) runCheck("auto");
      } catch {
        /* transient consume failure → no automatic check this launch */
      }
    })();
  }, [onboardingActive, runCheck]);

  const close = useCallback(() => {
    gen.current++;
    setModal(null);
  }, []);
  return { modal, close };
}
