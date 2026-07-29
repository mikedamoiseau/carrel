// @vitest-environment jsdom
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import "@testing-library/jest-dom/vitest";
import { invoke } from "@tauri-apps/api/core";
import AnalyticsConsentDialog from "./AnalyticsConsentDialog";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => {
      const map: Record<string, string> = {
        "settings.analyticsTitle": "Help improve Carrel?",
        "settings.analyticsMessage":
          "Carrel can send anonymous usage statistics (app launches, your OS and version) to help us understand how many people use the app. No personal data, book titles, or library contents are ever sent. You can change this anytime in Settings.",
        "settings.analyticsEnable": "Enable",
        "settings.analyticsNotNow": "Not now",
      };
      return map[key] ?? key;
    },
  }),
}));

const mockInvoke = vi.mocked(invoke);

describe("AnalyticsConsentDialog", () => {
  beforeEach(() => mockInvoke.mockReset());

  it("stays hidden when consent is already set", async () => {
    mockInvoke.mockResolvedValueOnce("enabled");
    render(<AnalyticsConsentDialog />);
    await waitFor(() => expect(mockInvoke).toHaveBeenCalledWith("get_analytics_consent"));
    expect(screen.queryByText(/Help improve Carrel/i)).not.toBeInTheDocument();
  });

  it("shows when unset and writes 'enabled' on Enable", async () => {
    mockInvoke.mockResolvedValueOnce("unset").mockResolvedValueOnce(undefined);
    render(<AnalyticsConsentDialog />);
    await screen.findByText(/Help improve Carrel/i);
    fireEvent.click(screen.getByRole("button", { name: /Enable/i }));
    expect(mockInvoke).toHaveBeenCalledWith("set_analytics_consent", { consent: "enabled" });
    await waitFor(() => expect(screen.queryByText(/Help improve Carrel/i)).not.toBeInTheDocument());
  });

  it("writes 'disabled' on Not now", async () => {
    mockInvoke.mockResolvedValueOnce("unset").mockResolvedValueOnce(undefined);
    render(<AnalyticsConsentDialog />);
    await screen.findByText(/Help improve Carrel/i);
    fireEvent.click(screen.getByRole("button", { name: /Not now/i }));
    expect(mockInvoke).toHaveBeenCalledWith("set_analytics_consent", { consent: "disabled" });
  });
});
