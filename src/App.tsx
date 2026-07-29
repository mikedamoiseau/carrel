import { useState, useCallback, lazy, Suspense, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { BrowserRouter, Routes, Route, Link, useLocation, useNavigate } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { ThemeProvider } from "./context/ThemeContext";
import { ImportProvider } from "./context/ImportContext";
import { OnboardingProvider } from "./context/OnboardingContext";
import { ToastProvider, useToast } from "./components/Toast";
import SettingsPanel from "./components/SettingsPanel";
import ReadingStats from "./components/ReadingStats";
import VocabularyPanel from "./components/VocabularyPanel";
import ProfileSwitcher from "./components/ProfileSwitcher";
import ProfileUnlockDialog from "./components/ProfileUnlockDialog";
import CatalogBrowser from "./components/CatalogBrowser";
import LanguageSwitcher from "./components/LanguageSwitcher";
import PrivateModeToggle from "./components/PrivateModeToggle";
import PrivateModeBar from "./components/PrivateModeBar";
import ImportStatusBar from "./components/ImportStatusBar";
import AnalyticsConsentDialog from "./components/AnalyticsConsentDialog";
import UpdateCheckHost from "./components/UpdateCheckHost";
import Library from "./screens/Library";
import ReaderSkeleton from "./components/ReaderSkeleton";
import ReaderErrorBoundary from "./components/ReaderErrorBoundary";

interface ProfileSummary {
  name: string;
  is_active: boolean;
}

interface ProfileLockStatus {
  locked: boolean;
  unlockedThisSession: boolean;
}

/**
 * Startup soft-lock gate (A-M3, spec Decision 6 / SB-7): the initially
 * active profile is entered without a `switch_profile` call, so it's never
 * offered the unlock prompt that call's `LockRequired` error normally
 * triggers. This checks it explicitly on mount, before the library ever
 * renders.
 *
 * This check is a UX nicety, not the security boundary — the real gate is
 * `AppState::active_db()` on the backend, which every data-bearing command
 * (including `get_library`) already enforces via `is_unlocked`. If this
 * check itself fails (e.g. a transient IPC error), we fail open *here* and
 * let the library attempt to render — a still-locked profile is still
 * blocked at the data layer, just without the polished prompt.
 */
function useStartupLockGate(): {
  checked: boolean;
  lockedProfile: string | null;
  clearLockedProfile: () => void;
} {
  const [checked, setChecked] = useState(false);
  const [lockedProfile, setLockedProfile] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const profiles = await invoke<ProfileSummary[]>("get_profiles");
        const active = profiles.find((p) => p.is_active)?.name ?? "default";
        const status = await invoke<ProfileLockStatus>("profile_lock_status", { profile: active });
        if (cancelled) return;
        setLockedProfile(status.locked && !status.unlockedThisSession ? active : null);
      } catch {
        if (!cancelled) setLockedProfile(null);
      } finally {
        if (!cancelled) setChecked(true);
      }
    })();
    return () => { cancelled = true; };
  }, []);

  return { checked, lockedProfile, clearLockedProfile: () => setLockedProfile(null) };
}

const Reader = lazy(() => import("./screens/Reader"));

function AppShell() {
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [statsOpen, setStatsOpen] = useState(false);
  const [vocabularyOpen, setVocabularyOpen] = useState(false);
  const [vocabState, setVocabState] = useState({ enabled: false, count: 0 });
  const [profileKey, setProfileKey] = useState(0);
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [catalogImportedBookIds, setCatalogImportedBookIds] = useState<string[]>([]);
  const location = useLocation();
  const navigate = useNavigate();
  const inReader = location.pathname.startsWith("/reader/");
  const { addToast } = useToast();
  const { t } = useTranslation();
  const authErrorShown = useRef(false);
  const { checked: lockGateChecked, lockedProfile, clearLockedProfile } = useStartupLockGate();

  useEffect(() => {
    const unlisten = listen<{ message: string }>("backup-auth-error", () => {
      if (authErrorShown.current) return;
      authErrorShown.current = true;
      addToast(t("toast.backupAuthFailed"), "error");
    });
    return () => { unlisten.then((fn) => fn()); };
  }, [addToast, t]);

  const handleProfileSwitch = useCallback(() => {
    navigate("/");
    setCatalogImportedBookIds([]);
    setProfileKey((k) => k + 1); // force Library remount
  }, [navigate]);

  // The active profile is shared with the web UI, which can switch it from a
  // phone — so the same reset has to run when the change came from elsewhere,
  // or the desktop keeps showing the previous profile's library.
  useEffect(() => {
    const unlisten = listen<string>("profile-changed", () => { handleProfileSwitch(); });
    return () => { unlisten.then((fn) => fn()); };
  }, [handleProfileSwitch]);

  // Vocabulary nav button visibility (F-1-5): shown when the setting is on
  // OR the list is non-empty — so words saved before the user disables the
  // toggle stay reachable. Re-checked on mount, whenever the Settings or
  // Vocabulary overlay closes, and on profile-identity changes (`profileKey`
  // bumps on switch, `lockedProfile` clears on unlock) — otherwise the
  // button can reflect a previous or still-locked profile's state.
  const loadVocabState = useCallback(async () => {
    try {
      const [enabledVal, words] = await Promise.all([
        invoke<string | null>("get_setting_value", { key: "vocabulary_enabled" }),
        invoke<unknown[]>("list_vocabulary"),
      ]);
      setVocabState({ enabled: enabledVal === "true", count: words.length });
    } catch {
      // ignore — nav button just keeps its last known state until next refresh
    }
  }, []);

  useEffect(() => {
    loadVocabState();
  }, [settingsOpen, vocabularyOpen, profileKey, lockedProfile, loadVocabState]);

  // Soft-lock startup gate (A-M3): don't render the library — or even the
  // brief unstyled flash of it — until we know whether the active profile
  // needs unlocking. No `onCancel`: there's no other profile to fall back
  // to at boot (see `useStartupLockGate`'s doc comment).
  if (!lockGateChecked) {
    return <div className="h-screen bg-paper" />;
  }
  if (lockedProfile) {
    return (
      <div className="h-screen bg-paper">
        <ProfileUnlockDialog profile={lockedProfile} onUnlocked={clearLockedProfile} />
      </div>
    );
  }

  return (
    <div className="flex flex-col h-screen bg-paper text-ink">
      {/* App-wide private-mode strip — always on top, survives into the reader */}
      <PrivateModeBar />
      {/* Top nav — minimal wordmark header, hidden in reader (reader has its own header) */}
      {!inReader && (
        <nav className="shrink-0 h-12 px-6 flex items-center border-b border-warm-border bg-surface">
          <Link
            to="/"
            className="font-serif text-xl font-semibold tracking-tight text-ink hover:text-accent transition-colors duration-150"
          >
            Folio
          </Link>
          <ProfileSwitcher onSwitch={handleProfileSwitch} />
          <div className="flex-1" />
          <button
            onClick={() => setStatsOpen(true)}
            className="p-2 text-ink-muted hover:text-ink transition-colors duration-150 rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label="Reading stats"
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M3 17V9h3v8H3zM8.5 17V5h3v12h-3zM14 17V1h3v16h-3z" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
            </svg>
          </button>
          {(vocabState.enabled || vocabState.count > 0) && (
            <button
              onClick={() => setVocabularyOpen(true)}
              className="p-2 text-ink-muted hover:text-ink transition-colors duration-150 rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              aria-label={t("vocabulary.title")}
              title={t("vocabulary.title")}
            >
              <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
                <rect x="3" y="5" width="12" height="9" rx="1.5" stroke="currentColor" strokeWidth="1.5" />
                <path d="M6 9h6M6 11.5h4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
                <path d="M17 7v7a1.5 1.5 0 01-1.5 1.5H7" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            </button>
          )}
          <button
            onClick={() => setCatalogOpen(true)}
            className="p-2 text-ink-muted hover:text-ink transition-colors duration-150 rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label="Browse catalogs"
            title="Browse book catalogs"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
              <path d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
          <PrivateModeToggle />
          <LanguageSwitcher />
          <button
            onClick={() => setSettingsOpen(true)}
            className="p-2 text-ink-muted hover:text-ink transition-colors duration-150 rounded-lg hover:bg-warm-subtle focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label="Open settings"
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path
                d="M10 12.5a2.5 2.5 0 100-5 2.5 2.5 0 000 5z"
                stroke="currentColor"
                strokeWidth="1.5"
              />
              <path
                d="M16.2 12.3a1.3 1.3 0 00.26 1.43l.05.05a1.58 1.58 0 11-2.23 2.23l-.05-.05a1.3 1.3 0 00-1.43-.26 1.3 1.3 0 00-.79 1.19v.14a1.58 1.58 0 01-3.16 0v-.07a1.3 1.3 0 00-.85-1.19 1.3 1.3 0 00-1.43.26l-.05.05a1.58 1.58 0 11-2.23-2.23l.05-.05a1.3 1.3 0 00.26-1.43 1.3 1.3 0 00-1.19-.79h-.14a1.58 1.58 0 010-3.16h.07a1.3 1.3 0 001.19-.85 1.3 1.3 0 00-.26-1.43l-.05-.05a1.58 1.58 0 112.23-2.23l.05.05a1.3 1.3 0 001.43.26h.06a1.3 1.3 0 00.79-1.19v-.14a1.58 1.58 0 013.16 0v.07a1.3 1.3 0 00.79 1.19 1.3 1.3 0 001.43-.26l.05-.05a1.58 1.58 0 112.23 2.23l-.05.05a1.3 1.3 0 00-.26 1.43v.06a1.3 1.3 0 001.19.79h.14a1.58 1.58 0 010 3.16h-.07a1.3 1.3 0 00-1.19.79z"
                stroke="currentColor"
                strokeWidth="1.5"
              />
            </svg>
          </button>
        </nav>
      )}

      <main
        key={inReader ? "reader" : "library"}
        className="flex-1 overflow-auto min-h-0"
        style={{ animation: "route-enter 0.25s ease both" }}
      >
        <Routes>
          <Route path="/" element={<Library key={profileKey} catalogImportedBookIds={catalogImportedBookIds} />} />
          <Route
            path="/reader/:bookId"
            element={
              <ReaderErrorBoundary>
                <Suspense fallback={<ReaderSkeleton />}>
                  <Reader
                    onOpenSettings={() => setSettingsOpen(true)}
                    settingsOpen={settingsOpen}
                  />
                </Suspense>
              </ReaderErrorBoundary>
            }
          />
        </Routes>
      </main>

      {statsOpen && <ReadingStats onClose={() => setStatsOpen(false)} />}
      {vocabularyOpen && <VocabularyPanel onClose={() => setVocabularyOpen(false)} />}
      {catalogOpen && (
        <CatalogBrowser
          onClose={() => { setCatalogOpen(false); setCatalogImportedBookIds([]); }}
          onBookImported={(bookId) => { if (bookId) setCatalogImportedBookIds(prev => [...prev, bookId]); setProfileKey((k) => k + 1); }}
        />
      )}

      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />

      <AnalyticsConsentDialog />
      <UpdateCheckHost deferWhilePresent={settingsOpen || statsOpen || vocabularyOpen || catalogOpen} />

      <ImportStatusBar />
    </div>
  );
}

function App() {
  return (
    <ThemeProvider>
      <ToastProvider>
        <ImportProvider>
          <OnboardingProvider>
            <BrowserRouter>
              <AppShell />
            </BrowserRouter>
          </OnboardingProvider>
        </ImportProvider>
      </ToastProvider>
    </ThemeProvider>
  );
}

export default App;
