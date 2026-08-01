// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import "@testing-library/jest-dom/vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
// Capture the `catalog-search-progress` listener so tests can drive live
// per-catalog progress events, and hand back an unlisten stub.
let progressHandler: ((e: { payload: unknown }) => void) | null = null;
const listen = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({ listen: (...a: unknown[]) => listen(...a) }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (k: string, p?: Record<string, unknown>) => (p ? `${k}:${JSON.stringify(p)}` : k) }),
}));
vi.mock("../lib/supportedFormats", () => ({
  // Real shape is a Set (pickSupportedOpdsLink calls `.has` on it) — an array
  // mock would only work as long as no test exercised a download link.
  FALLBACK_FORMATS: new Set(["epub"]),
  useSupportedFormats: () => new Set(["epub", "pdf"]),
}));
vi.mock("./OpdsPresetPicker", () => ({ default: () => null }));
vi.mock("../lib/useFocusTrap", () => ({ useFocusTrap: () => ({ current: null }) }));

import { render, screen, cleanup, fireEvent, act, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import CatalogBrowser from "./CatalogBrowser";

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
  progressHandler = null;
  // Capture the progress listener; return an unlisten stub.
  listen.mockImplementation((name: string, cb: (e: { payload: unknown }) => void) => {
    if (name === "catalog-search-progress") progressHandler = cb;
    return Promise.resolve(() => {});
  });
  invoke.mockImplementation((cmd: string) => {
    if (cmd === "get_opds_catalogs") return Promise.resolve([]);
    return Promise.resolve(undefined);
  });
});
afterEach(() => cleanup());

async function openAddForm() {
  render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_opds_catalogs"));
  // Reveal the add-catalog form
  const addToggle = await screen.findByText("catalog.addCustomCatalog");
  await act(async () => fireEvent.click(addToggle));
}

async function fillForm(name: string, url: string) {
  const nameInput = screen.getByPlaceholderText("catalog.catalogName");
  const urlInput = screen.getByPlaceholderText("catalog.opdsFeedUrl");
  await act(async () => {
    fireEvent.change(nameInput, { target: { value: name } });
    fireEvent.change(urlInput, { target: { value: url } });
  });
}

describe("CatalogBrowser add-catalog validation", () => {
  it("rejects an invalid URL without calling the backend", async () => {
    await openAddForm();
    await fillForm("My Feed", "not-a-url");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.add" })));

    expect(invoke).not.toHaveBeenCalledWith("browse_opds", expect.anything());
    expect(invoke).not.toHaveBeenCalledWith("add_opds_catalog", expect.anything());
    expect(screen.getByText("catalog.invalidUrl")).toBeInTheDocument();
  });

  it("saves then connection-tests via browse_opds for a valid URL (no rollback on success)", async () => {
    await openAddForm();
    await fillForm("My Feed", "https://example.com/opds");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.add" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("browse_opds", { url: "https://example.com/opds", catalogUrl: "https://example.com/opds" }),
    );
    expect(invoke).toHaveBeenCalledWith("add_opds_catalog", { name: "My Feed", url: "https://example.com/opds" });
    // success → no rollback
    expect(invoke).not.toHaveBeenCalledWith("remove_opds_catalog", expect.anything());
  });

  it("rolls back the add when the connection test fails", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([]);
      if (cmd === "browse_opds") return Promise.reject(new Error("connection refused"));
      return Promise.resolve(undefined);
    });
    await openAddForm();
    await fillForm("Broken", "https://bad.example/opds");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.add" })));

    await waitFor(() => expect(screen.getByText(/catalog\.connectionTestFailed/)).toBeInTheDocument());
    // it was provisionally added, then rolled back
    expect(invoke).toHaveBeenCalledWith("add_opds_catalog", { name: "Broken", url: "https://bad.example/opds" });
    expect(invoke).toHaveBeenCalledWith("remove_opds_catalog", { url: "https://bad.example/opds" });
  });
});

async function openSignIn() {
  await act(async () => fireEvent.click(screen.getByText("catalog.addSignIn")));
}

async function fillBasicCredentials(username: string, password: string) {
  await act(async () => {
    fireEvent.change(screen.getByPlaceholderText("catalog.authUsername"), { target: { value: username } });
    fireEvent.change(screen.getByPlaceholderText("catalog.authPassword"), { target: { value: password } });
  });
}

describe("CatalogBrowser add-catalog credentials", () => {
  it("adds a catalog and its credentials through one command", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await openAddForm();
    await fillForm("Secure Feed", "https://secure.example/opds");
    await openSignIn();
    await fillBasicCredentials("alice", "hunter2");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.add" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("add_opds_catalog_with_auth", {
        name: "Secure Feed",
        url: "https://secure.example/opds",
        presetId: null,
        kind: "basic",
        username: "alice",
        secret: "hunter2",
        allowInsecure: false,
      }),
    );
    expect(invoke).not.toHaveBeenCalledWith("add_opds_catalog", expect.anything());
    expect(invoke).not.toHaveBeenCalledWith("browse_opds", expect.anything());
  });

  it("keeps the credential-free path on the two-step add", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await openAddForm();
    await fillForm("Public Feed", "https://public.example/opds");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.add" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("add_opds_catalog", { name: "Public Feed", url: "https://public.example/opds" }),
    );
    expect(invoke).toHaveBeenCalledWith("browse_opds", {
      url: "https://public.example/opds",
      catalogUrl: "https://public.example/opds",
    });
    expect(invoke).not.toHaveBeenCalledWith("add_opds_catalog_with_auth", expect.anything());
  });

  it("requires acknowledging cleartext before sending a credential to a LAN host", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await openAddForm();
    await fillForm("LAN Feed", "http://192.168.0.50:8080/opds");
    await openSignIn();
    await fillBasicCredentials("bob", "s3cret");

    expect(screen.getByText("catalog.insecureCredentialWarning")).toBeInTheDocument();
    const addBtn = screen.getByRole("button", { name: "common.add" });
    expect(addBtn).toBeDisabled();

    await act(async () => fireEvent.click(screen.getByRole("checkbox")));
    expect(addBtn).not.toBeDisabled();
    await act(async () => fireEvent.click(addBtn));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "add_opds_catalog_with_auth",
        expect.objectContaining({ allowInsecure: true }),
      ),
    );
  });

  it("does not warn for a loopback URL", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    await openAddForm();
    await fillForm("Local Feed", "http://localhost:8080/opds");
    await openSignIn();
    await fillBasicCredentials("bob", "s3cret");

    expect(screen.queryByText("catalog.insecureCredentialWarning")).not.toBeInTheDocument();
    const addBtn = screen.getByRole("button", { name: "common.add" });
    expect(addBtn).not.toBeDisabled();
    await act(async () => fireEvent.click(addBtn));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "add_opds_catalog_with_auth",
        expect.objectContaining({ allowInsecure: false }),
      ),
    );
  });
});

describe("CatalogBrowser empty state", () => {
  it("shows the no-catalogs empty state only after a successful empty load, not while loading", async () => {
    // Hold the load open so we can assert the empty state is NOT shown until
    // `get_opds_catalogs` resolves (i.e. it must not flash on initial load).
    let resolveCatalogs!: (v: unknown[]) => void;
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs")
        return new Promise((res) => {
          resolveCatalogs = res as (v: unknown[]) => void;
        });
      return Promise.resolve(undefined);
    });
    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_opds_catalogs"));

    // While the load is still pending, the empty state must not appear.
    expect(screen.queryByText("catalog.empty.title")).not.toBeInTheDocument();

    // Resolve the load to an empty list — now the empty state appears.
    await act(async () => {
      resolveCatalogs([]);
    });

    expect(await screen.findByText("catalog.empty.title")).toBeInTheDocument();
    expect(screen.getByText("catalog.empty.subtitle")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "catalog.empty.browsePresets" })
    ).toBeInTheDocument();
  });

  it("does not show the empty state after a failed load", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.reject(new Error("boom"));
      return Promise.resolve(undefined);
    });
    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_opds_catalogs"));

    // Load failed → `catalogsLoaded` stays false → empty state hidden even
    // though `catalogs` is still [].
    expect(screen.queryByText("catalog.empty.title")).not.toBeInTheDocument();
  });

  it("hides the empty state once a catalog exists", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs")
        return Promise.resolve([{ name: "My Feed", url: "https://example.com/opds" }]);
      return Promise.resolve(undefined);
    });
    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("My Feed")).toBeInTheDocument());

    expect(screen.queryByText("catalog.empty.title")).not.toBeInTheDocument();
  });
});

describe("CatalogBrowser remove confirmation", () => {
  it("confirms before removing a catalog (no immediate backend call)", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs")
        return Promise.resolve([{ name: "My Feed", url: "https://example.com/opds" }]);
      return Promise.resolve(undefined);
    });
    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("My Feed")).toBeInTheDocument());

    await act(async () => fireEvent.click(screen.getByLabelText(/catalog\.removeCatalog/)));
    expect(invoke).not.toHaveBeenCalledWith("remove_opds_catalog", expect.anything());
    expect(screen.getByRole("dialog")).toBeInTheDocument();

    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.remove" })));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("remove_opds_catalog", { url: "https://example.com/opds" })
    );
  });
});

describe("CatalogBrowser unified search live progress", () => {
  const CATALOGS = [
    { name: "Gutenberg", url: "https://g/opds" },
    { name: "Feedbooks", url: "https://f/opds" },
  ];

  it("shows a per-catalog checklist and ticks each off as its progress event arrives", async () => {
    let resolveSearch!: (v: unknown[]) => void;
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve(CATALOGS);
      // Keep the unified search pending so we can drive progress events while
      // the checklist is on screen.
      if (cmd === "search_all_catalogs")
        return new Promise((res) => {
          resolveSearch = res as (v: unknown[]) => void;
        });
      return Promise.resolve(undefined);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("Gutenberg")).toBeInTheDocument());

    const input = screen.getByPlaceholderText("catalog.searchAllPlaceholder");
    await act(async () => fireEvent.change(input, { target: { value: "bible" } }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.search" })));

    // Checklist header + both catalogs seeded as pending (no counts yet).
    expect(await screen.findByText("catalog.searchingAll")).toBeInTheDocument();
    expect(screen.getByText("Gutenberg")).toBeInTheDocument();
    expect(screen.getByText("Feedbooks")).toBeInTheDocument();
    expect(screen.queryByText('catalog.searchCatalogCount:{"count":14}')).not.toBeInTheDocument();

    // Gutenberg finishes with 14 results.
    await act(async () => {
      progressHandler?.({
        payload: { query: "bible", url: "https://g/opds", name: "Gutenberg", count: 14, ok: true },
      });
    });
    expect(screen.getByText('catalog.searchCatalogCount:{"count":14}')).toBeInTheDocument();

    // Feedbooks fails.
    await act(async () => {
      progressHandler?.({
        payload: { query: "bible", url: "https://f/opds", name: "Feedbooks", count: 0, ok: false },
      });
    });
    expect(screen.getByText("catalog.searchCatalogFailed")).toBeInTheDocument();

    // Finishing the search holds the completed checklist on screen for
    // RESULTS_REVEAL_DELAY_MS (2 s) so the last tick is readable...
    await act(async () => resolveSearch([]));
    expect(screen.getByText("catalog.searchingAll")).toBeInTheDocument();
    // ...then flips to results. Allow past the 2 s hold.
    await waitFor(() => expect(screen.getByText("common.noResults")).toBeInTheDocument(), {
      timeout: 3000,
    });
    expect(screen.queryByText("catalog.searchingAll")).not.toBeInTheDocument();
  });

  it("ignores progress events from a stale (mismatched-query) search", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve(CATALOGS);
      // Leave the search pending — we only assert the stale event is ignored.
      if (cmd === "search_all_catalogs") return new Promise(() => {});
      return Promise.resolve(undefined);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("Gutenberg")).toBeInTheDocument());

    const input = screen.getByPlaceholderText("catalog.searchAllPlaceholder");
    await act(async () => fireEvent.change(input, { target: { value: "bible" } }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.search" })));
    await screen.findByText("catalog.searchingAll");

    // An event tagged with a different query must not update the checklist.
    await act(async () => {
      progressHandler?.({
        payload: { query: "shakespeare", url: "https://g/opds", name: "Gutenberg", count: 99, ok: true },
      });
    });
    expect(screen.queryByText('catalog.searchCatalogCount:{"count":99}')).not.toBeInTheDocument();
  });

  it("reveals results under StrictMode (mount→unmount→remount must not freeze the reveal)", async () => {
    let resolveSearch!: (v: unknown[]) => void;
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve(CATALOGS);
      if (cmd === "search_all_catalogs")
        return new Promise((res) => {
          resolveSearch = res as (v: unknown[]) => void;
        });
      return Promise.resolve(undefined);
    });

    // StrictMode double-invokes effects; the mountedRef guard must survive it,
    // otherwise the checklist never flips to results (regression).
    render(
      <StrictMode>
        <CatalogBrowser onClose={() => {}} onBookImported={() => {}} />
      </StrictMode>,
    );
    await waitFor(() => expect(screen.getByText("Gutenberg")).toBeInTheDocument());

    const input = screen.getByPlaceholderText("catalog.searchAllPlaceholder");
    await act(async () => fireEvent.change(input, { target: { value: "bible" } }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.search" })));
    await screen.findByText("catalog.searchingAll");

    await act(async () => resolveSearch([]));
    await waitFor(() => expect(screen.getByText("common.noResults")).toBeInTheDocument(), {
      timeout: 3000,
    });
    expect(screen.queryByText("catalog.searchingAll")).not.toBeInTheDocument();
  });
});

describe("CatalogBrowser pagination provenance", () => {
  it("paginates with the provenance the feed carried", async () => {
    invoke.mockImplementation((cmd: string, args?: { url?: string }) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "browse_opds") {
        if (args?.url === "https://a/opds") {
          return Promise.resolve({
            title: "A", entries: [], nextUrl: "https://a/opds?page=2", searchUrl: null, catalogUrl: "https://a/opds",
          });
        }
        return Promise.resolve({ title: "A p2", entries: [], nextUrl: null, searchUrl: null, catalogUrl: "https://a/opds" });
      }
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));
    await screen.findByText("catalog.loadMore");

    invoke.mockClear();
    await act(async () => fireEvent.click(screen.getByText("catalog.loadMore")));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("browse_opds", { url: "https://a/opds?page=2", catalogUrl: "https://a/opds" }),
    );
  });

  it("paginates with null once the backend cleared provenance", async () => {
    invoke.mockImplementation((cmd: string, args?: { url?: string }) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "browse_opds") {
        if (args?.url === "https://a/opds") {
          // Backend dropped provenance on this fetch (e.g. a cross-origin
          // hop) — catalogUrl comes back null even though we passed one in.
          return Promise.resolve({
            title: "A", entries: [], nextUrl: "https://other.example/opds?page=2", searchUrl: null, catalogUrl: null,
          });
        }
        return Promise.resolve({ title: "A p2", entries: [], nextUrl: null, searchUrl: null, catalogUrl: null });
      }
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));
    await screen.findByText("catalog.loadMore");

    invoke.mockClear();
    await act(async () => fireEvent.click(screen.getByText("catalog.loadMore")));

    // Must NOT re-assert the catalog's own URL as provenance — the component
    // has no "current catalog" memory to fall back on.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("browse_opds", { url: "https://other.example/opds?page=2", catalogUrl: null }),
    );
  });
});

const AUTH_REQUIRED_ERROR = { kind: "PermissionDenied", message: "OPDS auth required: HTTP 401" };

describe("CatalogBrowser sign-in prompt on 401/403", () => {
  it("prompts for sign-in when browsing returns 401 and retries after submit", async () => {
    let browseCalls = 0;
    invoke.mockImplementation((cmd: string, args?: { url?: string; catalogUrl?: string }) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "browse_opds") {
        browseCalls += 1;
        if (browseCalls === 1) return Promise.reject(AUTH_REQUIRED_ERROR);
        return Promise.resolve({ title: "A", entries: [], nextUrl: null, searchUrl: null, catalogUrl: args?.catalogUrl ?? null });
      }
      if (cmd === "get_opds_auth") return Promise.resolve(null);
      if (cmd === "set_opds_auth") return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));

    expect(await screen.findByText("catalog.signInRequired")).toBeInTheDocument();

    await act(async () => {
      fireEvent.change(screen.getByPlaceholderText("catalog.authUsername"), { target: { value: "user1" } });
      fireEvent.change(screen.getByPlaceholderText("catalog.authPassword"), { target: { value: "pass1" } });
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "catalog.signIn" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("set_opds_auth", {
        catalogUrl: "https://a/opds", kind: "basic", username: "user1", secret: "pass1", allowInsecure: false,
      }),
    );
    // Retried the same request that 401'd — same url and catalogUrl.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("browse_opds", { url: "https://a/opds", catalogUrl: "https://a/opds" }),
    );
    expect(browseCalls).toBe(2);
    expect(screen.queryByText("catalog.signInRequired")).not.toBeInTheDocument();
  });

  it("prompts bound to entry.catalogUrl when a download returns 403", async () => {
    let downloadCalls = 0;
    const entry = {
      id: "e1", title: "Book One", author: "", summary: "", coverUrl: null,
      links: [{ href: "https://a/dl/1.epub", mimeType: "application/epub+zip", rel: "http://opds-spec.org/acquisition" }],
      navUrl: null, catalogUrl: "https://a/opds",
    };
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "browse_opds") {
        return Promise.resolve({ title: "A", entries: [entry], nextUrl: null, searchUrl: null, catalogUrl: "https://a/opds" });
      }
      if (cmd === "download_opds_book") {
        downloadCalls += 1;
        if (downloadCalls === 1) return Promise.reject({ kind: "PermissionDenied", message: "OPDS auth required: HTTP 403" });
        return Promise.resolve({ id: "book-1", newly_imported: true });
      }
      if (cmd === "get_opds_auth") return Promise.resolve(null);
      if (cmd === "set_opds_auth") return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));
    await screen.findByText("Book One");

    await act(async () => fireEvent.click(screen.getByRole("button", { name: /epub/i })));
    expect(await screen.findByText("catalog.signInRequired")).toBeInTheDocument();

    await act(async () => {
      fireEvent.change(screen.getByPlaceholderText("catalog.authUsername"), { target: { value: "dl-user" } });
      fireEvent.change(screen.getByPlaceholderText("catalog.authPassword"), { target: { value: "dl-pass" } });
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "catalog.signIn" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("set_opds_auth", {
        catalogUrl: "https://a/opds", kind: "basic", username: "dl-user", secret: "dl-pass", allowInsecure: false,
      }),
    );
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("download_opds_book", {
        downloadUrl: "https://a/dl/1.epub", mimeType: "application/epub+zip", catalogUrl: "https://a/opds",
      }),
    );
    expect(downloadCalls).toBe(2);
  });

  it("prompts when a per-catalog search returns 401", async () => {
    let searchCalls = 0;
    invoke.mockImplementation((cmd: string, args?: { url?: string; catalogUrl?: string }) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "browse_opds") {
        if (args?.url === "https://a/opds") {
          return Promise.resolve({
            title: "A", entries: [], nextUrl: null, searchUrl: "https://a/search?q={searchTerms}", catalogUrl: "https://a/opds",
          });
        }
        // Any search-URL fetch: 401 once, then succeed.
        searchCalls += 1;
        if (searchCalls === 1) return Promise.reject(AUTH_REQUIRED_ERROR);
        return Promise.resolve({ title: "Results", entries: [], nextUrl: null, searchUrl: null, catalogUrl: args?.catalogUrl ?? null });
      }
      if (cmd === "get_opds_auth") return Promise.resolve(null);
      if (cmd === "set_opds_auth") return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));
    await screen.findByPlaceholderText("catalog.searchThisCatalog");

    await act(async () => {
      fireEvent.change(screen.getByPlaceholderText("catalog.searchThisCatalog"), { target: { value: "shakespeare" } });
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "common.search" })));

    expect(await screen.findByText("catalog.signInRequired")).toBeInTheDocument();

    // The auth panel's backdrop only intercepts pointer events, so nothing
    // stops keystrokes reaching the search box underneath while it's open.
    // The retry must replay the search that actually 401'd ("shakespeare"),
    // not whatever ends up in the box by the time the user signs in.
    act(() => {
      fireEvent.change(screen.getByPlaceholderText("catalog.searchThisCatalog"), { target: { value: "different-query" } });
    });

    await act(async () => {
      fireEvent.change(screen.getByPlaceholderText("catalog.authUsername"), { target: { value: "search-user" } });
      fireEvent.change(screen.getByPlaceholderText("catalog.authPassword"), { target: { value: "search-pass" } });
    });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "catalog.signIn" })));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("set_opds_auth", {
        catalogUrl: "https://a/opds", kind: "basic", username: "search-user", secret: "search-pass", allowInsecure: false,
      }),
    );
    expect(searchCalls).toBe(2);
    // Discriminating assertion: the retried browse_opds call must carry the
    // frozen url/catalogUrl from the request that failed, not a re-derived
    // one built from whatever is currently typed in the search box.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("browse_opds", {
        url: "https://a/search?q=shakespeare", catalogUrl: "https://a/opds",
      }),
    );
  });

  it("shows the cleartext warning in the retry panel for a non-loopback http catalog", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "http://192.168.0.50:8080/opds" }]);
      if (cmd === "browse_opds") return Promise.reject(AUTH_REQUIRED_ERROR);
      if (cmd === "get_opds_auth") return Promise.resolve(null);
      if (cmd === "set_opds_auth") return Promise.resolve(undefined);
      return Promise.resolve(null);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    await act(async () => fireEvent.click(screen.getByText("A")));

    expect(await screen.findByText("catalog.signInRequired")).toBeInTheDocument();
    expect(screen.getByText("catalog.insecureCredentialWarning")).toBeInTheDocument();

    await act(async () => {
      fireEvent.change(screen.getByPlaceholderText("catalog.authUsername"), { target: { value: "lan-user" } });
      fireEvent.change(screen.getByPlaceholderText("catalog.authPassword"), { target: { value: "lan-pass" } });
    });
    const signInBtn = screen.getByRole("button", { name: "catalog.signIn" });
    expect(signInBtn).toBeDisabled();

    await act(async () => fireEvent.click(screen.getByRole("checkbox")));
    expect(signInBtn).not.toBeDisabled();
    await act(async () => fireEvent.click(signInBtn));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "set_opds_auth",
        expect.objectContaining({ catalogUrl: "http://192.168.0.50:8080/opds", allowInsecure: true }),
      ),
    );
  });
});

describe("CatalogBrowser signed-in indicator and sign out", () => {
  it("marks catalogs that have stored credentials", async () => {
    invoke.mockImplementation((cmd: string, args?: { catalogUrl?: string }) => {
      if (cmd === "get_opds_catalogs")
        return Promise.resolve([
          { name: "A", url: "https://a/opds" },
          { name: "B", url: "https://b/opds" },
        ]);
      if (cmd === "get_opds_auth") {
        if (args?.catalogUrl === "https://a/opds") return Promise.resolve({ kind: "basic", username: "u" });
        return Promise.resolve(null);
      }
      return Promise.resolve(undefined);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByText("A")).toBeInTheDocument());
    expect(screen.getByText("B")).toBeInTheDocument();

    await waitFor(() => expect(screen.getByLabelText('catalog.signedInAs:{"name":"A"}')).toBeInTheDocument());
    expect(screen.queryByLabelText('catalog.signedInAs:{"name":"B"}')).not.toBeInTheDocument();
  });

  it("signs out and surfaces a keychain failure", async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "get_opds_catalogs") return Promise.resolve([{ name: "A", url: "https://a/opds" }]);
      if (cmd === "get_opds_auth") return Promise.resolve({ kind: "basic", username: "u" });
      if (cmd === "clear_opds_auth") return Promise.reject(new Error("keychain locked"));
      return Promise.resolve(undefined);
    });

    render(<CatalogBrowser onClose={() => {}} onBookImported={() => {}} />);
    await waitFor(() => expect(screen.getByLabelText('catalog.signedInAs:{"name":"A"}')).toBeInTheDocument());

    await act(async () => fireEvent.click(screen.getByLabelText('catalog.signOut:{"name":"A"}')));

    await waitFor(() => expect(screen.getByText(/catalog\.signOutFailed/)).toBeInTheDocument());
    // A failed sign-out must not silently clear the indicator.
    expect(screen.getByLabelText('catalog.signedInAs:{"name":"A"}')).toBeInTheDocument();
  });
});
