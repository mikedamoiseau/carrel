import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface KidsModeStatus {
  enabled: boolean;
  codeSet: boolean;
}

export interface UseKidsMode extends KidsModeStatus {
  /** A `get_kids_mode` read has succeeded, so `enabled` is the backend's
   *  answer. Until then Kids mode may be on: hide the admin controls. */
  checked: boolean;
  /** Nothing is known yet and the last read failed. */
  failed: boolean;
  /** Re-read the status from the backend (e.g. after setting the code). */
  refresh: () => Promise<void>;
}

/**
 * Kids mode state, mirrored from the backend (the single source of truth).
 * Reads `get_kids_mode` on mount and follows the `kids-mode-changed` event,
 * so every mounted instance converges — same shape as `usePrivateMode`.
 * Fails closed: callers treat `!checked` like Kids mode for what they hide.
 */
export function useKidsMode(): UseKidsMode {
  const [status, setStatus] = useState<KidsModeStatus>({ enabled: false, codeSet: false });
  const [checked, setChecked] = useState(false);
  const [failed, setFailed] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setStatus(await invoke<KidsModeStatus>("get_kids_mode"));
      setChecked(true);
      setFailed(false);
    } catch {
      // keep the last known state
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    invoke<KidsModeStatus>("get_kids_mode")
      .then((s) => {
        if (cancelled) return;
        setStatus(s);
        setChecked(true);
        setFailed(false);
      })
      .catch(() => { if (!cancelled) setFailed(true); });
    const unlisten = listen<boolean>("kids-mode-changed", (event) => {
      setStatus((prev) => ({ ...prev, enabled: event.payload }));
    });
    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
    };
  }, []);

  return { ...status, checked, failed: failed && !checked, refresh };
}
