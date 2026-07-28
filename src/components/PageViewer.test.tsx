// @vitest-environment jsdom
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import "@testing-library/jest-dom/vitest";

// jsdom lacks ResizeObserver, which PageViewer instantiates on mount to track
// the container width.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;

// jsdom lacks the Web Animations API. PageViewer's slide-in animation calls
// spreadRef.current.animate() (via requestAnimationFrame) on a page turn; stub
// it with a no-op that supports the onfinish/oncancel + cancel() the code uses.
globalThis.Element.prototype.animate = vi.fn(() => {
  const anim: {
    cancel: () => void;
    onfinish: (() => void) | null;
    oncancel: (() => void) | null;
    finished: Promise<void>;
  } = { cancel: () => {}, onfinish: null, oncancel: null, finished: Promise.resolve() };
  // Fire onfinish on the next microtask (after the caller assigns its handler),
  // so PageViewer clears isAnimating just as a real completed animation would —
  // otherwise a second page turn would be swallowed by the isAnimating guard.
  Promise.resolve().then(() => anim.onfinish?.());
  return anim;
}) as unknown as typeof Element.prototype.animate;

// Minimal i18n stub — PageViewer only needs t() to return *something*.
vi.mock("react-i18next", () => ({
  initReactI18next: { type: "3rdParty", init: () => {} },
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../components/Toast", () => ({ useToast: () => ({ addToast: vi.fn() }) }));

// Build a valid page-wire payload: raw image bytes with a trailing mime-tag
// byte (0 = JPEG — see mimeFromTag in ../lib/pageWire). blobUrlFromBytes slices
// the tag off the end, so the buffer just has to be non-empty.
function pageBytes(): ArrayBuffer {
  return new Uint8Array([0xff, 0xd8, 0xff, 0x00]).buffer;
}

// Tauri IPC mock. Page-render commands resolve a Promise<ArrayBuffer>; every
// other invoked command resolves a benign default so mount never throws
// (the PDF text layer fetches glyphs/text/highlights once an image renders).
const invokeMock = vi.fn((cmd: string, _args?: unknown) => {
  if (cmd === "get_pdf_page_bytes" || cmd === "get_comic_page_bytes") {
    return Promise.resolve(pageBytes());
  }
  return Promise.resolve([]);
});
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
  convertFileSrc: (p: string) => p,
}));

// jsdom has no object-URL implementation.
globalThis.URL.createObjectURL = vi.fn(() => "blob:mock");
globalThis.URL.revokeObjectURL = vi.fn();

import { render, fireEvent, cleanup, act } from "@testing-library/react";
import PageViewer from "./PageViewer";

// Foreground (non-preload) render invocations for the given command.
// Preload warms pass `cachedOnly`, so filtering those out isolates the
// user-facing backend re-fetches this test cares about.
function renderCalls(cmd: string) {
  return invokeMock.mock.calls.filter(
    (c) => c[0] === cmd && !(c[1] as { cachedOnly?: boolean })?.cachedOnly,
  );
}

async function flush() {
  // Let the mount page-load promise settle without advancing fake timers
  // (so the 250ms preload debounce stays dormant).
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.useRealTimers();
  localStorage.clear();
});

beforeEach(() => {
  vi.useFakeTimers();
});

describe("PageViewer — zoom re-fetch debounce", () => {
  it("collapses a rapid zoom burst into a single settled backend render", async () => {
    render(<PageViewer bookId="b1" format="pdf" totalPages={10} />);
    await flush();

    // Mount fetches page 0 at the fallback-width quantum (1600px).
    expect(renderCalls("get_pdf_page_bytes")).toHaveLength(1);
    expect(renderCalls("get_pdf_page_bytes")[0][1]).toMatchObject({ width: 1600 });

    // Zoom in four times (Ctrl+'='). In jsdom clientWidth is 0, so each +0.25
    // step crosses a 400px width quantum (2000 → 2400 → 2800 → 3200). Each
    // keypress is committed on its own tick (flush between, but do NOT advance
    // timers) — mirroring real keystrokes that repaint between events. WITHOUT
    // the debounce this fires one backend render per step.
    for (let i = 0; i < 4; i++) {
      await act(async () => {
        fireEvent.keyDown(window, { key: "=", ctrlKey: true });
        await Promise.resolve();
      });
    }

    // Mid-burst, before the settle window elapses: no new backend render — the
    // debounced render-zoom hasn't updated, so renderWidth is unchanged.
    expect(renderCalls("get_pdf_page_bytes")).toHaveLength(1);

    // Let the zoom settle.
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
    });

    const calls = renderCalls("get_pdf_page_bytes");
    // Exactly one render beyond the initial mount load, and it is the settled
    // width (1600 × zoom 2.0 = 3200), not any intermediate quantum.
    expect(calls).toHaveLength(2);
    expect(calls[1][1]).toMatchObject({ width: 3200 });
  });

  it("re-fetches immediately on a page turn (page turns are not debounced)", async () => {
    render(<PageViewer bookId="b1" format="pdf" totalPages={10} />);
    await flush();

    expect(renderCalls("get_pdf_page_bytes")).toHaveLength(1);

    // ArrowRight advances the page. The render must fire right away, WITHOUT
    // advancing past the zoom settle window — the debounce is on zoom only.
    act(() => {
      fireEvent.keyDown(window, { key: "ArrowRight" });
    });
    await act(async () => {
      await Promise.resolve();
    });

    const page1Calls = invokeMock.mock.calls.filter(
      (c) => c[0] === "get_pdf_page_bytes" && (c[1] as { pageIndex?: number })?.pageIndex === 1,
    );
    expect(page1Calls).toHaveLength(1);
  });

  it("turning a page while zoomed re-fetches once at the reset zoom, not twice", async () => {
    render(<PageViewer bookId="b1" format="pdf" totalPages={10} />);
    await flush();

    // Zoom to 2.0 and let it settle (sharp render at 3200).
    for (let i = 0; i < 4; i++) {
      await act(async () => {
        fireEvent.keyDown(window, { key: "=", ctrlKey: true });
        await Promise.resolve();
      });
    }
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
    });

    const page1 = () =>
      renderCalls("get_pdf_page_bytes").filter(
        (c) => (c[1] as { pageIndex?: number })?.pageIndex === 1,
      );

    // Turn the page. goTo resets zoom -> 1. The new page must be fetched ONCE,
    // immediately, at the zoom-1 width (1600) — NOT first at the stale zoom-2
    // width (3200) and again ~200ms later. That stale-then-correct double
    // render is the regression this guards against: because the page-turn
    // render snaps the debounced zoom synchronously, only one fetch is issued.
    act(() => {
      fireEvent.keyDown(window, { key: "ArrowRight" });
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(page1()).toHaveLength(1);
    expect(page1()[0][1]).toMatchObject({ pageIndex: 1, width: 1600 });

    // Advancing past the settle window must NOT add a second lagged render.
    await act(async () => {
      vi.advanceTimersByTime(250);
      await Promise.resolve();
    });
    expect(page1()).toHaveLength(1);
    expect(page1()[0][1]).toMatchObject({ width: 1600 });
  });
});
