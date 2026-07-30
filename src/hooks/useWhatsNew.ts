import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { releaseNotes, appVersion } from "virtual:release-notes";
import type { ReleaseVersion } from "../../vite-plugin-release-notes";

const DISMISSED_KEY = "carrel-whats-new-dismissed";
const ONBOARDING_KEY = "carrel-onboarding-complete";

export interface UseWhatsNew {
  showBanner: boolean;
  showModal: boolean;
  openModal: () => void;
  closeModal: () => void;
  dismissBanner: () => void;
  currentRelease: ReleaseVersion | null;
  flagLoaded: boolean;
}

export function useWhatsNew(): UseWhatsNew {
  const [flagEnabled, setFlagEnabled] = useState<boolean | null>(null);
  const [dismissed, setDismissed] = useState(
    () => localStorage.getItem(DISMISSED_KEY) === appVersion,
  );
  const [showModal, setShowModal] = useState(false);

  const currentRelease = useMemo(
    () => releaseNotes.find((r) => r.version === appVersion) ?? null,
    [],
  );

  const onboardingComplete = useMemo(
    () => localStorage.getItem(ONBOARDING_KEY) === "true",
    [],
  );

  useEffect(() => {
    invoke<boolean>("get_feature_flag_value", { key: "whats_new_banner" })
      .then(setFlagEnabled)
      .catch(() => setFlagEnabled(false));
  }, []);

  useEffect(() => {
    const unlisten = listen("whats-new-open", () => setShowModal(true));
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  const showBanner =
    flagEnabled === true &&
    !dismissed &&
    onboardingComplete &&
    currentRelease !== null;

  const dismissBanner = useCallback(() => {
    localStorage.setItem(DISMISSED_KEY, appVersion);
    setDismissed(true);
  }, []);

  const openModal = useCallback(() => setShowModal(true), []);
  const closeModal = useCallback(() => setShowModal(false), []);

  return {
    showBanner,
    showModal,
    openModal,
    closeModal,
    dismissBanner,
    currentRelease,
    flagLoaded: flagEnabled !== null,
  };
}
