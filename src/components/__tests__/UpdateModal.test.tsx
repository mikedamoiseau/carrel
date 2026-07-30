// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import UpdateModal, { isTrustedReleaseUrl, isTrustedChangelogUrl, type UpdateCheck } from "../UpdateModal";

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(() => Promise.resolve()) }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (k: string, o?: Record<string, unknown>) =>
      o && "version" in o ? `${k}:${o.version}` : k,
  }),
}));
vi.mock("../Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));

import { openUrl } from "@tauri-apps/plugin-opener";

const sample: UpdateCheck = {
  update_available: true,
  current_version: "2.7.0",
  latest_version: "2.8.0",
  release_url: "https://github.com/mikedamoiseau/carrel/releases/tag/v2.8.0",
  changelog_url: "https://github.com/mikedamoiseau/carrel/releases",
  release_notes: "Line one\nLine two",
};

describe("trusted-URL validation", () => {
  it("accepts the exact expected release + changelog URLs", () => {
    expect(isTrustedReleaseUrl(sample.release_url)).toBe(true);
    expect(isTrustedChangelogUrl(sample.changelog_url)).toBe(true);
    expect(isTrustedChangelogUrl(sample.changelog_url + "/")).toBe(true); // trailing slash ok
  });
  it("rejects deceptive hosts and schemes", () => {
    expect(isTrustedReleaseUrl("https://github.com.evil.org/mikedamoiseau/carrel/releases/tag/v1")).toBe(false);
    expect(isTrustedReleaseUrl("http://github.com/mikedamoiseau/carrel/releases/tag/v1")).toBe(false);
    expect(isTrustedReleaseUrl("https://evil.com/mikedamoiseau/carrel/releases/tag/v1")).toBe(false);
    expect(isTrustedReleaseUrl("not a url")).toBe(false);
  });
  it("rejects other repo paths (issues, PRs, bare repo, releases page as a release URL)", () => {
    expect(isTrustedReleaseUrl("https://github.com/mikedamoiseau/carrel/issues/1")).toBe(false);
    expect(isTrustedReleaseUrl("https://github.com/mikedamoiseau/carrel/pull/1")).toBe(false);
    expect(isTrustedReleaseUrl("https://github.com/mikedamoiseau/carrel/releases")).toBe(false);
    expect(isTrustedChangelogUrl("https://github.com/mikedamoiseau/carrel/releases/tag/v1")).toBe(false);
    expect(isTrustedChangelogUrl("https://github.com/mikedamoiseau/carrel/issues")).toBe(false);
  });
});

describe("UpdateModal", () => {
  beforeEach(() => vi.clearAllMocks());
  afterEach(() => cleanup());

  it("shows update-available with a Download button that opens the release URL", () => {
    render(<UpdateModal state={{ status: "available", data: sample }} onClose={() => {}} />);
    expect(screen.getByText("updateCheck.newVersion:2.8.0")).toBeTruthy();
    fireEvent.click(screen.getByText("updateCheck.download"));
    expect(openUrl).toHaveBeenCalledWith(sample.release_url);
  });

  it("focuses the Download button (not Close) in the available state", () => {
    render(<UpdateModal state={{ status: "available", data: sample }} onClose={() => {}} />);
    expect(document.activeElement).toBe(screen.getByText("updateCheck.download"));
  });

  it("does not focus the Close button in states without a primary action", () => {
    const { container } = render(
      <UpdateModal state={{ status: "uptodate", data: sample }} onClose={() => {}} />,
    );
    expect(document.activeElement).not.toBe(screen.getByLabelText("updateCheck.close"));
    // Focus rests on the dialog container itself, so no ✕ focus ring shows.
    expect(document.activeElement).toBe(container.querySelector('[role="dialog"]'));
  });

  it("opens the changelog via the external opener", () => {
    render(<UpdateModal state={{ status: "available", data: sample }} onClose={() => {}} />);
    fireEvent.click(screen.getByText("updateCheck.fullChangelog"));
    expect(openUrl).toHaveBeenCalledWith(sample.changelog_url);
  });

  it("does NOT open an untrusted release URL", () => {
    const evil = { ...sample, release_url: "https://evil.com/x", changelog_url: "https://evil.com/y" };
    render(<UpdateModal state={{ status: "available", data: evil }} onClose={() => {}} />);
    fireEvent.click(screen.getByText("updateCheck.download"));
    fireEvent.click(screen.getByText("updateCheck.fullChangelog"));
    expect(openUrl).not.toHaveBeenCalled();
  });

  it("renders hostile notes as text, not HTML", () => {
    const hostile = { ...sample, release_notes: '<img src=x onerror="alert(1)">hello' };
    const { container } = render(<UpdateModal state={{ status: "available", data: hostile }} onClose={() => {}} />);
    expect(container.querySelector("img")).toBeNull();
    expect(screen.getByText(/<img src=x/)).toBeTruthy();
  });

  it("shows empty-notes state when notes are blank", () => {
    render(<UpdateModal state={{ status: "available", data: { ...sample, release_notes: "" } }} onClose={() => {}} />);
    expect(screen.getByText("updateCheck.notesEmpty")).toBeTruthy();
  });

  it("omits Download in loading / up-to-date / error states", () => {
    const { rerender } = render(<UpdateModal state={{ status: "loading" }} onClose={() => {}} />);
    expect(screen.queryByText("updateCheck.download")).toBeNull();
    rerender(<UpdateModal state={{ status: "uptodate", data: sample }} onClose={() => {}} />);
    expect(screen.queryByText("updateCheck.download")).toBeNull();
    rerender(<UpdateModal state={{ status: "error", rateLimited: false }} onClose={() => {}} />);
    expect(screen.queryByText("updateCheck.download")).toBeNull();
  });

  it("shows up-to-date body", () => {
    render(<UpdateModal state={{ status: "uptodate", data: sample }} onClose={() => {}} />);
    expect(screen.getByText("updateCheck.upToDateBody:2.8.0")).toBeTruthy();
  });

  it("shows rate-limit vs generic error body", () => {
    const { rerender } = render(<UpdateModal state={{ status: "error", rateLimited: true }} onClose={() => {}} />);
    expect(screen.getByText("updateCheck.rateLimitBody")).toBeTruthy();
    rerender(<UpdateModal state={{ status: "error", rateLimited: false }} onClose={() => {}} />);
    expect(screen.getByText("updateCheck.errorBody")).toBeTruthy();
  });

  it("closes via close button, backdrop, and Escape", () => {
    const onClose = vi.fn();
    const { container } = render(<UpdateModal state={{ status: "loading" }} onClose={onClose} />);
    fireEvent.click(screen.getByLabelText("updateCheck.close"));
    fireEvent.click(container.firstChild as Element); // backdrop
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(3);
  });
});
