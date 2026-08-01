import { useState, useEffect, useCallback, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { friendlyError, isOpdsAuthError } from "../lib/errors";
import { pickSupportedOpdsLink, isValidHttpUrl, isLoopbackHost, formatBytes } from "../lib/utils";
import { FALLBACK_FORMATS, useSupportedFormats } from "../lib/supportedFormats";
import OpdsPresetPicker from "./OpdsPresetPicker";
import ConfirmDialog from "./ConfirmDialog";

interface OpdsCatalog {
  name: string;
  url: string;
  presetId?: string | null;
}

interface OpdsLink {
  href: string;
  mimeType: string;
  rel: string;
  sizeBytes?: number | null;
}

interface OpdsEntry {
  id: string;
  title: string;
  author: string;
  summary: string;
  coverUrl: string | null;
  links: OpdsLink[];
  navUrl: string | null;
  // Catalog this entry's provenance is scoped to. Backend-set; null once a
  // cross-origin hop drops it. Must be passed back verbatim when acting on
  // this entry (navUrl / download) — never re-derived or remembered.
  catalogUrl?: string | null;
}

interface OpdsFeed {
  title: string;
  entries: OpdsEntry[];
  nextUrl: string | null;
  searchUrl: string | null;
  // Same rule as OpdsEntry.catalogUrl, for the feed itself — pass back
  // verbatim when following nextUrl/searchUrl.
  catalogUrl?: string | null;
}

// Live per-catalog progress emitted by the backend during a unified search.
// Payload mirrors the `catalog-search-progress` event in commands.rs.
interface CatalogSearchProgress {
  query: string;
  url: string;
  name: string;
  count: number;
  ok: boolean;
}

// Per-catalog row in the unified-search checklist.
interface SearchProgressRow {
  url: string;
  name: string;
  status: "pending" | "done" | "failed";
  count: number;
}

// How long the finished checklist lingers before the view flips to results,
// so the last catalog's tick/count is actually readable. Only applied when a
// checklist was shown (i.e. at least one catalog).
const RESULTS_REVEAL_DELAY_MS = 2000;

interface CatalogBrowserProps {
  onClose: () => void;
  onBookImported: (bookId: string | null) => void;
}

export default function CatalogBrowser({ onClose, onBookImported }: CatalogBrowserProps) {
  const { t } = useTranslation();
  const [catalogs, setCatalogs] = useState<OpdsCatalog[]>([]);
  // Flipped true after the first successful `get_opds_catalogs`. Gates the
  // no-catalogs empty state so it never flashes during the initial load nor
  // appears after a failed load (where `catalogs` is still []). Stays false
  // on failure. In practice the backend always prepends DEFAULT_CATALOGS so
  // an empty list is rare — this is a safety net for builds where the
  // defaults are disabled/removed, keeping the empty state correct rather
  // than misleading even though it's seldom hit.
  const [catalogsLoaded, setCatalogsLoaded] = useState(false);
  // Which configured catalogs have a stored credential — drives the
  // signed-in indicator and the sign-out action. Keyed by catalog URL;
  // `null` means "checked, nothing stored" (as distinct from "not checked
  // yet", which is simply absent from the map).
  const [catalogAuth, setCatalogAuth] = useState<Record<string, { kind: "basic" | "bearer"; username: string } | null>>({});
  const [feed, setFeed] = useState<OpdsFeed | null>(null);
  const [loading, setLoading] = useState(false);
  // Non-null while a single-catalog search is in flight — names the catalog
  // ("Searching {name}…") instead of the generic "Loading…". Plain browsing
  // and pagination leave it null.
  const [feedLoadingLabel, setFeedLoadingLabel] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const lastActionRef = useRef<(() => void) | null>(null);
  // Sign-in panel shown on a 401/403 (`OPDS auth required`). Bound to the
  // catalogUrl of whatever request failed — never a remembered "current
  // catalog" — so credentials always land on the catalog that actually asked
  // for them. `secret` is always typed fresh: `get_opds_auth` never returns it.
  const [authPrompt, setAuthPrompt] = useState<{
    catalogUrl: string;
    kind: "basic" | "bearer";
    username: string;
    secret: string;
    allowInsecure: boolean;
    submitting: boolean;
    error: string | null;
  } | null>(null);
  // Guards the post-search reveal delay: don't touch state if the modal was
  // closed during the ~2 s hold.
  const mountedRef = useRef(true);
  useEffect(() => {
    // Set on (re)mount too — under StrictMode the effect runs mount → cleanup
    // → mount, and without re-setting true the ref would stay false and freeze
    // the post-search reveal.
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);
  const [history, setHistory] = useState<{ url: string; title: string; catalogUrl?: string | null }[]>([]);
  const [downloading, setDownloading] = useState<string | null>(null);
  const [downloadedIds, setDownloadedIds] = useState<Set<string>>(new Set());
  // Backend-supported formats for this build. `null` until the first fetch
  // resolves — fall back to the safe pre-MOBI core set so we don't briefly
  // offer `+ MOBI` buttons on `--no-default-features` builds during the
  // 50 ms–2 s capability-probe window (would 500 on click).
  const supportedFormats = useSupportedFormats();

  // Add catalog form
  const [showAddCatalog, setShowAddCatalog] = useState(false);
  const [showPresetPicker, setShowPresetPicker] = useState(false);
  const [newCatalogName, setNewCatalogName] = useState("");
  const [newCatalogUrl, setNewCatalogUrl] = useState("");
  const [addingCatalog, setAddingCatalog] = useState(false);
  const [addCatalogError, setAddCatalogError] = useState<string | null>(null);
  const [removeCatalogTarget, setRemoveCatalogTarget] = useState<{ name: string; url: string } | null>(null);
  // Optional sign-in for the catalog being added — disclosed on demand so the
  // common (public catalog) path stays a two-field form.
  const [showCredentials, setShowCredentials] = useState(false);
  const [authKind, setAuthKind] = useState<"basic" | "bearer">("basic");
  const [authUsername, setAuthUsername] = useState("");
  const [authSecret, setAuthSecret] = useState("");
  const [insecureAcknowledged, setInsecureAcknowledged] = useState(false);
  // True when a credential would be sent over cleartext HTTP to a non-loopback
  // host — mirrors the backend's `is_loopback_host` gate (opds.rs) so the
  // warning only appears where the backend would actually demand `allowInsecure`.
  const needsInsecureAck = (() => {
    if (!showCredentials) return false;
    const trimmed = newCatalogUrl.trim();
    if (!/^http:\/\//i.test(trimmed)) return false;
    try {
      return !isLoopbackHost(new URL(trimmed).hostname);
    } catch {
      return false;
    }
  })();

  // Search (per-catalog and unified)
  const [searchQuery, setSearchQuery] = useState("");
  const [unifiedQuery, setUnifiedQuery] = useState("");
  const [unifiedResults, setUnifiedResults] = useState<OpdsEntry[] | null>(null);
  const [unifiedLoading, setUnifiedLoading] = useState(false);
  // Live checklist of each catalog's search status. Null when no unified
  // search is running.
  const [searchProgress, setSearchProgress] = useState<SearchProgressRow[] | null>(null);

  const loadCatalogs = useCallback(async () => {
    try {
      const cs = await invoke<OpdsCatalog[]>("get_opds_catalogs");
      setCatalogs(cs);
      setCatalogsLoaded(true);
    } catch {
      // non-fatal — leave `catalogsLoaded` false so the empty state stays
      // hidden after a failed load (the error UI handles surfacing failures).
    }
  }, []);

  useEffect(() => { loadCatalogs(); }, [loadCatalogs]);

  // Refresh the signed-in indicator whenever the catalog list changes (initial
  // load, or after an add/remove). A per-catalog failure just leaves that one
  // catalog unmarked rather than failing the whole check.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const entries = await Promise.all(
        catalogs.map(async (cat) => {
          try {
            const auth = await invoke<{ kind: "basic" | "bearer"; username: string } | null>("get_opds_auth", { catalogUrl: cat.url });
            return [cat.url, auth] as const;
          } catch {
            return [cat.url, null] as const;
          }
        }),
      );
      if (!cancelled) setCatalogAuth(Object.fromEntries(entries));
    })();
    return () => { cancelled = true; };
  }, [catalogs]);

  const handleSignOut = async (catalogUrl: string) => {
    try {
      await invoke("clear_opds_auth", { catalogUrl });
      setCatalogAuth((prev) => ({ ...prev, [catalogUrl]: null }));
    } catch (err) {
      // Surface the failure rather than silently leaving the indicator on —
      // a rejected clear_opds_auth means the credential is still there.
      setError(t("catalog.signOutFailed", { error: friendlyError(err, t) }));
    }
  };

  // Opens the sign-in panel bound to `catalogUrl`, pre-filled from whatever
  // credential (kind + username only — never the secret) is already stored
  // for it. A fetch failure just leaves the panel blank; pre-fill is a
  // convenience, not a requirement to sign in.
  const openAuthPrompt = useCallback(async (catalogUrl: string) => {
    setAuthPrompt({ catalogUrl, kind: "basic", username: "", secret: "", allowInsecure: false, submitting: false, error: null });
    try {
      const existing = await invoke<{ kind: "basic" | "bearer"; username: string } | null>("get_opds_auth", { catalogUrl });
      if (existing) {
        setAuthPrompt((prev) => (prev && prev.catalogUrl === catalogUrl ? { ...prev, kind: existing.kind, username: existing.username } : prev));
      }
    } catch {
      // non-fatal — see comment above.
    }
  }, []);

  // True when the credential about to be sent for `authPrompt` would cross a
  // cleartext HTTP connection to a non-loopback host — same rule the backend
  // enforces (`is_loopback_host` in opds.rs) and the add-catalog form above.
  const authPromptNeedsInsecureAck = (() => {
    if (!authPrompt) return false;
    if (!/^http:\/\//i.test(authPrompt.catalogUrl)) return false;
    try {
      return !isLoopbackHost(new URL(authPrompt.catalogUrl).hostname);
    } catch {
      return false;
    }
  })();

  const submitAuthPrompt = async () => {
    if (!authPrompt || authPrompt.submitting) return;
    const username = authPrompt.kind === "basic" ? authPrompt.username.trim() : "";
    if (authPrompt.kind === "basic" && !username) return;
    if (!authPrompt.secret) return;
    if (authPromptNeedsInsecureAck && !authPrompt.allowInsecure) return;

    setAuthPrompt((prev) => (prev ? { ...prev, submitting: true, error: null } : prev));
    try {
      await invoke("set_opds_auth", {
        catalogUrl: authPrompt.catalogUrl,
        kind: authPrompt.kind,
        username,
        secret: authPrompt.secret,
        allowInsecure: authPrompt.allowInsecure,
      });
      setAuthPrompt(null);
      // Redo whatever request 401'd — same url and catalogUrl, now with a
      // credential the backend can attach.
      lastActionRef.current?.();
    } catch (err) {
      setAuthPrompt((prev) => (prev ? { ...prev, submitting: false, error: friendlyError(err, t) } : prev));
    }
  };

  // `catalogUrl` is the provenance to send with this request — the caller
  // must pass it explicitly (the configured catalog for a root browse,
  // entry.catalogUrl for a nav hop, feed.catalogUrl for pagination). Never
  // defaulted from remembered state: once the backend drops provenance on a
  // cross-origin hop, there is nothing here to re-assert it from.
  const browseTo = useCallback(async (url: string, title?: string, catalogUrl?: string | null) => {
    setLoading(true);
    setFeedLoadingLabel(null); // plain navigation → generic "Loading…"
    setError(null);
    lastActionRef.current = () => browseTo(url, title, catalogUrl);
    try {
      const f = await invoke<OpdsFeed>("browse_opds", { url, catalogUrl });
      setFeed(f);
      setHistory((prev) => [...prev, { url, title: title ?? f.title, catalogUrl: f.catalogUrl ?? null }]);
    } catch (err) {
      if (isOpdsAuthError(err) && catalogUrl) {
        openAuthPrompt(catalogUrl);
      } else {
        setError(friendlyError(err, t));
      }
    } finally {
      setLoading(false);
    }
  }, [t, openAuthPrompt]);

  const goBack = useCallback(() => {
    if (history.length <= 1) {
      setFeed(null);
      setHistory([]);
      return;
    }
    const newHistory = history.slice(0, -2);
    const prev = history[history.length - 2];
    setHistory(newHistory);
    browseTo(prev.url, prev.title, prev.catalogUrl);
  }, [history, browseTo]);

  const handleSearch = useCallback(async () => {
    if (!feed?.searchUrl || !searchQuery.trim()) return;
    const searchUrl = feed.searchUrl;
    // Provenance for a search_url follow comes from the feed that carried it,
    // never a remembered "current catalog".
    const catalogUrl = feed.catalogUrl;
    const url = searchUrl.replace("{searchTerms}", encodeURIComponent(searchQuery.trim()));
    setLoading(true);
    setFeedLoadingLabel(t("catalog.searchingServer", { name: feed.title || t("catalog.catalog") }));
    setError(null);
    lastActionRef.current = () => handleSearch();
    try {
      const f = await invoke<OpdsFeed>("browse_opds", { url, catalogUrl });
      // Preserve the parent's searchUrl so the search bar stays visible
      if (!f.searchUrl) f.searchUrl = searchUrl;
      setFeed(f);
      setHistory((prev) => [...prev, { url, title: `Search: ${searchQuery}`, catalogUrl: f.catalogUrl ?? null }]);
    } catch (err) {
      if (isOpdsAuthError(err) && catalogUrl) {
        openAuthPrompt(catalogUrl);
      } else {
        setError(friendlyError(err, t));
      }
    } finally {
      setLoading(false);
      setFeedLoadingLabel(null);
    }
  }, [feed, searchQuery, t, openAuthPrompt]);

  const handleDownload = useCallback(async (entry: OpdsEntry) => {
    // Walk the Carrel preference order (EPUB → PDF → CBZ → CBR → AZW3 → MOBI
    // → AZW) and pick the first matching link. If nothing matches, the UI
    // should already have hidden the button; bail out rather than pulling an
    // arbitrary non-importable link.
    const picked = pickSupportedOpdsLink(entry.links, supportedFormats ?? FALLBACK_FORMATS);
    if (!picked) return;

    setDownloading(entry.id);
    lastActionRef.current = () => handleDownload(entry);
    try {
      // Pass the MIME type so the backend can derive the file extension even
      // when the acquisition URL is opaque (e.g. `/download/123`). Provenance
      // is the entry's own catalogUrl — never re-derived.
      const result = await invoke<{ id: string; newly_imported: boolean }>("download_opds_book", {
        downloadUrl: picked.link.href,
        mimeType: picked.link.mimeType,
        catalogUrl: entry.catalogUrl,
      });
      setDownloadedIds((prev) => new Set(prev).add(entry.id));
      onBookImported(result.newly_imported ? result.id : null);
    } catch (err) {
      if (isOpdsAuthError(err) && entry.catalogUrl) {
        openAuthPrompt(entry.catalogUrl);
      } else {
        setError(t("catalog.downloadFailed", { title: entry.title, error: friendlyError(err, t) }));
      }
    } finally {
      setDownloading(null);
    }
  }, [onBookImported, t, supportedFormats, openAuthPrompt]);

  const resetAddForm = () => {
    setNewCatalogName("");
    setNewCatalogUrl("");
    setShowAddCatalog(false);
    setShowCredentials(false);
    setAuthKind("basic");
    setAuthUsername("");
    setAuthSecret("");
    setInsecureAcknowledged(false);
  };

  const handleCancelAddCatalog = () => {
    setShowAddCatalog(false);
    setAddCatalogError(null);
    // Never let a typed secret linger in state once the form is dismissed.
    setShowCredentials(false);
    setAuthUsername("");
    setAuthSecret("");
    setInsecureAcknowledged(false);
  };

  const handleAddCatalog = async () => {
    if (addingCatalog) return; // guard re-entry (Enter key / double-click while testing)
    const name = newCatalogName.trim();
    const url = newCatalogUrl.trim();
    if (!name || !url) return;

    // Validate the URL shape before hitting the network.
    if (!isValidHttpUrl(url)) {
      setAddCatalogError(t("catalog.invalidUrl"));
      return;
    }

    const username = authUsername.trim();
    const secret = authSecret;
    const hasCredentials = showCredentials && secret !== "" && (authKind === "bearer" || username !== "");
    if (hasCredentials && needsInsecureAck && !insecureAcknowledged) return; // guards the Enter-key path too

    setAddCatalogError(null);
    setAddingCatalog(true);
    try {
      if (hasCredentials) {
        // The backend connection-tests the feed with the new credential and
        // rolls back both the catalog row and the keychain entry atomically
        // on failure — no separate browse_opds call here.
        await invoke("add_opds_catalog_with_auth", {
          name,
          url,
          presetId: null,
          kind: authKind,
          username: authKind === "basic" ? username : "",
          secret,
          allowInsecure: insecureAcknowledged,
        });
      } else {
        // Save first, then connection-test. `browse_opds` only relaxes its
        // private/loopback SSRF guard for hosts already in the saved catalog
        // list, so a LAN/localhost feed must be saved before it can be tested.
        // `add_opds_catalog` intentionally trusts the user-entered URL.
        await invoke("add_opds_catalog", { name, url });
        try {
          // Provisional connection test is a root browse of the catalog being
          // added, so its provenance is its own (about-to-be-configured) URL.
          await invoke("browse_opds", { url, catalogUrl: url });
        } catch (testErr) {
          // Roll back the provisional add so a broken feed isn't kept.
          await invoke("remove_opds_catalog", { url }).catch(() => {});
          throw testErr;
        }
      }
      resetAddForm();
      await loadCatalogs();
    } catch (err) {
      setAddCatalogError(t("catalog.connectionTestFailed", { error: friendlyError(err, t) }));
    } finally {
      setAddingCatalog(false);
    }
  };

  const handleRemoveCatalog = async (url: string) => {
    try {
      await invoke("remove_opds_catalog", { url });
      await loadCatalogs();
    } catch (err) {
      setError(friendlyError(err, t));
    }
  };

  const handleUnifiedSearch = useCallback(async () => {
    const q = unifiedQuery.trim();
    if (!q) return;
    setUnifiedLoading(true);
    setError(null);
    // Seed the checklist from the known catalog list; the backend emits one
    // `catalog-search-progress` event per catalog (matched by url) as it
    // finishes. `query` on the payload guards against stale events from a
    // previous, still-draining search.
    setSearchProgress(catalogs.map((c) => ({ url: c.url, name: c.name, status: "pending", count: 0 })));
    const unlisten = await listen<CatalogSearchProgress>("catalog-search-progress", (event) => {
      if (event.payload.query !== q) return;
      setSearchProgress((prev) =>
        prev
          ? prev.map((row) =>
              row.url === event.payload.url
                ? { ...row, status: event.payload.ok ? "done" : "failed", count: event.payload.count }
                : row,
            )
          : prev,
      );
    });
    try {
      const results = await invoke<OpdsEntry[]>("search_all_catalogs", { query: q });
      // The last catalog's progress event and this result land almost
      // together, so hold the finished checklist on screen briefly before
      // flipping to results — otherwise the final tick is never seen.
      if (catalogs.length > 0) {
        await new Promise((resolve) => setTimeout(resolve, RESULTS_REVEAL_DELAY_MS));
      }
      if (!mountedRef.current) return;
      setUnifiedResults(results);
    } catch (err) {
      if (mountedRef.current) setError(friendlyError(err, t));
    } finally {
      unlisten();
      if (mountedRef.current) {
        setUnifiedLoading(false);
        setSearchProgress(null);
      }
    }
  }, [unifiedQuery, catalogs, t]);

  const clearUnifiedSearch = useCallback(() => {
    setUnifiedResults(null);
    setUnifiedQuery("");
  }, []);

  // Sign-in panel — rendered above whichever view (catalog list or feed) is
  // on screen when a request 401s/403s. Higher stacking context than the
  // main modal so it sits on top of it.
  const authPromptPanel = authPrompt && (
    <>
      <div className="fixed inset-0 bg-ink/40 z-[60]" onClick={() => setAuthPrompt(null)} />
      <div className="fixed inset-0 z-[60] flex items-center justify-center p-4 pointer-events-none">
        <div className="bg-surface rounded-2xl shadow-xl border border-warm-border w-full max-w-sm pointer-events-auto animate-fade-in p-5 space-y-3">
          <h3 className="font-serif text-sm font-semibold text-ink">{t("catalog.signInRequired")}</h3>
          <div className="flex gap-3 text-xs text-ink">
            <label className="flex items-center gap-1.5">
              <input
                type="radio" name="opds-retry-auth-kind" checked={authPrompt.kind === "basic"}
                onChange={() => setAuthPrompt((p) => (p ? { ...p, kind: "basic" } : p))}
              />
              {t("catalog.authBasic")}
            </label>
            <label className="flex items-center gap-1.5">
              <input
                type="radio" name="opds-retry-auth-kind" checked={authPrompt.kind === "bearer"}
                onChange={() => setAuthPrompt((p) => (p ? { ...p, kind: "bearer" } : p))}
              />
              {t("catalog.authBearer")}
            </label>
          </div>
          {authPrompt.kind === "basic" && (
            <input
              type="text" value={authPrompt.username}
              onChange={(e) => setAuthPrompt((p) => (p ? { ...p, username: e.target.value } : p))}
              placeholder={t("catalog.authUsername")} autoComplete="off"
              className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
            />
          )}
          <input
            type="password" value={authPrompt.secret}
            onChange={(e) => setAuthPrompt((p) => (p ? { ...p, secret: e.target.value } : p))}
            placeholder={authPrompt.kind === "basic" ? t("catalog.authPassword") : t("catalog.authToken")}
            autoComplete="off"
            className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
          />
          {authPromptNeedsInsecureAck && (
            <label className="flex items-start gap-1.5 text-xs text-amber-700">
              <input
                type="checkbox" checked={authPrompt.allowInsecure}
                onChange={(e) => setAuthPrompt((p) => (p ? { ...p, allowInsecure: e.target.checked } : p))}
                className="mt-0.5"
              />
              {t("catalog.insecureCredentialWarning")}
            </label>
          )}
          {authPrompt.error && <p className="text-xs text-red-600">{authPrompt.error}</p>}
          <div className="flex gap-2">
            <button
              onClick={submitAuthPrompt}
              disabled={
                authPrompt.submitting ||
                !authPrompt.secret ||
                (authPrompt.kind === "basic" && !authPrompt.username.trim()) ||
                (authPromptNeedsInsecureAck && !authPrompt.allowInsecure)
              }
              className="flex-1 py-1.5 text-xs font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors disabled:opacity-40"
            >
              {authPrompt.submitting ? t("catalog.testingConnection") : t("catalog.signIn")}
            </button>
            <button
              onClick={() => setAuthPrompt(null)} disabled={authPrompt.submitting}
              className="flex-1 py-1.5 text-xs text-ink-muted hover:text-ink transition-colors disabled:opacity-40"
            >
              {t("common.cancel")}
            </button>
          </div>
        </div>
      </div>
    </>
  );

  // Catalog list view
  if (!feed) {
    return (
      <>
        <div className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-50 animate-fade-in" onClick={onClose} />
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 pointer-events-none">
          <div className="bg-surface rounded-2xl shadow-xl border border-warm-border w-full max-w-lg pointer-events-auto animate-fade-in max-h-[80vh] flex flex-col">
            <div className="px-5 py-4 border-b border-warm-border flex items-center justify-between shrink-0">
              <h2 className="font-serif text-base font-semibold text-ink">{t("catalog.title")}</h2>
              <button onClick={onClose} className="p-1 text-ink-muted hover:text-ink transition-colors rounded" aria-label={t("common.close")}>
                <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
                  <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
            </div>

            {/* Unified search bar */}
            <div className="px-5 py-3 border-b border-warm-border flex gap-2">
              <input
                type="text" value={unifiedQuery} onChange={(e) => setUnifiedQuery(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") handleUnifiedSearch(); if (e.key === "Escape" && unifiedResults) clearUnifiedSearch(); }}
                placeholder={t("catalog.searchAllPlaceholder")}
                className="flex-1 text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-1.5 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
              />
              {unifiedResults ? (
                <button onClick={clearUnifiedSearch}
                  className="px-3 py-1.5 text-sm text-ink-muted hover:text-ink rounded-lg transition-colors">
                  {t("common.clear")}
                </button>
              ) : (
                <button onClick={handleUnifiedSearch} disabled={!unifiedQuery.trim() || unifiedLoading}
                  className="px-3 py-1.5 text-sm font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors disabled:opacity-40">
                  {t("common.search")}
                </button>
              )}
            </div>

            <div className="px-5 py-2 border-b border-warm-border flex gap-2 shrink-0">
              <button
                type="button"
                onClick={() => {
                  setShowPresetPicker(true);
                  setShowAddCatalog(false);
                }}
                className="flex-1 text-xs font-medium text-accent hover:bg-accent-light/50 rounded-lg px-3 py-1.5 transition-colors"
              >
                {t("catalog.presets.browseButton")}
              </button>
              <button
                type="button"
                onClick={() => {
                  setShowAddCatalog((v) => !v);
                  setShowPresetPicker(false);
                }}
                className="flex-1 text-xs font-medium text-accent hover:bg-accent-light/50 rounded-lg px-3 py-1.5 transition-colors"
              >
                {t("catalog.addCustomCatalog")}
              </button>
            </div>

            <div className="flex-1 overflow-y-auto py-2 relative">
              {/* Loading overlay when browsing to a catalog */}
              {loading && !feed && (
                <div className="absolute inset-0 flex items-center justify-center bg-surface/80 z-10">
                  <div className="flex items-center gap-2">
                    <div className="w-4 h-4 border-2 border-accent/30 border-t-accent rounded-full animate-spin" />
                    <span className="text-sm text-ink-muted">{t("common.loading")}</span>
                  </div>
                </div>
              )}
              {/* Preset picker / Unified search results / Catalog list */}
              {showPresetPicker && !unifiedLoading && !unifiedResults ? (
                <OpdsPresetPicker
                  currentCatalogs={catalogs}
                  onClose={() => setShowPresetPicker(false)}
                  onAdded={async () => {
                    await loadCatalogs();
                  }}
                />
              ) : unifiedLoading ? (
                <div className="px-5 py-6">
                  <p className="text-sm text-ink-muted mb-3 text-center">{t("catalog.searchingAll")}</p>
                  <div className="space-y-1.5 max-w-xs mx-auto">
                    {(searchProgress ?? []).map((row) => (
                      <div key={row.url} className="flex items-center gap-2 text-sm">
                        {row.status === "pending" ? (
                          <div className="w-3.5 h-3.5 border-2 border-accent/30 border-t-accent rounded-full animate-spin shrink-0" />
                        ) : row.status === "failed" ? (
                          <svg width="14" height="14" viewBox="0 0 20 20" fill="none" className="text-red-500 shrink-0">
                            <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                          </svg>
                        ) : (
                          <svg width="14" height="14" viewBox="0 0 20 20" fill="none" className="text-accent shrink-0">
                            <path d="M4 10l4 4 8-8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                          </svg>
                        )}
                        <span className="text-ink truncate flex-1">{row.name}</span>
                        {row.status === "done" && (
                          <span className="text-xs text-ink-muted shrink-0">
                            {t("catalog.searchCatalogCount", { count: row.count })}
                          </span>
                        )}
                        {row.status === "failed" && (
                          <span className="text-xs text-red-500 shrink-0">{t("catalog.searchCatalogFailed")}</span>
                        )}
                      </div>
                    ))}
                  </div>
                </div>
              ) : unifiedResults ? (
                unifiedResults.length === 0 ? (
                  <div className="flex items-center justify-center py-12">
                    <p className="text-sm text-ink-muted">{t("common.noResults")}</p>
                  </div>
                ) : (
                  unifiedResults.map((entry) => {
                    const picked = pickSupportedOpdsLink(entry.links, supportedFormats ?? FALLBACK_FORMATS);
                    const hasDownloads = picked !== null;
                    const isDownloaded = downloadedIds.has(entry.id);
                    const isDownloading = downloading === entry.id;

                    return (
                      <div key={entry.id} className="flex items-start gap-3 px-5 py-3 border-b border-warm-border/50 transition-colors">
                        {entry.coverUrl ? (
                          <img src={entry.coverUrl} alt="" className="w-12 h-16 object-cover rounded shrink-0 bg-warm-subtle"
                            onError={(e) => { (e.target as HTMLImageElement).style.display = "none"; }} />
                        ) : (
                          <div className="w-12 h-16 rounded bg-warm-subtle shrink-0 flex items-center justify-center">
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" className="text-ink-muted/40">
                              <path d="M4 19.5v-15A2.5 2.5 0 016.5 2H20v20H6.5a2.5 2.5 0 010-5H20" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                            </svg>
                          </div>
                        )}
                        <div className="flex-1 min-w-0">
                          <p className="text-sm font-medium text-ink leading-snug">{entry.title}</p>
                          {entry.author && <p className="text-xs text-ink-muted mt-0.5">{entry.author}</p>}
                          {entry.summary && <p className="text-xs text-ink-muted mt-1 line-clamp-2 leading-relaxed">{entry.summary}</p>}
                          {hasDownloads && (
                            <div className="flex items-center gap-2 mt-2">
                              {isDownloaded ? (
                                <span className="text-[11px] text-accent font-medium">{t("catalog.addedToLibrary")}</span>
                              ) : isDownloading ? (
                                <span className="text-[11px] text-accent font-medium flex items-center gap-1.5">
                                  <svg className="animate-spin w-3 h-3" viewBox="0 0 24 24" fill="none">
                                    <circle cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="3" className="opacity-25" />
                                    <path d="M4 12a8 8 0 018-8" stroke="currentColor" strokeWidth="3" strokeLinecap="round" className="opacity-75" />
                                  </svg>
                                  {picked?.label
                                    ? t("catalog.downloadingFormat", { format: picked.label })
                                    : t("common.downloading")}
                                </span>
                              ) : (
                                <button
                                  onClick={() => handleDownload(entry)}
                                  className="px-2 py-0.5 text-[11px] font-medium text-accent bg-accent-light hover:bg-accent hover:text-white rounded transition-colors"
                                >
                                  + {picked?.label ?? ""}
                                </button>
                              )}
                              {picked?.link.sizeBytes != null && !isDownloaded && (
                                <span className="text-[11px] text-ink-muted">{formatBytes(picked.link.sizeBytes)}</span>
                              )}
                            </div>
                          )}
                        </div>
                      </div>
                    );
                  })
                )
              ) : (
              /* Catalog list (hidden during unified search) */
              <>
              {catalogsLoaded && catalogs.length === 0 && !showAddCatalog && (
                <div className="flex flex-col items-center justify-center text-center px-8 py-12 gap-3">
                  <div className="w-12 h-12 rounded-2xl bg-accent-light flex items-center justify-center">
                    <svg width="24" height="24" viewBox="0 0 24 24" fill="none" className="text-accent">
                      <path d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                    </svg>
                  </div>
                  <h3 className="font-serif text-base font-semibold text-ink">{t("catalog.empty.title")}</h3>
                  <p className="text-sm text-ink-muted leading-relaxed max-w-xs">{t("catalog.empty.subtitle")}</p>
                  <button
                    type="button"
                    onClick={() => {
                      setShowPresetPicker(true);
                      setShowAddCatalog(false);
                    }}
                    className="mt-1 px-4 py-2 text-sm font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors"
                  >
                    {t("catalog.empty.browsePresets")}
                  </button>
                </div>
              )}
              {catalogs.map((cat) => (
                // Row + remove are sibling buttons (not nested) — a button
                // inside a button is invalid HTML and breaks click handling.
                <div key={cat.url} className="relative flex items-center group">
                  <button
                    onClick={() => browseTo(cat.url, cat.name, cat.url)}
                    className="w-full flex items-center gap-3 px-5 py-3 text-left hover:bg-warm-subtle transition-colors"
                  >
                    <div className="w-8 h-8 rounded-lg bg-accent-light flex items-center justify-center shrink-0">
                      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" className="text-accent">
                        <path d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    </div>
                    <div className="flex-1 min-w-0">
                      <p className="text-sm font-medium text-ink flex items-center gap-1.5">
                        <span className="truncate">{cat.name}</span>
                        {catalogAuth[cat.url] && (
                          <svg
                            width="11" height="11" viewBox="0 0 24 24" fill="none" className="text-accent shrink-0"
                            role="img" aria-label={t("catalog.signedInAs", { name: cat.name })}
                          >
                            <title>{t("catalog.signedInAs", { name: cat.name })}</title>
                            <path d="M15.75 5.25a3 3 0 013 3m3 0a6 6 0 11-12 0 6 6 0 0112 0zM3 21l6.75-6.75" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                          </svg>
                        )}
                      </p>
                      <p className="text-[11px] text-ink-muted truncate">{cat.url}</p>
                    </div>
                  </button>
                  <div className="absolute right-3 flex items-center gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100">
                    {catalogAuth[cat.url] && (
                      <button
                        onClick={() => handleSignOut(cat.url)}
                        className="p-1 text-ink-muted hover:text-ink transition-colors bg-surface rounded"
                        aria-label={t("catalog.signOut", { name: cat.name })}
                        title={t("catalog.signOutTitle")}
                      >
                        <svg width="12" height="12" viewBox="0 0 24 24" fill="none">
                          <path d="M9 21H5a2 2 0 01-2-2V5a2 2 0 012-2h4M16 17l5-5-5-5M21 12H9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                        </svg>
                      </button>
                    )}
                    <button
                      onClick={() => setRemoveCatalogTarget({ name: cat.name, url: cat.url })}
                      className="p-1 text-ink-muted hover:text-red-500 transition-colors bg-surface rounded"
                      aria-label={t("catalog.removeCatalog", { name: cat.name })}
                      title={t("catalog.removeCatalogTitle")}
                    >
                      <svg width="12" height="12" viewBox="0 0 20 20" fill="none">
                        <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                      </svg>
                    </button>
                  </div>
                </div>
              ))}

              {/* Add custom catalog form */}
              {showAddCatalog && (
                <div className="px-5 py-3 space-y-2 border-t border-warm-border">
                  <input
                    type="text" value={newCatalogName} onChange={(e) => setNewCatalogName(e.target.value)}
                    placeholder={t("catalog.catalogName")} autoFocus
                    className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                  />
                  <input
                    type="url" value={newCatalogUrl}
                    onChange={(e) => { setNewCatalogUrl(e.target.value); setAddCatalogError(null); setInsecureAcknowledged(false); }}
                    placeholder={t("catalog.opdsFeedUrl")}
                    onKeyDown={(e) => { if (e.key === "Enter") handleAddCatalog(); }}
                    className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                  />

                  <button
                    type="button"
                    onClick={() => setShowCredentials((v) => !v)}
                    className="text-xs font-medium text-accent hover:text-accent/80 transition-colors"
                  >
                    {showCredentials ? t("catalog.hideSignIn") : t("catalog.addSignIn")}
                  </button>

                  {showCredentials && (
                    <div className="space-y-2 pl-3 border-l-2 border-warm-border">
                      <div className="flex gap-3 text-xs text-ink">
                        <label className="flex items-center gap-1.5">
                          <input type="radio" name="opds-auth-kind" checked={authKind === "basic"} onChange={() => setAuthKind("basic")} />
                          {t("catalog.authBasic")}
                        </label>
                        <label className="flex items-center gap-1.5">
                          <input type="radio" name="opds-auth-kind" checked={authKind === "bearer"} onChange={() => setAuthKind("bearer")} />
                          {t("catalog.authBearer")}
                        </label>
                      </div>
                      {authKind === "basic" && (
                        <input
                          type="text" value={authUsername} onChange={(e) => setAuthUsername(e.target.value)}
                          placeholder={t("catalog.authUsername")} autoComplete="off"
                          className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                        />
                      )}
                      <input
                        type="password" value={authSecret} onChange={(e) => setAuthSecret(e.target.value)}
                        placeholder={authKind === "basic" ? t("catalog.authPassword") : t("catalog.authToken")}
                        autoComplete="off"
                        className="w-full text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                      />
                      {needsInsecureAck && (
                        <label className="flex items-start gap-1.5 text-xs text-amber-700">
                          <input
                            type="checkbox" checked={insecureAcknowledged}
                            onChange={(e) => setInsecureAcknowledged(e.target.checked)}
                            className="mt-0.5"
                          />
                          {t("catalog.insecureCredentialWarning")}
                        </label>
                      )}
                    </div>
                  )}

                  {addCatalogError && (
                    <p className="text-xs text-red-600">{addCatalogError}</p>
                  )}
                  <div className="flex gap-2">
                    <button
                      onClick={handleAddCatalog}
                      disabled={
                        !newCatalogName.trim() ||
                        !newCatalogUrl.trim() ||
                        addingCatalog ||
                        (needsInsecureAck && !insecureAcknowledged)
                      }
                      className="flex-1 py-1.5 text-xs font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors disabled:opacity-40">
                      {addingCatalog ? t("catalog.testingConnection") : t("common.add")}
                    </button>
                    <button onClick={handleCancelAddCatalog} disabled={addingCatalog}
                      className="flex-1 py-1.5 text-xs text-ink-muted hover:text-ink transition-colors disabled:opacity-40">
                      {t("common.cancel")}
                    </button>
                  </div>
                </div>
              )}
              </>
              )}
            </div>

            {error && (
              <div className="px-5 py-2 border-t border-warm-border flex items-center gap-2">
                <p className="text-xs text-red-600 flex-1">{error}</p>
                {lastActionRef.current && (
                  <button
                    onClick={() => lastActionRef.current?.()}
                    className="text-xs text-accent hover:text-accent/80 font-medium shrink-0"
                  >
                    {t("common.retry")}
                  </button>
                )}
              </div>
            )}
          </div>
        </div>

        {removeCatalogTarget && (
          <ConfirmDialog
            title={t("catalog.removeCatalogConfirmTitle", { name: removeCatalogTarget.name })}
            message={t("catalog.removeCatalogConfirmMessage")}
            confirmLabel={t("common.remove")}
            onCancel={() => setRemoveCatalogTarget(null)}
            onConfirm={() => {
              const url = removeCatalogTarget.url;
              setRemoveCatalogTarget(null);
              void handleRemoveCatalog(url);
            }}
          />
        )}
        {authPromptPanel}
      </>
    );
  }

  // Feed browsing view
  return (
    <>
      <div className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-50 animate-fade-in" onClick={onClose} />
      <div className="fixed inset-0 z-50 flex items-center justify-center p-4 pointer-events-none">
        <div className="bg-surface rounded-2xl shadow-xl border border-warm-border w-full max-w-2xl pointer-events-auto animate-fade-in max-h-[85vh] flex flex-col">
          {/* Header */}
          <div className="px-5 py-3 border-b border-warm-border flex items-center gap-3 shrink-0">
            <button onClick={goBack} className="p-1 text-ink-muted hover:text-ink transition-colors rounded" aria-label={t("common.back")}>
              <svg width="16" height="16" viewBox="0 0 20 20" fill="none">
                <path d="M12 4l-6 6 6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
            <h2 className="font-serif text-sm font-semibold text-ink truncate flex-1">{feed.title || t("catalog.catalog")}</h2>
            <button onClick={onClose} className="p-1 text-ink-muted hover:text-ink transition-colors rounded" aria-label={t("common.close")}>
              <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
                <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
              </svg>
            </button>
          </div>

          {/* Search bar (if feed has search) */}
          {feed.searchUrl && (
            <div className="px-5 py-2 border-b border-warm-border flex gap-2">
              <input
                type="text" value={searchQuery} onChange={(e) => setSearchQuery(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") handleSearch(); }}
                placeholder={t("catalog.searchThisCatalog")}
                className="flex-1 text-sm bg-warm-subtle border border-warm-border rounded-lg px-3 py-1.5 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
              />
              <button onClick={handleSearch} disabled={!searchQuery.trim()}
                className="px-3 py-1.5 text-sm font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors disabled:opacity-40">
                {t("common.search")}
              </button>
            </div>
          )}

          {/* Entries */}
          <div className="flex-1 overflow-y-auto">
            {loading ? (
              <div className="flex items-center justify-center py-12">
                <p className="text-sm text-ink-muted">{feedLoadingLabel ?? t("common.loading")}</p>
              </div>
            ) : feed.entries.length === 0 ? (
              <div className="flex items-center justify-center py-12">
                <p className="text-sm text-ink-muted">{t("catalog.noEntries")}</p>
              </div>
            ) : (
              feed.entries.map((entry) => {
                const picked = pickSupportedOpdsLink(entry.links, supportedFormats ?? FALLBACK_FORMATS);
                const hasDownloads = picked !== null;
                const isNav = !!entry.navUrl && !hasDownloads;
                const isDownloaded = downloadedIds.has(entry.id);
                const isDownloading = downloading === entry.id;

                return (
                  <div
                    key={entry.id}
                    className={`flex items-start gap-3 px-5 py-3 border-b border-warm-border/50 ${isNav ? "hover:bg-warm-subtle cursor-pointer" : ""} transition-colors`}
                    onClick={isNav ? () => browseTo(entry.navUrl!, entry.title, entry.catalogUrl) : undefined}
                  >
                    {/* Cover thumbnail — only for book entries, not nav */}
                    {!isNav && entry.coverUrl ? (
                      <img
                        src={entry.coverUrl}
                        alt=""
                        className="w-12 h-16 object-cover rounded shrink-0 bg-warm-subtle"
                        onError={(e) => { (e.target as HTMLImageElement).style.display = "none"; }}
                      />
                    ) : !isNav ? (
                      <div className="w-12 h-16 rounded bg-warm-subtle shrink-0 flex items-center justify-center">
                        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" className="text-ink-muted/40">
                          <path d="M4 19.5v-15A2.5 2.5 0 016.5 2H20v20H6.5a2.5 2.5 0 010-5H20" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                        </svg>
                      </div>
                    ) : null}

                    <div className="flex-1 min-w-0">
                      <p className="text-sm font-medium text-ink leading-snug">{entry.title}</p>
                      {entry.author && <p className="text-xs text-ink-muted mt-0.5">{entry.author}</p>}
                      {entry.summary && (
                        <p className="text-xs text-ink-muted mt-1 line-clamp-2 leading-relaxed">{entry.summary}</p>
                      )}

                      {/* Download buttons */}
                      {hasDownloads && (
                        <div className="flex items-center gap-2 mt-2">
                          {isDownloaded ? (
                            <span className="text-[11px] text-accent font-medium">{t("catalog.addedToLibrary")}</span>
                          ) : isDownloading ? (
                            <span className="text-[11px] text-accent font-medium flex items-center gap-1.5">
                              <svg className="animate-spin w-3 h-3" viewBox="0 0 24 24" fill="none">
                                <circle cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="3" className="opacity-25" />
                                <path d="M4 12a8 8 0 018-8" stroke="currentColor" strokeWidth="3" strokeLinecap="round" className="opacity-75" />
                              </svg>
                              {picked?.label
                                ? t("catalog.downloadingFormat", { format: picked.label })
                                : t("common.downloading")}
                            </span>
                          ) : (
                            picked && (
                              <button
                                onClick={(e) => { e.stopPropagation(); handleDownload(entry); }}
                                className="px-2 py-0.5 text-[11px] font-medium text-accent bg-accent-light hover:bg-accent hover:text-white rounded transition-colors"
                              >
                                + {picked.label}
                              </button>
                            )
                          )}
                          {picked?.link.sizeBytes != null && !isDownloaded && (
                            <span className="text-[11px] text-ink-muted">{formatBytes(picked.link.sizeBytes)}</span>
                          )}
                        </div>
                      )}
                    </div>

                    {/* Nav arrow for sub-catalogs */}
                    {isNav && (
                      <svg width="14" height="14" viewBox="0 0 20 20" fill="none" className="text-ink-muted shrink-0 mt-2">
                        <path d="M8 4l6 6-6 6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    )}
                  </div>
                );
              })
            )}

            {/* Next page */}
            {feed.nextUrl && !loading && (
              <button
                onClick={() => browseTo(feed.nextUrl!, undefined, feed.catalogUrl)}
                className="w-full py-3 text-sm text-accent hover:bg-warm-subtle transition-colors"
              >
                {t("catalog.loadMore")}
              </button>
            )}
          </div>

          {error && (
            <div className="px-5 py-2 border-t border-warm-border">
              <p className="text-xs text-red-600">{error}</p>
            </div>
          )}
        </div>
      </div>
      {authPromptPanel}
    </>
  );
}
