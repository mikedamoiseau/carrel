import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface KidsModeStatus {
  enabled: boolean;
  codeSet: boolean;
}

export interface UseKidsMode extends KidsModeStatus {
  /** Re-read the status from the backend (e.g. after setting the code). */
  refresh: () => Promise<void>;
}

/**
 * Kids mode state, mirrored from the backend (the single source of truth).
 * Reads `get_kids_mode` on mount and follows the `kids-mode-changed` event,
 * so every mounted instance converges — same shape as `usePrivateMode`.
 * Fails to "off": the backend still enforces the filter either way.
 */
export function useKidsMode(): UseKidsMode {
  const [status, setStatus] = useState<KidsModeStatus>({ enabled: false, codeSet: false });

  const refresh = useCallback(async () => {
    try {
      setStatus(await invoke<KidsModeStatus>("get_kids_mode"));
    } catch {
      // keep the last known state
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    invoke<KidsModeStatus>("get_kids_mode")
      .then((s) => { if (!cancelled) setStatus(s); })
      .catch(() => {});
    const unlisten = listen<boolean>("kids-mode-changed", (event) => {
      setStatus((prev) => ({ ...prev, enabled: event.payload }));
    });
    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
    };
  }, []);

  return { ...status, refresh };
}
