// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import WhatsNewBanner from "../WhatsNewBanner";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, string>) => {
      const map: Record<string, string> = {
        "whatsNew.bannerTitle": `Carrel ${opts?.version ?? ""}`,
        "whatsNew.bannerSummary": `${opts?.title ?? ""} and more`,
        "whatsNew.bannerCta": "See what's new",
        "reader.dismiss": "Dismiss",
      };
      return map[key] ?? key;
    },
  }),
}));

afterEach(() => cleanup());

describe("WhatsNewBanner", () => {
  const props = {
    version: "2.0.3",
    summary: "OPDS feed primitives",
    onClickCta: vi.fn(),
    onDismiss: vi.fn(),
  };

  it("renders version and summary", () => {
    render(<WhatsNewBanner {...props} />);
    expect(screen.getByText("Carrel 2.0.3")).toBeInTheDocument();
    expect(screen.getByText("OPDS feed primitives and more")).toBeInTheDocument();
  });

  it("calls onClickCta when CTA clicked", () => {
    render(<WhatsNewBanner {...props} />);
    fireEvent.click(screen.getByText("See what's new", { exact: false }));
    expect(props.onClickCta).toHaveBeenCalled();
  });

  it("calls onDismiss when dismiss button clicked", () => {
    render(<WhatsNewBanner {...props} />);
    fireEvent.click(screen.getByLabelText("Dismiss"));
    expect(props.onDismiss).toHaveBeenCalled();
  });
});
