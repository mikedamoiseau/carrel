import { useState, useCallback } from "react";

export const STORAGE_KEY = "carrel-onboarding-complete";

type Step = 1 | 2 | 3 | 4;

export interface UseOnboarding {
  isActive: boolean;
  currentStep: Step;
  advance: () => void;
  skip: () => void;
  complete: () => void;
  restart: () => void;
}

export function useOnboarding(): UseOnboarding {
  const [isActive, setIsActive] = useState(
    () => localStorage.getItem(STORAGE_KEY) !== "true"
  );
  const [currentStep, setCurrentStep] = useState<Step>(1);

  const dismiss = useCallback(() => {
    localStorage.setItem(STORAGE_KEY, "true");
    setIsActive(false);
  }, []);

  const advance = useCallback(() => {
    setCurrentStep((s) => (s < 4 ? ((s + 1) as Step) : s));
  }, []);

  const restart = useCallback(() => {
    localStorage.removeItem(STORAGE_KEY);
    setCurrentStep(1);
    setIsActive(true);
  }, []);

  return { isActive, currentStep, advance, skip: dismiss, complete: dismiss, restart };
}
