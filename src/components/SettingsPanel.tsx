import { useEffect, useRef, useState, useCallback, useReducer, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { open as openFilePicker } from "@tauri-apps/plugin-dialog";
import { useTranslation } from "react-i18next";
import { useTheme, MIN_FONT_SIZE, MAX_FONT_SIZE, type ColorTokens } from "../context/ThemeContext";
import { friendlyError } from "../lib/errors";
import { missingRequiredFields, connectionResultFeedback } from "../lib/backupConnection";
import type { DictionaryStatus } from "../lib/dictionary";
import {
  dictionaryReducer,
  initialDictionaryState,
  downloadPercent,
  isVerifying,
} from "../lib/dictionaryState";
import { isPinUnsaved, shouldSaveOnBlur } from "../lib/pinSaveState";
import { validateWebServerPort, WEB_SERVER_PORT_MIN, WEB_SERVER_PORT_MAX, formatBytes } from "../lib/utils";
import { useOnboardingContext } from "../context/OnboardingContext";
import { startupCheckEnabled } from "../hooks/useUpdateCheck";
import { useToast } from "./Toast";
import {
  SEPIA_TOKENS,
  LIGHT_TOKENS,
  deriveTokensFromBase,
  FONT_OPTIONS,
} from "../lib/themes";
import ActivityLog from "./ActivityLog";
import ConfirmDialog from "./ConfirmDialog";
import SavedThemesList from "./SavedThemesList";
import PluginsPanel from "./PluginsPanel";
import { emit } from "@tauri-apps/api/event";
import { releaseNotes, appVersion as buildVersion } from "virtual:release-notes";
import {
  loadSavedThemes,
  saveSavedThemes,
  addTheme,
  deleteTheme,
  renameTheme,
  type SavedTheme,
} from "../lib/savedThemes";

const COMMON_PINS = new Set([
  "0000", "1111", "2222", "3333", "4444", "5555", "6666", "7777", "8888",
  "9999", "1234", "4321", "0123", "3210", "1212", "6969", "1122", "2001",
  "1984", "2000", "1010", "2580", "0852",
]);

type PinStrength = "weak" | "fair" | "strong";

function getPinStrength(pin: string): { strength: PinStrength; error: string | null } {
  if (pin.length === 0) return { strength: "weak", error: null };
  if (!/^\d+$/.test(pin)) return { strength: "weak", error: "settings.pinDigitsOnly" };
  if (pin.length < 4) return { strength: "weak", error: "settings.pinTooShort" };

  if (COMMON_PINS.has(pin)) return { strength: "weak", error: "settings.pinTooCommon" };

  const allSame = pin.split("").every((c) => c === pin[0]);
  if (allSame) return { strength: "weak", error: "settings.pinAllSame" };

  const digits = pin.split("").map((c) => parseInt(c, 10));
  const ascending = digits.every((d, i) => i === 0 || ((d - digits[i - 1] + 10) % 10) === 1);
  const descending = digits.every((d, i) => i === 0 || ((digits[i - 1] - d + 10) % 10) === 1);
  if (ascending || descending) return { strength: "weak", error: "settings.pinSequential" };

  const len = pin.length;
  for (let sub = 2; sub <= len / 2; sub++) {
    if (len % sub === 0) {
      const pat = pin.slice(0, sub);
      let repeats = true;
      for (let i = sub; i < len; i += sub) {
        if (pin.slice(i, i + sub) !== pat) { repeats = false; break; }
      }
      if (repeats) return { strength: "weak", error: "settings.pinRepeating" };
    }
  }

  if (pin.length >= 6) return { strength: "strong", error: null };
  return { strength: "fair", error: null };
}

export function Accordion({
  title,
  children,
  open,
  onToggle,
  query = "",
  keywords = "",
}: {
  title: string;
  children: ReactNode;
  open: boolean;
  onToggle: () => void;
  /** Active settings-search query. When set, non-matching sections hide and matches force-open. */
  query?: string;
  /** Extra searchable terms (key setting labels inside this section). */
  keywords?: string;
}) {
  const sectionId = `section-${title.replace(/\s+/g, "-").toLowerCase()}`;
  const q = query.trim().toLowerCase();
  if (q && !`${title} ${keywords}`.toLowerCase().includes(q)) return null;
  // While searching, force matching sections open so the content is visible.
  const isOpen = q ? true : open;
  return (
    <section>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={isOpen}
        aria-controls={sectionId}
        className="w-full flex items-center justify-between py-1 group"
      >
        <h3 className="text-xs font-semibold uppercase tracking-wider text-ink-muted">
          {title}
        </h3>
        <svg
          width="12"
          height="12"
          viewBox="0 0 20 20"
          fill="none"
          aria-hidden="true"
          className={`text-ink-muted/50 group-hover:text-ink-muted transition-transform duration-200 ${isOpen ? "rotate-180" : ""}`}
        >
          <path d="M5 7.5L10 12.5L15 7.5" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      <div
        id={sectionId}
        role="region"
        aria-label={title}
        className={`grid transition-[grid-template-rows] duration-200 ease-out ${isOpen ? "grid-rows-[1fr] mt-3" : "grid-rows-[0fr]"}`}
      >
        <div className="overflow-hidden">
          <div className="bg-warm-subtle/40 rounded-xl p-4">
            {children}
          </div>
        </div>
      </div>
    </section>
  );
}

// ── Custom color theme editor ───────────────────────────────

const TOKEN_GROUPS: Array<{
  labelKey: string;
  tokens: Array<{ key: keyof ColorTokens; labelKey: string }>;
}> = [
  {
    labelKey: "settings.accent",
    tokens: [
      { key: "accent", labelKey: "settings.accent" },
      { key: "accent-hover", labelKey: "settings.accentHover" },
      { key: "accent-light", labelKey: "settings.accentLight" },
    ],
  },
  {
    labelKey: "settings.surface",
    tokens: [
      { key: "surface", labelKey: "settings.cardLabel" },
      { key: "ink-muted", labelKey: "settings.mutedText" },
      { key: "warm-border", labelKey: "settings.border" },
      { key: "warm-subtle", labelKey: "settings.subtleFill" },
    ],
  },
];

function CustomColorEditor({
  customColors,
  setCustomColors,
}: {
  customColors: ColorTokens;
  setCustomColors: (c: ColorTokens) => void;
}) {
  const { t } = useTranslation();
  const [showAdvanced, setShowAdvanced] = useState(false);

  const updateColor = (key: keyof ColorTokens, value: string) => {
    setCustomColors({ ...customColors, [key]: value });
  };

  const updateBaseAndDerive = (key: "paper" | "ink", value: string) => {
    const paper = key === "paper" ? value : customColors.paper;
    const ink = key === "ink" ? value : customColors.ink;
    const derived = deriveTokensFromBase(paper, ink);
    setCustomColors(derived);
  };

  return (
    <div className="space-y-3 rounded-xl bg-warm-subtle p-3">
      {/* Primary pickers: Background + Text */}
      <div className="flex gap-3">
        <ColorInput
          label={t("settings.background")}
          value={customColors.paper}
          onChange={(v) => updateBaseAndDerive("paper", v)}
        />
        <ColorInput
          label={t("settings.text")}
          value={customColors.ink}
          onChange={(v) => updateBaseAndDerive("ink", v)}
        />
      </div>

      {/* Live preview */}
      <div
        className="rounded-lg px-3 py-2 text-sm leading-relaxed"
        style={{ backgroundColor: customColors.paper, color: customColors.ink }}
      >
        {t("settings.fontPreview")}
      </div>

      {/* Advanced toggle */}
      <button
        type="button"
        onClick={() => setShowAdvanced(!showAdvanced)}
        className="text-xs text-ink-muted hover:text-ink transition-colors"
      >
        {showAdvanced ? t("settings.hideAdvanced") : t("settings.showAdvanced")}
      </button>

      {/* Advanced token grid */}
      {showAdvanced && (
        <div className="space-y-3">
          {TOKEN_GROUPS.map((group) => (
            <div key={group.labelKey}>
              <p className="text-xs text-ink-muted mb-1.5">{t(group.labelKey)}</p>
              <div className="flex flex-wrap gap-2">
                {group.tokens.map(({ key, labelKey }) => (
                  <ColorInput
                    key={key}
                    label={t(labelKey)}
                    value={customColors[key]}
                    onChange={(v) => updateColor(key, v)}
                    compact
                  />
                ))}
              </div>
            </div>
          ))}
        </div>
      )}

      {/* Preset shortcuts */}
      <div className="flex gap-2 pt-1">
        <button
          type="button"
          onClick={() => setCustomColors({ ...SEPIA_TOKENS })}
          className="flex-1 px-2 py-1.5 text-xs rounded-lg border border-warm-border text-ink-muted hover:text-ink hover:border-ink-muted transition-colors"
        >
          {t("settings.resetToSepia")}
        </button>
        <button
          type="button"
          onClick={() => setCustomColors({ ...LIGHT_TOKENS })}
          className="flex-1 px-2 py-1.5 text-xs rounded-lg border border-warm-border text-ink-muted hover:text-ink hover:border-ink-muted transition-colors"
        >
          {t("settings.resetToLight")}
        </button>
      </div>
    </div>
  );
}

function ColorInput({
  label,
  value,
  onChange,
  compact = false,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  compact?: boolean;
}) {
  return (
    <label className={`flex items-center gap-2 ${compact ? "" : "flex-1"}`}>
      <div className="relative">
        <input
          type="color"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="absolute inset-0 opacity-0 cursor-pointer w-full h-full"
        />
        <div
          className={`${compact ? "w-6 h-6" : "w-8 h-8"} rounded-lg border border-warm-border shadow-sm cursor-pointer`}
          style={{ backgroundColor: value }}
        />
      </div>
      <span className={`${compact ? "text-xs" : "text-sm"} text-ink-muted`}>
        {label}
      </span>
    </label>
  );
}

interface SettingsPanelProps {
  open: boolean;
  onClose: () => void;
}

interface LibraryFolderInfo {
  path: string;
  file_count: number;
  linked_count: number;
  total_size_bytes: number;
}

interface ConfigField {
  key: string;
  label: string;
  fieldType: string;
  required: boolean;
  placeholder: string;
}

interface ProviderInfo {
  providerType: string;
  label: string;
  fields: ConfigField[];
}

interface EnrichmentProviderInfo {
  id: string;
  name: string;
  requiresApiKey: boolean;
  apiKeyHelp: string;
  config: {
    enabled: boolean;
    apiKey: string | null;
  };
}

interface BackupConfig {
  providerType: string;
  values: Record<string, string>;
}

interface ConnectionTestResult {
  status: "Ok" | "AuthFailed" | "PermissionDenied" | "NetworkError" | "Timeout";
  latency_ms?: number;
  message?: string;
}

interface SyncResult {
  booksPushed: number;
  progressPushed: number;
  bookmarksPushed: number;
  highlightsPushed: number;
  collectionsPushed: number;
  filesPushed: number;
}

interface SyncManifest {
  lastSyncAt: number;
  deviceId: string;
}

type CacheSection = {
  name: string;
  kind: "memory" | "disk";
  entry_count: number;
  total_bytes: number | null;
};
type UnifiedCacheStats = { sections: CacheSection[]; total_bytes: number };

interface MigrationDialogState {
  currentFolder: string;
  newFolder: string;
  fileCount: number;
  totalSizeBytes: number;
}

export default function SettingsPanel({ open, onClose }: SettingsPanelProps) {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const { restart: restartOnboarding } = useOnboardingContext();
  const { mode, setMode, customColors, setCustomColors, fontSize, setFontSize, fontFamily, setFontFamily, scrollMode, setScrollMode, typography, setTypography, customCss, setCustomCss, dualPage, setDualPage, mangaMode, setMangaMode, pageAnimation, setPageAnimation, loadTheme } =
    useTheme();
  const [openSection, setOpenSection] = useState<string | null>("appearance");
  const toggleSection = (id: string) => setOpenSection((prev) => (prev === id ? null : id));
  const [settingsQuery, setSettingsQuery] = useState("");
  // The panel stays mounted across open/close, so clear any prior search each
  // time it opens — otherwise it reopens filtered with sections hidden.
  useEffect(() => {
    if (open) setSettingsQuery("");
  }, [open]);
  const panelRef = useRef<HTMLDivElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  // Autostart state
  const [autoStartEnabled, setAutoStartEnabled] = useState(false);
  const [autoStartLoading, setAutoStartLoading] = useState(false);
  const [autoStartError, setAutoStartError] = useState<string | null>(null);

  // Usage analytics consent
  const [analyticsEnabled, setAnalyticsEnabled] = useState(false);

  // App version for the About section — reads tauri.conf.json at runtime
  // so we don't have to sync a frontend constant with the package.
  const [appVersion, setAppVersion] = useState<string>("");
  useEffect(() => {
    getVersion().then(setAppVersion).catch(() => setAppVersion(""));
  }, []);

  const hasReleaseNotes = releaseNotes.some((r) => r.version === buildVersion);

  // Continue Reading section visibility (default true)
  const [showContinueReading, setShowContinueReading] = useState(() => {
    const stored = localStorage.getItem("folio-show-continue-reading");
    return stored === null ? true : stored === "true";
  });

  // Discover section visibility
  const [showDiscover, setShowDiscover] = useState(() => localStorage.getItem("folio-show-discover") === "true");

  // Want to Read section visibility
  const [showWantToRead, setShowWantToRead] = useState(() => localStorage.getItem("folio-show-want-to-read") === "true");

  // Auto-focus new imported books (default off)
  const [autoFocusNewBooks, setAutoFocusNewBooks] = useState(() => localStorage.getItem("folio-auto-focus-new-books") === "true");
  const [updateCheckStartup, setUpdateCheckStartup] = useState(true);

  const [savedThemes, setSavedThemes] = useState<SavedTheme[]>(loadSavedThemes);
  const [activeThemeId, setActiveThemeId] = useState<string | null>(null);

  const handleLoadTheme = useCallback((theme: SavedTheme) => {
    loadTheme(theme);
    setActiveThemeId(theme.id);
  }, [loadTheme]);

  const handleSaveTheme = useCallback((name: string) => {
    const existing = savedThemes.find((t) => t.name === name);
    const theme: SavedTheme = {
      id: existing ? existing.id : crypto.randomUUID(),
      name,
      mode,
      colors: customColors,
      fontFamily,
      fontSize,
      typography,
      createdAt: existing ? existing.createdAt : Date.now(),
    };
    const updated = addTheme(savedThemes, theme);
    setSavedThemes(updated);
    saveSavedThemes(updated);
  }, [mode, customColors, fontFamily, fontSize, typography, savedThemes]);

  const handleDeleteTheme = useCallback((id: string) => {
    const updated = deleteTheme(savedThemes, id);
    setSavedThemes(updated);
    saveSavedThemes(updated);
  }, [savedThemes]);

  const handleRenameTheme = useCallback((id: string, newName: string) => {
    const updated = renameTheme(savedThemes, id, newName);
    setSavedThemes(updated);
    saveSavedThemes(updated);
  }, [savedThemes]);

  // Library folder state
  const [libraryFolder, setLibraryFolder] = useState<string | null>(null);
  const [libraryInfo, setLibraryInfo] = useState<LibraryFolderInfo | null>(null);
  const [migrationDialog, setMigrationDialog] = useState<MigrationDialogState | null>(null);
  const [dontMoveFiles, setDontMoveFiles] = useState(false);
  const [migrating, setMigrating] = useState(false);
  const [migrationError, setMigrationError] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
  const [backupMessage, setBackupMessage] = useState<string | null>(null);
  const [restoreModalOpen, setRestoreModalOpen] = useState(false);
  const [autoBackups, setAutoBackups] = useState<{ path: string; label: string; timestamp: number; sizeBytes: number }[]>([]);
  const [restoreConfirmPath, setRestoreConfirmPath] = useState<string | null>(null);
  const [restoring, setRestoring] = useState(false);
  const [includeFiles, setIncludeFiles] = useState(false);
  const [cleanupState, setCleanupState] = useState<
    "idle" | "confirm" | "scanning" | "done"
  >("idle");
  const [cleanupProgress, setCleanupProgress] = useState({ current: 0, total: 0 });
  const [cleanupResult, setCleanupResult] = useState<{
    removedCount: number;
    backupPath: string;
  } | null>(null);

  // Import mode setting
  const [importMode, setImportMode] = useState<string>("import");

  // Page cache settings
  const [pageCacheLimit, setPageCacheLimit] = useState("500");
  const [cacheStats, setCacheStats] = useState<UnifiedCacheStats | null>(null);

  // Metadata scan settings
  const [autoScanImport, setAutoScanImport] = useState(true);
  const [autoScanStartup, setAutoScanStartup] = useState(false);

  // Offline dictionary (F-1-1)
  const [dictEnabled, setDictEnabled] = useState(false);
  const [dictState, dictDispatch] = useReducer(dictionaryReducer, undefined, initialDictionaryState);
  const [dictDeleteConfirm, setDictDeleteConfirm] = useState(false);

  // Vocabulary builder (F-1-5)
  const [vocabEnabled, setVocabEnabled] = useState(false);

  // Enrichment providers
  const [enrichmentProviders, setEnrichmentProviders] = useState<EnrichmentProviderInfo[]>([]);

  // Activity log state
  const [showActivityLog, setShowActivityLog] = useState(false);

  // Remote backup state
  const [backupProviders, setBackupProviders] = useState<ProviderInfo[]>([]);
  const [selectedProvider, setSelectedProvider] = useState<string>("");
  const [backupFieldValues, setBackupFieldValues] = useState<Record<string, string>>({});
  const [savedBackupConfig, setSavedBackupConfig] = useState<BackupConfig | null>(null);
  const [savingBackupConfig, setSavingBackupConfig] = useState(false);
  const [runningBackup, setRunningBackup] = useState(false);
  const [backupProgressText, setBackupProgressText] = useState("");
  const [backupStatus, setBackupStatus] = useState<SyncManifest | null>(null);
  const [remoteBackupMessage, setRemoteBackupMessage] = useState<string | null>(null);
  // Save and Test connection report their outcomes independently so a save
  // failure is never conflated with a connectivity failure (UX audit F4c).
  const [saveFeedback, setSaveFeedback] = useState<{ text: string; isError: boolean } | null>(null);
  const [testFeedback, setTestFeedback] = useState<{ text: string; isError: boolean } | null>(null);
  const [testingConnection, setTestingConnection] = useState(false);
  const [syncEnabled, setSyncEnabled] = useState(false);
  const [lastSyncSuccess, setLastSyncSuccess] = useState<number | null>(null);
  const [lastSyncError, setLastSyncError] = useState<{ at: number; message: string } | null>(null);

  // Web server state
  const [webServerRunning, setWebServerRunning] = useState(false);
  const [webServerUrl, setWebServerUrl] = useState<string | null>(null);
  const [webServerPort, setWebServerPort] = useState("7788");
  const [webServerPin, setWebServerPin] = useState("");
  const [webServerQr, setWebServerQr] = useState<string | null>(null);
  const [webServerError, setWebServerError] = useState<string | null>(null);
  const [webServerLoading, setWebServerLoading] = useState(false);
  const [webServerPortError, setWebServerPortError] = useState<string | null>(null);
  const [webUiEnabled, setWebUiEnabled] = useState(false);
  const [opdsEnabled, setOpdsEnabled] = useState(false);
  const [pinSaved, setPinSaved] = useState(false);
  // The last PIN value successfully persisted via `web_server_set_pin`. Drives
  // the dirty/unsaved + save-on-blur decisions. This is distinct from
  // `pinSaved`, which is only the transient "PIN saved ✓" confirmation.
  const [savedPin, setSavedPin] = useState("");
  const [pinError, setPinError] = useState<string | null>(null);

  // Profile lock state (A-M3). Distinct from the web-access PIN above:
  // this gates switching into the *active* profile inside the desktop app
  // itself (see docs/superpowers/specs/2026-07-07-profile-soft-lock-design.md).
  const [profileLockName, setProfileLockName] = useState("default");
  const [profileLockStatus, setProfileLockStatus] = useState<{
    locked: boolean;
    unlockedThisSession: boolean;
  } | null>(null);
  const [newLockPassword, setNewLockPassword] = useState("");
  const [currentLockPassword, setCurrentLockPassword] = useState("");
  const [changeLockPassword, setChangeLockPassword] = useState("");
  const [lockBusy, setLockBusy] = useState(false);
  const [pendingRemoveLock, setPendingRemoveLock] = useState(false);

  // Custom fonts
  interface CustomFont {
    id: string;
    name: string;
    fileName: string;
    filePath: string;
    createdAt: number;
  }

  const [customFonts, setCustomFonts] = useState<CustomFont[]>([]);
  const [deletingFontId, setDeletingFontId] = useState<string | null>(null);

  const loadCustomFonts = useCallback(async () => {
    try {
      const fonts = await invoke<CustomFont[]>("get_custom_fonts");
      setCustomFonts(fonts);
    } catch {
      // non-fatal
    }
  }, []);

  useEffect(() => {
    loadCustomFonts();
  }, [loadCustomFonts]);

  // Load autostart state on mount
  useEffect(() => {
    invoke<boolean>("get_autostart_enabled").then(setAutoStartEnabled).catch(() => {});
  }, []);

  // Inject @font-face rules for custom fonts
  useEffect(() => {
    const styleId = "custom-fonts-style";
    let style = document.getElementById(styleId) as HTMLStyleElement | null;
    if (!style) {
      style = document.createElement("style");
      style.id = styleId;
      document.head.appendChild(style);
    }
    style.textContent = customFonts
      .map(
        (f) =>
          `@font-face { font-family: "CustomFont-${f.id}"; src: url("https://asset.localhost/${f.filePath}"); font-display: swap; }`,
      )
      .join("\n");
  }, [customFonts]);

  const handleImportFont = async () => {
    try {
      const selected = await openFilePicker({
        multiple: false,
        filters: [
          { name: "Font Files", extensions: ["ttf", "otf", "woff2"] },
        ],
      });
      if (!selected) return;
      const filePath = typeof selected === "string" ? selected : selected[0];
      await invoke("import_custom_font", { filePath });
      await loadCustomFonts();
    } catch {
      // non-fatal
    }
  };

  const handleDeleteFont = async (fontId: string) => {
    try {
      if (fontFamily === `custom:${fontId}`) {
        setFontFamily("serif");
      }
      await invoke("remove_custom_font", { fontId });
      await loadCustomFonts();
    } catch {
      // non-fatal
    }
    setDeletingFontId(null);
  };

  const loadLibraryFolder = useCallback(async () => {
    try {
      const folder = await invoke<string>("get_library_folder");
      setLibraryFolder(folder);
      const info = await invoke<LibraryFolderInfo>("get_library_folder_info");
      setLibraryInfo(info);
    } catch (e) {
      addToast(friendlyError(e, t), "error");
    }
  }, []);

  const loadCacheInfo = useCallback(async () => {
    try {
      const limit = await invoke<string | null>("get_setting_value", { key: "page_cache_max_size_mb" });
      if (limit) setPageCacheLimit(limit);
      const stats = await invoke<UnifiedCacheStats>("get_unified_cache_stats");
      setCacheStats(stats);
    } catch {
      // Cache stats unavailable
    }
  }, []);

  const loadBackupSettings = useCallback(async () => {
    try {
      const providers = await invoke<ProviderInfo[]>("get_backup_providers");
      setBackupProviders(providers);
      const config = await invoke<BackupConfig | null>("get_backup_config");
      if (config) {
        setSavedBackupConfig(config);
        setSelectedProvider(config.providerType);
        setBackupFieldValues(config.values);
      } else if (providers.length > 0) {
        setSelectedProvider(providers[0].providerType);
      }
      const status = await invoke<SyncManifest | null>("get_backup_status");
      setBackupStatus(status);

      const syncSetting = await invoke<string | null>("get_setting_value", { key: "sync_enabled" });
      setSyncEnabled(syncSetting === "true");

      const successAt = await invoke<string | null>("get_setting_value", { key: "last_sync_success_at" });
      setLastSyncSuccess(successAt ? parseInt(successAt, 10) : null);

      const errorAt = await invoke<string | null>("get_setting_value", { key: "last_sync_error_at" });
      const errorMsg = await invoke<string | null>("get_setting_value", { key: "last_sync_error_message" });
      if (errorAt) {
        setLastSyncError({ at: parseInt(errorAt, 10), message: errorMsg || "Unknown error" });
      } else {
        setLastSyncError(null);
      }
    } catch (e) {
      addToast(friendlyError(e, t), "error");
    }
  }, []);

  const loadProviders = useCallback(async () => {
    try {
      const providers = await invoke<EnrichmentProviderInfo[]>("get_enrichment_providers");
      setEnrichmentProviders(providers);
    } catch {}
  }, []);

  const loadServerStatus = useCallback(async () => {
    try {
      const status = await invoke<{
        running: boolean;
        url: string | null;
        port: number;
        hasPin: boolean;
        webUiEnabled: boolean;
        opdsEnabled: boolean;
      }>("web_server_status");
      setWebServerRunning(status.running);
      setWebServerUrl(status.url);
      setWebServerPort(String(status.port));
      setWebUiEnabled(status.webUiEnabled);
      setOpdsEnabled(status.opdsEnabled);
      if (status.running) {
        try { const qr = await invoke<string>("web_server_get_qr"); setWebServerQr(qr); } catch {}
      } else {
        setWebServerQr(null);
      }
    } catch {}
  }, []);

  // Save the typed PIN. Shared by the explicit button and the blur handler.
  // Returns true on success. Never submits an invalid PIN.
  const savePin = useCallback(async () => {
    const { error } = getPinStrength(webServerPin);
    if (error) { setPinError(t(error)); return false; }
    try {
      await invoke("web_server_set_pin", { pin: webServerPin });
      setWebServerError(null);
      setPinError(null);
      setSavedPin(webServerPin);
      setPinSaved(true);
      setTimeout(() => setPinSaved(false), 2000);
      return true;
    } catch (e) {
      setWebServerError(friendlyError(e, t));
      return false;
    }
  }, [webServerPin, t]);

  // Loads which profile is active and its lock status. The section always
  // operates on the currently active profile — there's no picker, matching
  // how the rest of Settings implicitly scopes to it (e.g. the library
  // folder, web server settings).
  const loadProfileLockStatus = useCallback(async () => {
    try {
      const profiles = await invoke<{ name: string; is_active: boolean }[]>("get_profiles");
      const active = profiles.find((p) => p.is_active)?.name ?? "default";
      setProfileLockName(active);
      const status = await invoke<{ locked: boolean; unlockedThisSession: boolean }>(
        "profile_lock_status",
        { profile: active }
      );
      setProfileLockStatus(status);
    } catch {
      // non-fatal
    }
  }, []);

  const handleSetProfileLock = useCallback(async () => {
    if (!newLockPassword || lockBusy) return;
    setLockBusy(true);
    try {
      await invoke("set_profile_lock", { profile: profileLockName, password: newLockPassword });
      setNewLockPassword("");
      await loadProfileLockStatus();
      addToast(t("settings.lockSetSuccess"), "success");
    } catch (e) {
      addToast(friendlyError(e, t), "error");
    } finally {
      setLockBusy(false);
    }
  }, [newLockPassword, lockBusy, profileLockName, loadProfileLockStatus, addToast, t]);

  const handleChangeProfileLock = useCallback(async () => {
    if (!currentLockPassword || !changeLockPassword || lockBusy) return;
    setLockBusy(true);
    try {
      await invoke("set_profile_lock", {
        profile: profileLockName,
        password: changeLockPassword,
        currentPassword: currentLockPassword,
      });
      setCurrentLockPassword("");
      setChangeLockPassword("");
      addToast(t("settings.lockChangedSuccess"), "success");
    } catch (e) {
      addToast(friendlyError(e, t), "error");
    } finally {
      setLockBusy(false);
    }
  }, [currentLockPassword, changeLockPassword, lockBusy, profileLockName, addToast, t]);

  const handleRemoveProfileLock = useCallback(async () => {
    if (!currentLockPassword || lockBusy) return;
    setLockBusy(true);
    try {
      await invoke("remove_profile_lock", { profile: profileLockName, currentPassword: currentLockPassword });
      setCurrentLockPassword("");
      setChangeLockPassword("");
      setPendingRemoveLock(false);
      await loadProfileLockStatus();
      addToast(t("settings.lockRemovedSuccess"), "success");
    } catch (e) {
      addToast(friendlyError(e, t), "error");
    } finally {
      setLockBusy(false);
    }
  }, [currentLockPassword, lockBusy, profileLockName, loadProfileLockStatus, addToast, t]);

  useEffect(() => {
    if (!open) return;
    const onFocus = () => { loadServerStatus(); };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [open, loadServerStatus]);

  useEffect(() => {
    if (open) {
      loadLibraryFolder();
      loadBackupSettings();
      loadProviders();
      loadCacheInfo();
      loadProfileLockStatus();
      (async () => {
        const scanImport = await invoke<string | null>("get_setting_value", { key: "auto_scan_import" });
        setAutoScanImport(scanImport !== "false");
        const scanStartup = await invoke<string | null>("get_setting_value", { key: "auto_scan_startup" });
        setAutoScanStartup(scanStartup === "true");
        const importModeVal = await invoke<string | null>("get_setting_value", { key: "import_mode" });
        if (importModeVal) setImportMode(importModeVal);
        const dictEnabledVal = await invoke<string | null>("get_setting_value", { key: "dictionary_enabled" });
        setDictEnabled(dictEnabledVal === "true");
        const dictStatus = await invoke<DictionaryStatus>("get_dictionary_status");
        dictDispatch({ type: "statusLoaded", status: dictStatus });
        const vocabEnabledVal = await invoke<string | null>("get_setting_value", { key: "vocabulary_enabled" });
        setVocabEnabled(vocabEnabledVal === "true");
        const analyticsConsent = await invoke<string | null>("get_analytics_consent");
        setAnalyticsEnabled(analyticsConsent === "enabled");
        const updateCheckVal = await invoke<string | null>("get_setting_value", {
          key: "update_check_on_startup",
        });
        setUpdateCheckStartup(startupCheckEnabled(updateCheckVal));
        await loadServerStatus();
      })().catch(() => {});
    }
  }, [open, loadLibraryFolder, loadBackupSettings, loadProviders, loadCacheInfo, loadServerStatus, loadProfileLockStatus]);

  useEffect(() => {
    if (!open) return;

    previousFocus.current = document.activeElement as HTMLElement;
    requestAnimationFrame(() => panelRef.current?.focus());

    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        if (migrationDialog) {
          setMigrationDialog(null);
          setMigrationError(null);
          return;
        }
        onClose();
        return;
      }

      if (e.key === "Tab" && panelRef.current) {
        const focusable = panelRef.current.querySelectorAll<HTMLElement>(
          'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
        );
        if (focusable.length === 0) return;

        const first = focusable[0];
        const last = focusable[focusable.length - 1];

        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      previousFocus.current?.focus();
    };
  }, [open, onClose, migrationDialog]);

  const handleCacheLimitChange = useCallback(async (value: string) => {
    setPageCacheLimit(value);
    try {
      await invoke("set_setting_value", { key: "page_cache_max_size_mb", value });
    } catch {
      // Setting save failed
    }
  }, []);

  const handleClearCache = useCallback(async () => {
    try {
      await invoke("clear_page_cache");
      await loadCacheInfo();
    } catch {
      // Clear failed
    }
  }, [loadCacheInfo]);

  const handleClearAllCaches = useCallback(async () => {
    try {
      await invoke("clear_all_caches");
      await loadCacheInfo();
    } catch {
      // Clear failed
    }
  }, [loadCacheInfo]);

  const handleSetModes = useCallback(
    async (next: { webUi?: boolean; opds?: boolean; port?: number }) => {
      // Resolve the port to send. When the caller passes an explicit port
      // (the onBlur path) it is already validated. Otherwise validate the
      // current field value here — this is the single choke point, so a
      // toggle change with an out-of-range port in the input never reaches
      // the backend.
      let port = next.port;
      if (port === undefined) {
        const result = validateWebServerPort(webServerPort);
        if (!result.valid) {
          setWebServerPortError(
            t("settings.portRange", {
              min: WEB_SERVER_PORT_MIN,
              max: WEB_SERVER_PORT_MAX,
            })
          );
          return;
        }
        port = result.port;
      }
      setWebServerError(null);
      setWebServerLoading(true);
      try {
        const status = await invoke<{
          running: boolean;
          url: string | null;
          port: number;
          hasPin: boolean;
          webUiEnabled: boolean;
          opdsEnabled: boolean;
        }>("web_server_set_modes", {
          webUi: next.webUi ?? webUiEnabled,
          opds: next.opds ?? opdsEnabled,
          port,
        });
        setWebUiEnabled(status.webUiEnabled);
        setOpdsEnabled(status.opdsEnabled);
        setWebServerRunning(status.running);
        setWebServerUrl(status.url ?? null);
        if (status.running) {
          try {
            const qr = await invoke<string>("web_server_get_qr");
            setWebServerQr(qr);
          } catch {
            // QR generation failed; non-fatal.
          }
        } else {
          setWebServerQr(null);
        }
      } catch (e) {
        setWebServerError(friendlyError(e, t));
        // Settings were persisted before the start attempt — mirror that intent
        // into local state so the checkboxes don't snap back to the previous
        // values (which would mislead the user about what's saved).
        if (next.webUi !== undefined) setWebUiEnabled(next.webUi);
        if (next.opds !== undefined) setOpdsEnabled(next.opds);
      } finally {
        setWebServerLoading(false);
      }
    },
    [webUiEnabled, opdsEnabled, webServerPort, t],
  );

  const handleChangeFolder = async () => {
    try {
      const picked = await openFilePicker({ directory: true });
      if (!picked) return;

      const newFolder = typeof picked === "string" ? picked : picked[0];
      if (!newFolder) return;

      const info = await invoke<LibraryFolderInfo>("get_library_folder_info");
      setDontMoveFiles(false);
      setMigrationError(null);
      setMigrationDialog({
        currentFolder: info.path,
        newFolder,
        fileCount: info.file_count,
        totalSizeBytes: info.total_size_bytes,
      });
    } catch (err) {
      // Folder picker cancelled or command unavailable
    }
  };

  const handleConfirmMigration = async () => {
    if (!migrationDialog) return;
    setMigrating(true);
    setMigrationError(null);
    try {
      await invoke("set_library_folder", {
        newFolder: migrationDialog.newFolder,
        moveFiles: !dontMoveFiles,
      });
      setLibraryFolder(migrationDialog.newFolder);
      setMigrationDialog(null);
    } catch (err) {
      setMigrationError(friendlyError(err, t));
    } finally {
      setMigrating(false);
    }
  };

  const handleCancelMigration = () => {
    if (migrating) return;
    setMigrationDialog(null);
    setMigrationError(null);
  };

  const handleExport = async () => {
    try {
      const dest = await openFilePicker({
        directory: true,
      });
      if (!dest) return;
      setExporting(true);
      setBackupMessage(null);
      const folder = typeof dest === "string" ? dest : dest[0];
      const path = `${folder}/carrel-backup-${new Date().toISOString().slice(0, 10)}.zip`;
      await invoke("export_library", { destPath: path, includeFiles });
      setBackupMessage(t("settings.exportedTo", { path }));
    } catch (err) {
      setBackupMessage(t("settings.exportFailed", { error: friendlyError(err, t) }));
    } finally {
      setExporting(false);
    }
  };

  const loadAutoBackups = async () => {
    try {
      const list = await invoke<{ path: string; label: string; timestamp: number; sizeBytes: number }[]>(
        "list_auto_backups"
      );
      setAutoBackups(list);
    } catch {
      setAutoBackups([]);
    }
  };

  const handleOpenRestoreModal = async () => {
    setRestoreModalOpen(true);
    setBackupMessage(null);
    await loadAutoBackups();
  };

  const handleRestoreFromFile = async () => {
    try {
      const selected = await openFilePicker({
        multiple: false,
        filters: [{ name: "Backup", extensions: ["zip"] }],
      } as Parameters<typeof openFilePicker>[0]);
      if (!selected) return;
      const path = typeof selected === "string" ? selected : selected[0];
      setRestoreConfirmPath(path);
    } catch {
      // User cancelled
    }
  };

  const handleConfirmRestore = async () => {
    if (!restoreConfirmPath) return;
    setRestoring(true);
    try {
      const count = await invoke<number>("import_library_backup", { archivePath: restoreConfirmPath });
      setBackupMessage(t("settings.importedBooks", { count }));
      // Tell the Library screen to re-fetch — restore writes books,
      // collections, and tags directly to the DB.
      window.dispatchEvent(new Event("folio-library-changed"));
      setRestoreConfirmPath(null);
      setRestoreModalOpen(false);
    } catch (err) {
      setBackupMessage(t("settings.importFailed", { error: friendlyError(err, t) }));
      setRestoreConfirmPath(null);
    } finally {
      setRestoring(false);
    }
  };

  const currentProviderInfo = backupProviders.find(
    (p) => p.providerType === selectedProvider
  );

  const handleProviderChange = (providerType: string) => {
    setSelectedProvider(providerType);
    setBackupFieldValues({});
    setRemoteBackupMessage(null);
    setSaveFeedback(null);
    setTestFeedback(null);
  };

  // Update a single backup config field and invalidate any prior save/test
  // feedback — once the entered values change, "saved"/test results no longer
  // describe what's in the form, so they must clear (UX audit F4c follow-up).
  const updateBackupField = (key: string, value: string) => {
    setBackupFieldValues((prev) => ({ ...prev, [key]: value }));
    setSaveFeedback(null);
    setTestFeedback(null);
  };

  // Save persists the config without running a connectivity check, so a save
  // failure is reported on its own and never conflated with a connection
  // failure (UX audit F4c).
  const handleSaveBackupConfig = async () => {
    if (!selectedProvider || !currentProviderInfo) return;
    const missing = missingRequiredFields(currentProviderInfo.fields, backupFieldValues);
    if (missing.length > 0) {
      setSaveFeedback({ text: t("settings.requiredFields", { fields: missing.join(", ") }), isError: true });
      setTestFeedback(null);
      return;
    }
    setSavingBackupConfig(true);
    setSaveFeedback(null);
    setTestFeedback(null);
    try {
      const config: BackupConfig = {
        providerType: selectedProvider,
        values: backupFieldValues,
      };
      await invoke<ConnectionTestResult>("save_backup_config", { config, skipTest: true });
      setSavedBackupConfig(config);
      setSaveFeedback({ text: t("settings.configSaved"), isError: false });
    } catch (err) {
      setSaveFeedback({ text: t("settings.saveFailed", { error: friendlyError(err, t) }), isError: true });
    } finally {
      setSavingBackupConfig(false);
    }
  };

  // Test connection checks connectivity against the currently entered values
  // and reports its own outcome, distinct from the save feedback above.
  const handleTestConnection = async () => {
    if (!selectedProvider || !currentProviderInfo) return;
    const missing = missingRequiredFields(currentProviderInfo.fields, backupFieldValues);
    if (missing.length > 0) {
      setTestFeedback({ text: t("settings.requiredFields", { fields: missing.join(", ") }), isError: true });
      setSaveFeedback(null);
      return;
    }
    setTestingConnection(true);
    setTestFeedback(null);
    setSaveFeedback(null);
    try {
      const config: BackupConfig = {
        providerType: selectedProvider,
        values: backupFieldValues,
      };
      const result = await invoke<ConnectionTestResult>("test_backup_connection", { config });
      setTestFeedback(connectionResultFeedback(result, t));
    } catch (err) {
      setTestFeedback({ text: t("settings.testFailed", { error: friendlyError(err, t) }), isError: true });
    } finally {
      setTestingConnection(false);
    }
  };

  const handleToggleSync = async (enabled: boolean) => {
    const prev = syncEnabled;
    setSyncEnabled(enabled);
    try {
      await invoke("set_setting_value", { key: "sync_enabled", value: enabled ? "true" : "false" });
    } catch {
      setSyncEnabled(prev);
    }
  };

  const handleToggleDictionary = async (val: boolean) => {
    setDictEnabled(val); // optimistic
    try {
      await invoke("set_setting_value", { key: "dictionary_enabled", value: val ? "true" : "false" });
      if (val && dictState.phase === "unknown") {
        const status = await invoke<DictionaryStatus>("get_dictionary_status");
        dictDispatch({ type: "statusLoaded", status });
      }
    } catch (err) {
      setDictEnabled(!val); // revert — the setting did not persist
      addToast(friendlyError(err, t), "error");
    }
  };

  const handleToggleVocabulary = async (val: boolean) => {
    setVocabEnabled(val); // optimistic
    try {
      await invoke("set_setting_value", { key: "vocabulary_enabled", value: val ? "true" : "false" });
    } catch (err) {
      setVocabEnabled(!val); // revert — the setting did not persist
      addToast(friendlyError(err, t), "error");
    }
  };

  const handleDownloadDictionary = async () => {
    dictDispatch({ type: "downloadStarted" });
    // Listener lifecycle mirrors the backup flow: subscribe, await the invoke,
    // always unlisten in finally.
    const unlisten = await listen<{ loaded: number; total: number }>(
      "dictionary-download-progress",
      (event) => {
        dictDispatch({ type: "downloadProgress", loaded: event.payload.loaded, total: event.payload.total });
      },
    );
    try {
      await invoke("download_dictionary");
      const status = await invoke<DictionaryStatus>("get_dictionary_status");
      dictDispatch({ type: "downloadSucceeded", status });
    } catch (err) {
      dictDispatch({ type: "downloadFailed", error: friendlyError(err, t) });
    } finally {
      unlisten();
    }
  };

  const handleDeleteDictionary = async () => {
    setDictDeleteConfirm(false);
    try {
      await invoke("delete_dictionary");
      dictDispatch({ type: "deleted" });
    } catch (err) {
      addToast(friendlyError(err, t), "error");
    }
  };

  const handleRunBackup = async () => {
    setRunningBackup(true);
    setRemoteBackupMessage(null);
    setBackupProgressText(t("settings.backupStarting"));
    const unlisten = await listen<{ step: string; current: number; total: number }>("backup-progress", (event) => {
      const { step, current, total } = event.payload;
      if (total > 0) {
        setBackupProgressText(`${step} ${current}/${total}`);
      } else {
        setBackupProgressText(step);
      }
    });
    try {
      const result = await invoke<SyncResult>("run_backup");
      const parts: string[] = [];
      if (result.booksPushed > 0) parts.push(`${result.booksPushed} books`);
      if (result.progressPushed > 0) parts.push(`${result.progressPushed} progress entries`);
      if (result.bookmarksPushed > 0) parts.push(`${result.bookmarksPushed} bookmarks`);
      if (result.highlightsPushed > 0) parts.push(`${result.highlightsPushed} highlights`);
      if (result.collectionsPushed > 0) parts.push(`${result.collectionsPushed} collections`);
      if (result.filesPushed > 0) parts.push(`${result.filesPushed} files`);
      const summary =
        parts.length > 0 ? t("settings.backedUp", { details: parts.join(", ") }) : t("settings.alreadyUpToDate");
      setRemoteBackupMessage(summary);
      const status = await invoke<SyncManifest | null>("get_backup_status");
      setBackupStatus(status);
    } catch (err) {
      setRemoteBackupMessage(t("settings.backupFailed", { error: friendlyError(err, t) }));
    } finally {
      unlisten();
      setRunningBackup(false);
      setBackupProgressText("");
    }
  };

  const handleCleanup = async () => {
    setCleanupState("scanning");
    setCleanupProgress({ current: 0, total: 0 });
    const unlisten = await listen<{ current: number; total: number }>(
      "cleanup-progress",
      (event) => {
        setCleanupProgress(event.payload);
      }
    );
    try {
      const result = await invoke<{ removedCount: number; removedBooks: { id: string; title: string; author: string }[]; backupPath: string }>(
        "cleanup_library"
      );
      setCleanupResult({ removedCount: result.removedCount, backupPath: result.backupPath });
      setCleanupState("done");
    } catch (err) {
      setCleanupResult(null);
      setCleanupState("idle");
    } finally {
      unlisten();
    }
  };

  if (!open) return null;

  const diskCacheSection = cacheStats?.sections.find((s) => s.name === "pages") ?? null;
  const cacheSectionLabel = (name: string) => {
    switch (name) {
      case "epub":
        return t("settings.cacheSectionEpub");
      case "mobi":
        return t("settings.cacheSectionMobi");
      case "pages":
        return t("settings.cacheSectionPages");
      default:
        return name;
    }
  };

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 bg-ink/20 backdrop-blur-sm z-40"
        onClick={onClose}
        aria-hidden="true"
      />

      {/* Panel */}
      <div
        ref={panelRef}
        role="dialog"
        aria-labelledby="settings-panel-title"
        aria-modal="true"
        tabIndex={-1}
        className="fixed right-0 top-0 bottom-0 w-80 max-w-[90vw] bg-surface border-l border-warm-border z-50 flex flex-col shadow-[-4px_0_24px_-4px_rgba(44,34,24,0.12)] outline-none animate-slide-in-right"
      >
        {/* Header */}
        <div className="px-5 py-4 border-b border-warm-border flex items-center justify-between">
          <h2 id="settings-panel-title" className="font-serif text-base font-semibold text-ink">
            {t("settings.title")}
          </h2>
          <button
            onClick={onClose}
            className="p-1 text-ink-muted hover:text-ink transition-colors rounded focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            aria-label={t("settings.closeLabel")}
          >
            <svg width="18" height="18" viewBox="0 0 20 20" fill="none">
              <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        {/* Search / filter */}
        <div className="px-5 pt-4">
          <div className="relative">
            <svg
              width="14" height="14" viewBox="0 0 20 20" fill="none" aria-hidden="true"
              className="absolute left-2.5 top-1/2 -translate-y-1/2 text-ink-muted/60"
            >
              <circle cx="9" cy="9" r="5.5" stroke="currentColor" strokeWidth="1.5" />
              <path d="M13 13l4 4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
            <input
              type="text"
              value={settingsQuery}
              onChange={(e) => setSettingsQuery(e.target.value)}
              placeholder={t("settings.searchPlaceholder")}
              aria-label={t("settings.searchPlaceholder")}
              className="w-full h-9 pl-8 pr-8 bg-warm-subtle rounded-lg text-sm text-ink placeholder-ink-muted/50 border border-transparent focus:border-accent focus:outline-none"
            />
            {settingsQuery && (
              <button
                type="button"
                onClick={() => setSettingsQuery("")}
                aria-label={t("common.clear")}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-ink-muted hover:text-ink text-xs"
              >
                <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
                  <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
            )}
          </div>
        </div>

        {/* Settings content */}
        <div className="flex-1 overflow-y-auto p-5 space-y-7">
          {/* General */}
          <Accordion title={t("settings.general")} open={openSection === "general"} onToggle={() => toggleSection("general")} query={settingsQuery} keywords={t("settings.searchTermsGeneral") + " " + t("settings.searchTermsAnalytics")}>
            <div className="space-y-2">
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.launchAtStartup")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.launchAtStartupHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={autoStartEnabled}
                  disabled={autoStartLoading}
                  onClick={async () => {
                    const newValue = !autoStartEnabled;
                    setAutoStartLoading(true);
                    setAutoStartError(null);
                    try {
                      await invoke("set_autostart_enabled", { enabled: newValue });
                      setAutoStartEnabled(newValue);
                    } catch (e) {
                      setAutoStartError(t("settings.autoStartFailed", { error: friendlyError(e, t) }));
                    }
                    setAutoStartLoading(false);
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${autoStartEnabled ? "bg-accent" : "bg-warm-border"} ${autoStartLoading ? "opacity-40 cursor-not-allowed" : ""}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${autoStartEnabled ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              {autoStartError && (
                <p className="text-xs text-red-500 px-1">{autoStartError}</p>
              )}
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.analyticsToggle")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.analyticsToggleHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={analyticsEnabled}
                  onClick={async () => {
                    const next = !analyticsEnabled;
                    setAnalyticsEnabled(next); // optimistic
                    try {
                      await invoke("set_analytics_consent", { consent: next ? "enabled" : "disabled" });
                    } catch (e) {
                      setAnalyticsEnabled(!next); // revert — did not persist
                      addToast(friendlyError(e, t), "error");
                    }
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${analyticsEnabled ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${analyticsEnabled ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.showContinueReading")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.showContinueReadingHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={showContinueReading}
                  data-testid="show-continue-reading-toggle"
                  onClick={() => {
                    const next = !showContinueReading;
                    setShowContinueReading(next);
                    localStorage.setItem("folio-show-continue-reading", String(next));
                    window.dispatchEvent(new Event("folio-show-continue-reading-changed"));
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${showContinueReading ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${showContinueReading ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.showDiscover")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.showDiscoverHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={showDiscover}
                  data-testid="show-discover-toggle"
                  onClick={() => {
                    const next = !showDiscover;
                    setShowDiscover(next);
                    localStorage.setItem("folio-show-discover", String(next));
                    window.dispatchEvent(new Event("folio-show-discover-changed"));
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${showDiscover ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${showDiscover ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.showWantToRead")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.showWantToReadHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={showWantToRead}
                  data-testid="show-want-to-read-toggle"
                  onClick={() => {
                    const next = !showWantToRead;
                    setShowWantToRead(next);
                    localStorage.setItem("folio-show-want-to-read", String(next));
                    window.dispatchEvent(new Event("folio-show-want-to-read-changed"));
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${showWantToRead ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${showWantToRead ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              <label className="flex items-center justify-between gap-3 bg-warm-subtle rounded-xl px-3 py-2.5">
                <div>
                  <span className="text-sm text-ink">{t("settings.autoFocusNewBooks")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.autoFocusNewBooksHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={autoFocusNewBooks}
                  data-testid="auto-focus-new-books-toggle"
                  onClick={() => {
                    const next = !autoFocusNewBooks;
                    setAutoFocusNewBooks(next);
                    localStorage.setItem("folio-auto-focus-new-books", String(next));
                  }}
                  className={`relative w-10 h-6 rounded-full transition-colors shrink-0 ${autoFocusNewBooks ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${autoFocusNewBooks ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              <label className="flex items-start gap-2.5 cursor-pointer px-1">
                <input
                  type="checkbox"
                  checked={updateCheckStartup}
                  onChange={async (e) => {
                    const val = e.target.checked;
                    setUpdateCheckStartup(val);
                    try {
                      await invoke("set_setting_value", {
                        key: "update_check_on_startup",
                        value: val ? "true" : "false",
                      });
                    } catch (err) {
                      setUpdateCheckStartup(!val); // revert — the setting did not persist
                      addToast(friendlyError(err, t), "error");
                    }
                  }}
                  className="mt-0.5 accent-accent"
                />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.updateCheckOnStartup")}
                  <span className="block text-xs text-ink-muted mt-0.5">
                    {t("settings.updateCheckOnStartupHint")}
                  </span>
                </span>
              </label>
            </div>
          </Accordion>

          {/* Theme */}
          <Accordion title={t("settings.appearance")} open={openSection === "appearance"} onToggle={() => toggleSection("appearance")} query={settingsQuery} keywords={t("settings.searchTermsAppearance")}>
            {/* Saved Themes */}
            <div className="pb-4 mb-4 border-b border-warm-border/50">
              <h4 className="text-xs font-semibold uppercase tracking-wider text-ink-muted mb-2">
                {t("settings.savedThemes")}
              </h4>
              <SavedThemesList
                themes={savedThemes}
                activeThemeId={activeThemeId}
                onLoad={handleLoadTheme}
                onSave={handleSaveTheme}
                onDelete={handleDeleteTheme}
                onRename={handleRenameTheme}
              />
            </div>

            <div className="space-y-3">
              {/* Preset mode buttons */}
              <div className="flex gap-1 bg-warm-subtle rounded-xl p-1">
                {(["light", "sepia", "dark", "system"] as const).map((option) => (
                  <button
                    type="button"
                    key={option}
                    onClick={() => { setMode(option); setActiveThemeId(null); }}
                    className={`flex-1 px-2 py-2 text-sm rounded-lg capitalize transition-all duration-150 ${
                      mode === option
                        ? "bg-surface text-ink shadow-sm font-medium"
                        : "text-ink-muted hover:text-ink"
                    }`}
                  >
                    {option === "system" ? t("settings.auto") : option === "light" ? t("settings.light") : option === "sepia" ? t("settings.sepia") : t("settings.dark")}
                  </button>
                ))}
              </div>

              {/* Custom theme toggle */}
              <button
                type="button"
                onClick={() => { setMode(mode === "custom" ? "light" : "custom"); setActiveThemeId(null); }}
                className={`w-full px-3 py-2 text-sm rounded-lg border transition-all duration-150 ${
                  mode === "custom"
                    ? "border-accent bg-accent-light text-ink font-medium"
                    : "border-warm-border text-ink-muted hover:text-ink hover:border-ink-muted"
                }`}
              >
                {t("settings.customColors")}
              </button>

              {/* Custom color editor */}
              {mode === "custom" && (
                <CustomColorEditor
                  customColors={customColors}
                  setCustomColors={setCustomColors}
                />
              )}
            </div>

            {/* Custom CSS */}
            <div className="mt-4 pt-4 border-t border-warm-border/50 space-y-2">
              <label className="text-xs font-medium text-ink-muted mb-1 block">{t("settings.customCss")}</label>
              <textarea
                value={customCss}
                onChange={(e) => setCustomCss(e.target.value)}
                placeholder={`.reader-content p {\n  color: #333;\n}`}
                className="w-full h-28 text-xs font-mono bg-warm-subtle border border-warm-border rounded-lg px-3 py-2 text-ink placeholder-ink-muted/40 focus:outline-none focus:border-accent resize-y"
                spellCheck={false}
              />
              <p className="text-[11px] text-ink-muted leading-relaxed" dangerouslySetInnerHTML={{ __html: t("settings.customCssHint") }} />
              <p className="text-[11px] text-ink-muted/60 leading-relaxed">
                {t("settings.customCssGlobalHint")}
              </p>
              {customCss && (
                <button
                  type="button"
                  onClick={() => setCustomCss("")}
                  className="text-xs text-ink-muted hover:text-ink transition-colors"
                >
                  {t("settings.clearCustomCss")}
                </button>
              )}
            </div>

            {/* Typography */}
            <div className="mt-4 pt-4 border-t border-warm-border/50">
              <h4 className="text-xs font-semibold uppercase tracking-wider text-ink-muted mb-3">
                {t("settings.typography")}
              </h4>

              <div className="flex items-center gap-3">
                <button
                  onClick={() => setFontSize(fontSize - 1)}
                  disabled={fontSize <= MIN_FONT_SIZE}
                  className="w-8 h-8 flex items-center justify-center rounded-lg bg-warm-subtle text-ink-muted hover:text-ink hover:bg-warm-border transition-colors disabled:opacity-50 disabled:cursor-not-allowed text-sm font-medium focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                  aria-label={t("reader.decreaseFontSize")}
                >
                  −
                </button>
                <div className="flex-1 flex flex-col items-center gap-1">
                  <input
                    type="range"
                    min={MIN_FONT_SIZE}
                    max={MAX_FONT_SIZE}
                    value={fontSize}
                    onChange={(e) => setFontSize(Number(e.target.value))}
                    className="w-full accent-accent"
                    aria-label={t("settings.fontSize")}
                    aria-valuetext={`${fontSize} pixels`}
                  />
                  <span className="text-xs text-ink-muted tabular-nums">
                    {fontSize}px
                  </span>
                </div>
                <button
                  onClick={() => setFontSize(fontSize + 1)}
                  disabled={fontSize >= MAX_FONT_SIZE}
                  className="w-8 h-8 flex items-center justify-center rounded-lg bg-warm-subtle text-ink-muted hover:text-ink hover:bg-warm-border transition-colors disabled:opacity-50 disabled:cursor-not-allowed text-sm font-medium focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                  aria-label={t("reader.increaseFontSize")}
                >
                  +
                </button>
              </div>

              {/* Reading font */}
              <div className="mt-4 pt-4 border-t border-warm-border/50">
              <label className="text-xs font-medium text-ink-muted mb-2 block">{t("settings.readingFont")}</label>
              <div className="flex flex-col gap-1">
                {/* Built-in fonts */}
                {FONT_OPTIONS.map((option) => (
                  <button
                    type="button"
                    key={option.key}
                    onClick={() => setFontFamily(option.key)}
                    className={`w-full text-left px-3 py-2 text-sm rounded-lg transition-all duration-150 ${
                      fontFamily === option.key
                        ? "bg-accent-light text-accent font-medium"
                        : "text-ink-muted hover:text-ink hover:bg-warm-subtle"
                    }`}
                    style={{ fontFamily: option.css }}
                  >
                    {option.label}
                  </button>
                ))}

                {/* Custom fonts */}
                {customFonts.map((font) => (
                  <div
                    key={font.id}
                    className={`group flex items-center gap-2 px-3 py-2 rounded-lg transition-all duration-150 cursor-pointer ${
                      fontFamily === `custom:${font.id}`
                        ? "bg-accent-light text-accent font-medium"
                        : "text-ink-muted hover:text-ink hover:bg-warm-subtle"
                    }`}
                    onClick={() => setFontFamily(`custom:${font.id}`)}
                  >
                    <span
                      className="flex-1 text-sm truncate"
                      style={{ fontFamily: `"CustomFont-${font.id}", serif` }}
                    >
                      {font.name}
                    </span>
                    {deletingFontId === font.id ? (
                      <span className="flex items-center gap-1 shrink-0">
                        <button
                          onClick={(e) => { e.stopPropagation(); handleDeleteFont(font.id); }}
                          className="text-[10px] px-1.5 py-0.5 bg-accent text-white rounded hover:bg-accent-hover transition-colors"
                        >
                          {t("common.delete")}
                        </button>
                        <button
                          onClick={(e) => { e.stopPropagation(); setDeletingFontId(null); }}
                          className="text-[10px] px-1.5 py-0.5 text-ink-muted hover:text-ink transition-colors"
                        >
                          {t("common.cancel")}
                        </button>
                      </span>
                    ) : (
                      <button
                        onClick={(e) => { e.stopPropagation(); setDeletingFontId(font.id); }}
                        className="opacity-0 group-hover:opacity-100 p-0.5 text-ink-muted hover:text-red-500 transition-all shrink-0"
                        aria-label={t("common.remove") + " " + font.name}
                      >
                        <svg width="12" height="12" viewBox="0 0 20 20" fill="none">
                          <path d="M15 5L5 15M5 5l10 10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                        </svg>
                      </button>
                    )}
                  </div>
                ))}

                {/* Add font button */}
                <button
                  type="button"
                  onClick={handleImportFont}
                  className="w-full text-left px-3 py-2 text-sm text-accent hover:bg-warm-subtle rounded-lg transition-colors flex items-center gap-2"
                >
                  <svg width="14" height="14" viewBox="0 0 20 20" fill="none">
                    <path d="M10 4v12M4 10h12" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
                  </svg>
                  {t("settings.addFont")}
                </button>
                <p className="px-3 text-[10px] text-ink-muted/60">
                  {t("settings.addFontWarning")}
                </p>
              </div>

              {/* Font preview */}
              <p
                className="mt-3 text-sm text-ink-muted leading-relaxed"
                style={{
                  fontFamily:
                    fontFamily === "serif"
                      ? '"Lora Variable", Georgia, serif'
                      : fontFamily === "literata"
                        ? '"Literata Variable", Georgia, serif'
                        : fontFamily === "dyslexic"
                          ? '"OpenDyslexic", sans-serif'
                          : fontFamily.startsWith("custom:")
                            ? `"CustomFont-${fontFamily.slice(7)}", serif`
                            : '"DM Sans Variable", system-ui, sans-serif',
                }}
              >
                {t("settings.fontPreview")}
              </p>
              </div>

              {/* Typography sliders */}
              <div className="mt-4 pt-4 border-t border-warm-border/50">
              <div className="space-y-4">
                {/* Line height */}
                <div>
                  <div className="flex items-center justify-between mb-1">
                    <label className="text-xs font-medium text-ink-muted">{t("settings.lineHeight")}</label>
                    <span className="text-xs text-ink-muted tabular-nums">{typography.lineHeight.toFixed(1)}</span>
                  </div>
                  <input
                    type="range"
                    min={1.2}
                    max={2.4}
                    step={0.1}
                    value={typography.lineHeight}
                    onChange={(e) => setTypography({ ...typography, lineHeight: parseFloat(e.target.value) })}
                    className="w-full accent-accent"
                  />
                </div>

                {/* Page margins */}
                <div>
                  <div className="flex items-center justify-between mb-1">
                    <label className="text-xs font-medium text-ink-muted">{t("settings.pageMargins")}</label>
                    <span className="text-xs text-ink-muted tabular-nums">{typography.pageMargins}px</span>
                  </div>
                  <input
                    type="range"
                    min={0}
                    max={80}
                    step={4}
                    value={typography.pageMargins}
                    onChange={(e) => setTypography({ ...typography, pageMargins: parseInt(e.target.value, 10) })}
                    className="w-full accent-accent"
                  />
                </div>

                {/* Paragraph spacing */}
                <div>
                  <div className="flex items-center justify-between mb-1">
                    <label className="text-xs font-medium text-ink-muted">{t("settings.paragraphSpacing")}</label>
                    <span className="text-xs text-ink-muted tabular-nums">{typography.paragraphSpacing.toFixed(1)}em</span>
                  </div>
                  <input
                    type="range"
                    min={0}
                    max={2}
                    step={0.1}
                    value={typography.paragraphSpacing}
                    onChange={(e) => setTypography({ ...typography, paragraphSpacing: parseFloat(e.target.value) })}
                    className="w-full accent-accent"
                  />
                </div>

                {/* Text alignment */}
                <div>
                  <label className="text-xs font-medium text-ink-muted mb-1 block">{t("settings.textAlignment")}</label>
                  <div className="flex gap-1 bg-warm-subtle rounded-xl p-1">
                    {(["left", "justify"] as const).map((option) => (
                      <button
                        type="button"
                        key={option}
                        onClick={() => setTypography({ ...typography, textAlign: option })}
                        className={`flex-1 px-3 py-2 text-sm rounded-lg capitalize transition-all duration-150 ${
                          typography.textAlign === option
                            ? "bg-surface text-ink shadow-sm font-medium"
                            : "text-ink-muted hover:text-ink"
                        }`}
                      >
                        {option === "left" ? t("settings.left") : t("settings.justify")}
                      </button>
                    ))}
                  </div>
                </div>

                {/* Hyphenation */}
                <div>
                  <label className="flex items-center justify-between cursor-pointer">
                    <span className="text-xs font-medium text-ink-muted">{t("settings.hyphenation")}</span>
                    <button
                      type="button"
                      onClick={() => setTypography({ ...typography, hyphenation: !typography.hyphenation })}
                      className={`relative w-9 h-5 rounded-full transition-colors duration-200 ${typography.hyphenation ? "bg-accent" : "bg-warm-border"}`}
                    >
                      <span className={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white shadow transition-transform duration-200 ${typography.hyphenation ? "translate-x-4" : ""}`} />
                    </button>
                  </label>
                  <p className="text-[11px] text-ink-muted/60 mt-1">{t("settings.hyphenationHint")}</p>
                </div>
              </div>
              </div>
            </div>
          </Accordion>

          {/* Page Layout */}
          <Accordion title={t("settings.pageLayout")} open={openSection === "layout"} onToggle={() => toggleSection("layout")} query={settingsQuery} keywords={t("settings.searchTermsLayout")}>
            <div className="flex gap-1 bg-warm-subtle rounded-xl p-1">
              {(["paginated", "continuous"] as const).map((option) => (
                <button
                  type="button"
                  key={option}
                  onClick={() => setScrollMode(option)}
                  className={`flex-1 px-3 py-2 text-sm rounded-lg capitalize transition-all duration-150 ${
                    scrollMode === option
                      ? "bg-surface text-ink shadow-sm font-medium"
                      : "text-ink-muted hover:text-ink"
                  }`}
                >
                  {option === "paginated" ? t("settings.paginated") : t("settings.continuous")}
                </button>
              ))}
            </div>
            <p className="mt-2 text-xs text-ink-muted">
              {scrollMode === "continuous"
                ? t("reader.continuousDescription")
                : t("reader.paginatedDescription")}
            </p>

            {/* Dual-page spread */}
            <div className="mt-4 pt-4 border-t border-warm-border/50 space-y-4">
              <label className="flex items-center justify-between gap-3">
                <div>
                  <span className="text-sm text-ink">{t("settings.dualPageSpread")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.dualPageHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={dualPage}
                  onClick={() => setDualPage(!dualPage)}
                  className={`relative w-10 h-6 rounded-full transition-colors ${dualPage ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${dualPage ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>

              {/* Manga mode toggle */}
              <label className={`flex items-center justify-between gap-3 ${!dualPage ? "opacity-40 pointer-events-none" : ""}`}>
                <div>
                  <span className="text-sm text-ink">{t("settings.mangaMode")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.mangaHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={mangaMode}
                  aria-disabled={!dualPage}
                  onClick={() => dualPage && setMangaMode(!mangaMode)}
                  className={`relative w-10 h-6 rounded-full transition-colors ${mangaMode && dualPage ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${mangaMode && dualPage ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
              {/* Page turn animation toggle */}
              <label className="flex items-center justify-between gap-3">
                <div>
                  <span className="text-sm text-ink">{t("settings.pageAnimation")}</span>
                  <p className="text-[11px] text-ink-muted/60 mt-0.5">{t("settings.pageAnimationHint")}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={pageAnimation}
                  onClick={() => setPageAnimation(!pageAnimation)}
                  className={`relative w-10 h-6 rounded-full transition-colors ${pageAnimation ? "bg-accent" : "bg-warm-border"}`}
                >
                  <span
                    className={`absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform ${pageAnimation ? "translate-x-4" : ""}`}
                  />
                </button>
              </label>
            </div>
          </Accordion>

          {/* Library */}
          <Accordion title={t("settings.librarySection")} open={openSection === "library"} onToggle={() => toggleSection("library")} query={settingsQuery} keywords={t("settings.searchTermsLibrary")}>
            <div className="space-y-2">
              <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
                <p className="text-xs text-ink-muted mb-0.5">{t("settings.storageFolder")}</p>
                <p className="text-sm text-ink break-all leading-snug font-mono">
                  {libraryFolder ?? "—"}
                </p>
                {libraryInfo && (
                  <p className="text-xs text-ink-muted mt-1.5">
                    {t("settings.importedCount", { count: libraryInfo.file_count })} · {formatBytes(libraryInfo.total_size_bytes)}
                    {libraryInfo.linked_count > 0 && <> · {t("settings.linkedCount", { count: libraryInfo.linked_count })}</>}
                  </p>
                )}
              </div>
              <button
                onClick={handleChangeFolder}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
              >
                {t("settings.changeFolder")}
              </button>
              <button
                onClick={() => setCleanupState("confirm")}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
              >
                {t("settings.checkMissingFiles")}
              </button>
              <button
                type="button"
                onClick={() => setShowActivityLog(true)}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
              >
                {t("settings.viewActivityLog")}
              </button>
              <button
                type="button"
                onClick={() => {
                  onClose();
                  restartOnboarding();
                }}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
              >
                {t("settings.rerunWizard")}
              </button>

              <div className="mt-3 pt-3 border-t border-warm-border/50">
                <label className="text-xs font-medium text-ink-muted mb-2 block">{t("settings.importMode")}</label>
                <div className="flex gap-1 bg-warm-subtle rounded-xl p-1">
                  {(["import", "link"] as const).map((option) => (
                    <button
                      type="button"
                      key={option}
                      onClick={async () => {
                        setImportMode(option);
                        await invoke("set_setting_value", { key: "import_mode", value: option });
                      }}
                      className={`flex-1 px-3 py-2 text-sm rounded-lg transition-all duration-150 ${
                        importMode === option
                          ? "bg-surface text-ink shadow-sm font-medium"
                          : "text-ink-muted hover:text-ink"
                      }`}
                    >
                      {option === "import" ? t("settings.importModeCopy") : t("settings.importModeLink")}
                    </button>
                  ))}
                </div>
                <p className="mt-2 text-xs text-ink-muted">{t("settings.importModeHelp")}</p>
              </div>

              {/* Page Cache */}
              <div className="mt-3 pt-3 border-t border-warm-border/50">
                <label className="text-xs font-medium text-ink-muted mb-2 block">{t("settings.pageCacheSection")}</label>

                <label className="block text-xs text-ink-muted mb-1">
                  {t("settings.pageCacheLimit")}
                </label>
                <select
                  value={pageCacheLimit}
                  onChange={(e) => handleCacheLimitChange(e.target.value)}
                  className="w-full rounded-xl border border-warm-border bg-warm-subtle px-3 py-2 text-sm text-ink"
                >
                  <option value="250">250 MB</option>
                  <option value="500">500 MB</option>
                  <option value="1024">1 GB</option>
                  <option value="2048">2 GB</option>
                </select>
                <p className="mt-1 text-xs text-ink-muted">{t("settings.pageCacheLimitHelp")}</p>

                <div className="mt-2 text-sm text-ink-muted">
                  {diskCacheSection && diskCacheSection.entry_count > 0
                    ? t("settings.pageCacheUsage", {
                        size: formatBytes(diskCacheSection.total_bytes ?? 0),
                        count: diskCacheSection.entry_count,
                      })
                    : t("settings.pageCacheUsageEmpty")}
                </div>

                {diskCacheSection && diskCacheSection.entry_count > 0 && (
                  <button
                    onClick={handleClearCache}
                    className="w-full mt-2 px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
                  >
                    {t("settings.clearPageCache")}
                  </button>
                )}

                {cacheStats && (
                  <div className="mt-3 pt-3 border-t border-warm-border/50">
                    <ul className="space-y-1">
                      {cacheStats.sections.map((s) => (
                        <li key={s.name} className="flex items-center justify-between text-xs text-ink-muted">
                          <span>
                            {cacheSectionLabel(s.name)}
                            <span className="ml-1.5 rounded bg-warm-subtle px-1.5 py-0.5">
                              {s.kind === "memory" ? t("settings.cacheKindMemory") : t("settings.cacheKindDisk")}
                            </span>
                          </span>
                          <span>
                            {t("settings.cacheEntries", { count: s.entry_count })}
                            {s.total_bytes !== null ? ` · ${formatBytes(s.total_bytes)}` : " · —"}
                          </span>
                        </li>
                      ))}
                    </ul>
                    <div className="mt-2 text-xs text-ink-muted">
                      {t("settings.cacheTotal", { size: formatBytes(cacheStats.total_bytes) })}
                    </div>
                    <button
                      onClick={handleClearAllCaches}
                      className="w-full mt-2 px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
                    >
                      {t("settings.clearAllCaches")}
                    </button>
                  </div>
                )}
              </div>
            </div>
          </Accordion>

          {/* Backup & Restore */}
          <Accordion title={t("settings.backupRestore")} open={openSection === "backup"} onToggle={() => toggleSection("backup")} query={settingsQuery} keywords={t("settings.searchTermsBackup")}>
            <div className="space-y-2">
              <label className="flex items-start gap-2.5 cursor-pointer px-1">
                <input
                  type="checkbox"
                  checked={includeFiles}
                  onChange={(e) => setIncludeFiles(e.target.checked)}
                  disabled={exporting}
                  className="mt-0.5 accent-accent"
                />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.includeBookFiles")}
                  <span className="block text-xs text-ink-muted mt-0.5">
                    {includeFiles
                      ? t("settings.fullBackup")
                      : t("settings.metadataOnly")}
                  </span>
                </span>
              </label>
              <button
                onClick={handleExport}
                disabled={exporting}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left disabled:opacity-40"
              >
                {exporting ? t("common.working") : t("settings.exportLibrary")}
              </button>
              <button
                onClick={handleOpenRestoreModal}
                disabled={exporting}
                className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left disabled:opacity-40"
              >
                {t("settings.restoreFromBackup")}
              </button>
              {backupMessage && (
                <p className="text-xs text-ink-muted px-1">{backupMessage}</p>
              )}
            </div>
          </Accordion>

          {/* Metadata Scan */}
          <Accordion title={t("settings.metadataScan")} open={openSection === "scan"} onToggle={() => toggleSection("scan")} query={settingsQuery} keywords={t("settings.searchTermsScan")}>
            <div className="space-y-2">
              <label className="flex items-start gap-2.5 cursor-pointer px-1">
                <input type="checkbox" checked={autoScanImport}
                  onChange={async (e) => {
                    const val = e.target.checked;
                    setAutoScanImport(val);
                    try {
                      await invoke("set_setting_value", { key: "auto_scan_import", value: val ? "true" : "false" });
                    } catch (err) {
                      setAutoScanImport(!val); // revert — the setting did not persist
                      addToast(friendlyError(err, t), "error");
                    }
                  }}
                  className="mt-0.5 accent-accent" />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.autoScanImport")}
                  <span className="block text-xs text-ink-muted mt-0.5">{t("settings.autoScanImportHint")}</span>
                </span>
              </label>
              <label className="flex items-start gap-2.5 cursor-pointer px-1">
                <input type="checkbox" checked={autoScanStartup}
                  onChange={async (e) => {
                    const val = e.target.checked;
                    setAutoScanStartup(val);
                    try {
                      await invoke("set_setting_value", { key: "auto_scan_startup", value: val ? "true" : "false" });
                    } catch (err) {
                      setAutoScanStartup(!val); // revert — the setting did not persist
                      addToast(friendlyError(err, t), "error");
                    }
                  }}
                  className="mt-0.5 accent-accent" />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.autoScanStartup")}
                  <span className="block text-xs text-ink-muted mt-0.5">{t("settings.autoScanStartupHint")}</span>
                </span>
              </label>
              {enrichmentProviders.length > 0 && (
                <div className="mt-3">
                  <h4 className="text-xs font-medium text-ink-muted mb-1">{t("settings.enrichmentSources")}</h4>
                  <p className="text-[10px] text-ink-muted mb-2">{t("settings.enrichmentSourcesOrder")}</p>
                  {enrichmentProviders.map((provider, index) => (
                    <div key={provider.id} className="flex items-start gap-2 py-2 border-b border-warm-border last:border-0">
                      <div className="flex flex-col items-center gap-0.5 mt-0.5">
                        <button
                          onClick={async () => {
                            if (index === 0) return;
                            const reordered = [...enrichmentProviders];
                            [reordered[index - 1], reordered[index]] = [reordered[index], reordered[index - 1]];
                            setEnrichmentProviders(reordered);
                            await invoke("set_enrichment_provider_order", {
                              order: reordered.map((p) => p.id),
                            }).catch(() => {});
                          }}
                          disabled={index === 0}
                          className="text-[10px] leading-none text-ink-muted hover:text-ink disabled:opacity-30 disabled:cursor-not-allowed"
                          aria-label={`Move ${provider.name} up`}
                        >
                          ▲
                        </button>
                        <button
                          onClick={async () => {
                            if (index === enrichmentProviders.length - 1) return;
                            const reordered = [...enrichmentProviders];
                            [reordered[index], reordered[index + 1]] = [reordered[index + 1], reordered[index]];
                            setEnrichmentProviders(reordered);
                            await invoke("set_enrichment_provider_order", {
                              order: reordered.map((p) => p.id),
                            }).catch(() => {});
                          }}
                          disabled={index === enrichmentProviders.length - 1}
                          className="text-[10px] leading-none text-ink-muted hover:text-ink disabled:opacity-30 disabled:cursor-not-allowed"
                          aria-label={`Move ${provider.name} down`}
                        >
                          ▼
                        </button>
                      </div>
                      <input
                        type="checkbox"
                        checked={provider.config.enabled}
                        onChange={async (e) => {
                          await invoke("set_enrichment_provider_config", {
                            providerId: provider.id,
                            enabled: e.target.checked,
                            apiKey: provider.config.apiKey,
                          }).catch(() => {});
                          loadProviders();
                        }}
                        className="mt-0.5 accent-accent"
                      />
                      <div className="flex-1 min-w-0">
                        <span className="text-sm text-ink">{provider.name}</span>
                        {provider.apiKeyHelp && (
                          <div className="mt-1">
                            <input
                              type="text"
                              value={provider.config.apiKey ?? ""}
                              onChange={(e) => {
                                setEnrichmentProviders((prev) =>
                                  prev.map((p) =>
                                    p.id === provider.id
                                      ? { ...p, config: { ...p.config, apiKey: e.target.value } }
                                      : p
                                  )
                                );
                              }}
                              onBlur={async (e) => {
                                await invoke("set_enrichment_provider_config", {
                                  providerId: provider.id,
                                  enabled: provider.config.enabled,
                                  apiKey: e.target.value || null,
                                }).catch(() => {});
                              }}
                              placeholder={t("settings.apiKeyPlaceholder")}
                              className="w-full text-xs bg-warm-subtle border border-warm-border rounded px-2 py-1 text-ink placeholder-ink-muted/50 focus:outline-none focus:border-accent"
                            />
                            <p className="text-[10px] text-ink-muted mt-0.5">{provider.apiKeyHelp}</p>
                          </div>
                        )}
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </Accordion>

          <Accordion title={t("settings.dictionarySection")} open={openSection === "dictionary"} onToggle={() => toggleSection("dictionary")} query={settingsQuery} keywords={t("settings.searchTermsDictionary")}>
            <div className="space-y-3">
              <label className="flex items-start gap-2.5 cursor-pointer px-1">
                <input type="checkbox" checked={dictEnabled}
                  onChange={(e) => handleToggleDictionary(e.target.checked)}
                  className="mt-0.5 accent-accent" />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.dictionaryEnable")}
                  <span className="block text-xs text-ink-muted mt-0.5">{t("settings.dictionaryEnableHint")}</span>
                </span>
              </label>

              {dictEnabled && (
                <div className="px-1 space-y-2">
                  {(dictState.phase === "missing" || dictState.phase === "corrupt" || dictState.phase === "unknown") && (
                    <div className="flex items-center gap-2 flex-wrap">
                      {dictState.phase === "corrupt" && (
                        <p className="w-full text-xs text-red-500">{t("settings.dictionaryCorrupt")}</p>
                      )}
                      <button
                        onClick={handleDownloadDictionary}
                        disabled={dictState.phase === "unknown"}
                        className="shrink-0 px-3 py-1.5 text-xs bg-accent text-white rounded-lg hover:bg-accent-hover transition-colors font-medium disabled:opacity-50"
                      >
                        {t("settings.dictionaryDownload")}
                      </button>
                      <span className="text-xs text-ink-muted">{t("settings.dictionaryDownloadHint")}</span>
                    </div>
                  )}

                  {dictState.phase === "downloading" && (
                    <div>
                      <div className="h-2 bg-warm-border rounded overflow-hidden">
                        <div
                          className="h-full bg-accent transition-[width] duration-200"
                          style={{ width: `${downloadPercent(dictState) ?? 15}%` }}
                        />
                      </div>
                      <p className="text-xs text-ink-muted mt-1">
                        {isVerifying(dictState) ? t("settings.dictionaryVerifying") : t("settings.dictionaryDownloading")}
                      </p>
                    </div>
                  )}

                  {dictState.phase === "error" && (
                    <div className="flex items-center gap-2 flex-wrap">
                      <p className="w-full text-xs text-red-500">{dictState.error}</p>
                      <button
                        onClick={handleDownloadDictionary}
                        className="shrink-0 px-3 py-1.5 text-xs bg-accent text-white rounded-lg hover:bg-accent-hover transition-colors font-medium"
                      >
                        {t("settings.dictionaryRetry")}
                      </button>
                    </div>
                  )}

                  {dictState.phase === "ready" && (
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-xs text-ink-muted">
                        {t("settings.dictionaryReady", {
                          version: dictState.wordnetVersion ?? "",
                          size: formatBytes(dictState.sizeBytes),
                        })}
                      </span>
                      <button
                        onClick={() => setDictDeleteConfirm(true)}
                        className="shrink-0 px-3 py-1.5 text-xs bg-warm-subtle text-ink rounded-lg hover:bg-warm-border transition-colors font-medium"
                      >
                        {t("settings.dictionaryDelete")}
                      </button>
                    </div>
                  )}

                  <p className="text-[10px] text-ink-muted">{t("settings.dictionaryAttribution")}</p>
                </div>
              )}

              <label className="flex items-start gap-2.5 cursor-pointer px-1 pt-1">
                <input type="checkbox" checked={vocabEnabled}
                  onChange={(e) => handleToggleVocabulary(e.target.checked)}
                  className="mt-0.5 accent-accent" />
                <span className="text-sm text-ink leading-snug">
                  {t("settings.vocabularyEnable")}
                  <span className="block text-xs text-ink-muted mt-0.5">{t("settings.vocabularyEnableHint")}</span>
                </span>
              </label>

              {dictDeleteConfirm && (
                <ConfirmDialog
                  title={t("settings.dictionaryDeleteConfirmTitle")}
                  message={t("settings.dictionaryDeleteConfirmMessage")}
                  confirmLabel={t("settings.dictionaryDelete")}
                  onConfirm={handleDeleteDictionary}
                  onCancel={() => setDictDeleteConfirm(false)}
                />
              )}
            </div>
          </Accordion>

          <Accordion title={t("settings.remoteAccess")} open={openSection === "webserver"} onToggle={() => toggleSection("webserver")} query={settingsQuery} keywords={t("settings.searchTermsWebserver")}>
            <div className="space-y-2">
              {/* PIN input (R4-2: save feedback, R4-1: guard) */}
              <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
                <label htmlFor="web-server-pin" className="text-xs text-ink-muted mb-1 block">{t("settings.pin")}</label>
                <input
                  id="web-server-pin"
                  type="password"
                  value={webServerPin}
                  onChange={(e) => {
                    const v = e.target.value;
                    setWebServerPin(v);
                    setPinSaved(false);
                    setPinError(null);
                  }}
                  onBlur={() => {
                    const isValid = getPinStrength(webServerPin).error === null;
                    if (shouldSaveOnBlur(webServerPin, savedPin, isValid)) {
                      savePin();
                    }
                  }}
                  placeholder={t("settings.pinPlaceholder")}
                  maxLength={8}
                  className="w-full bg-transparent text-sm text-ink focus:outline-none"
                />
                {webServerPin.length > 0 && (() => {
                  const { strength, error } = getPinStrength(webServerPin);
                  const colors = { weak: "bg-red-400", fair: "bg-yellow-400", strong: "bg-green-400" };
                  const widths = { weak: "w-1/3", fair: "w-2/3", strong: "w-full" };
                  return (
                    <div className="mt-1.5">
                      <div className="h-1 w-full bg-warm-border rounded-full overflow-hidden">
                        <div className={`h-full rounded-full transition-all ${colors[strength]} ${widths[strength]}`} />
                      </div>
                      <div className="flex items-center justify-between mt-0.5">
                        <span className={`text-[10px] ${strength === "weak" ? "text-red-400" : strength === "fair" ? "text-yellow-500" : "text-green-500"}`}>
                          {t(`settings.pinStrength.${strength}`)}
                        </span>
                        {error && <span className="text-[10px] text-red-400">{t(error)}</span>}
                      </div>
                    </div>
                  );
                })()}
                <div className="flex items-center gap-2 mt-1">
                  {webServerPin && (
                    <button
                      type="button"
                      onClick={() => savePin()}
                      className="text-xs text-accent hover:underline"
                    >
                      {t("settings.savePin")}
                    </button>
                  )}
                  {isPinUnsaved(webServerPin, savedPin) && !pinError && (
                    <span className="text-xs text-yellow-500">{t("settings.pinUnsaved")}</span>
                  )}
                  {pinSaved && <span className="text-xs text-green-400">{t("settings.pinSaved")}</span>}
                  {pinError && <span className="text-xs text-red-400">{pinError}</span>}
                </div>
              </div>

              {/* Port input */}
              <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
                <label htmlFor="web-server-port" className="text-xs text-ink-muted mb-1 block">{t("settings.port")}</label>
                <input
                  type="number"
                  value={webServerPort}
                  onChange={(e) => {
                    const v = e.target.value;
                    setWebServerPort(v);
                    // Surface the valid range inline instead of silently
                    // clamping; clear once the value is back in range.
                    setWebServerPortError(
                      v.trim() === "" || validateWebServerPort(v).valid
                        ? null
                        : t("settings.portRange", {
                            min: WEB_SERVER_PORT_MIN,
                            max: WEB_SERVER_PORT_MAX,
                          })
                    );
                  }}
                  onBlur={() => {
                    const result = validateWebServerPort(webServerPort);
                    if (!result.valid) {
                      setWebServerPortError(
                        t("settings.portRange", {
                          min: WEB_SERVER_PORT_MIN,
                          max: WEB_SERVER_PORT_MAX,
                        })
                      );
                      return;
                    }
                    setWebServerPortError(null);
                    if (webUiEnabled || opdsEnabled) {
                      handleSetModes({ port: result.port });
                    }
                  }}
                  className="w-full bg-transparent text-sm text-ink focus:outline-none"
                  id="web-server-port"
                  aria-invalid={webServerPortError ? true : undefined}
                  aria-describedby={webServerPortError ? "web-server-port-error" : undefined}
                  min={WEB_SERVER_PORT_MIN}
                  max={WEB_SERVER_PORT_MAX}
                />
                {webServerPortError && (
                  <p id="web-server-port-error" className="text-xs text-red-600 dark:text-red-400 mt-1">
                    {webServerPortError}
                  </p>
                )}
              </div>

              {/* Web UI / OPDS toggles. Server runs iff at least one is on. */}
              <div className="space-y-2 px-1">
                <label className="flex items-start gap-2.5 cursor-pointer">
                  <input
                    type="checkbox"
                    checked={webUiEnabled}
                    onChange={(e) => handleSetModes({ webUi: e.target.checked })}
                    disabled={webServerLoading}
                    className="mt-0.5 accent-accent"
                  />
                  <span className="flex flex-col gap-0.5">
                    <span className="text-sm text-ink">{t("settings.remoteAccessDescriptions.webUiCheckbox")}</span>
                    <span className="text-xs text-ink-muted">{t("settings.remoteAccessDescriptions.webUiHint")}</span>
                  </span>
                </label>
                <label className="flex items-start gap-2.5 cursor-pointer">
                  <input
                    type="checkbox"
                    checked={opdsEnabled}
                    onChange={(e) => handleSetModes({ opds: e.target.checked })}
                    disabled={webServerLoading}
                    className="mt-0.5 accent-accent"
                  />
                  <span className="flex flex-col gap-0.5">
                    <span className="text-sm text-ink">{t("settings.remoteAccessDescriptions.opdsCheckbox")}</span>
                    <span className="text-xs text-ink-muted">{t("settings.remoteAccessDescriptions.opdsHint")}</span>
                  </span>
                </label>
              </div>
              {/* Status + R1-5: Copy URL */}
              {webServerRunning && webServerUrl && (
                <div className="bg-warm-subtle rounded-xl px-3 py-2.5 space-y-2">
                  <div className="flex items-center gap-2">
                    <p className="text-xs text-ink-muted flex-1">
                      {t("settings.serverRunningAt")}{" "}
                      <a href={webServerUrl} target="_blank" rel="noopener noreferrer" className="text-accent hover:underline">
                        {webServerUrl}
                      </a>
                    </p>
                    <button
                      type="button"
                      onClick={() => {
                        navigator.clipboard.writeText(webServerUrl);
                      }}
                      className="text-xs text-accent hover:underline shrink-0"
                    >
                      {t("settings.copyUrl")}
                    </button>
                  </div>
                  {webServerQr && (
                    <div className="flex justify-center" dangerouslySetInnerHTML={{ __html: webServerQr }} />
                  )}
                </div>
              )}

              {webServerError && (
                <p className="text-xs text-red-500 px-1">{webServerError}</p>
              )}
            </div>
          </Accordion>

          <Accordion title={t("settings.profileLockSection")} open={openSection === "profileLock"} onToggle={() => toggleSection("profileLock")} query={settingsQuery} keywords={t("settings.searchTermsProfileLock")}>
            <div className="space-y-2">
              <p className="text-xs text-ink-muted px-1">{t("settings.profileLockDescription")}</p>
              <div className="bg-warm-subtle rounded-xl px-3 py-2.5 space-y-2">
                <p className="text-xs text-ink-muted leading-relaxed">{t("settings.profileLockHonestSentence")}</p>
                <p className="text-xs text-ink">
                  {t("settings.profileLockFor", { name: profileLockName })}{" "}
                  <span className={profileLockStatus?.locked ? "text-accent" : "text-ink-muted"}>
                    {profileLockStatus?.locked ? t("settings.profileLockStatusOn") : t("settings.profileLockStatusOff")}
                  </span>
                </p>
              </div>

              {profileLockStatus && !profileLockStatus.locked && (
                <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
                  <label htmlFor="new-lock-password" className="text-xs text-ink-muted mb-1 block">
                    {t("settings.newLockPasswordLabel")}
                  </label>
                  <input
                    id="new-lock-password"
                    type="password"
                    value={newLockPassword}
                    onChange={(e) => setNewLockPassword(e.target.value)}
                    placeholder={t("settings.newLockPasswordPlaceholder")}
                    className="w-full bg-transparent text-sm text-ink focus:outline-none"
                  />
                  <button
                    type="button"
                    onClick={handleSetProfileLock}
                    disabled={!newLockPassword || lockBusy}
                    className="mt-1.5 text-xs text-accent hover:underline disabled:opacity-40 disabled:cursor-not-allowed"
                  >
                    {t("settings.setLockButton")}
                  </button>
                </div>
              )}

              {profileLockStatus?.locked && (
                <div className="bg-warm-subtle rounded-xl px-3 py-2.5 space-y-2">
                  <div>
                    <label htmlFor="current-lock-password" className="text-xs text-ink-muted mb-1 block">
                      {t("settings.currentLockPasswordLabel")}
                    </label>
                    <input
                      id="current-lock-password"
                      type="password"
                      value={currentLockPassword}
                      onChange={(e) => setCurrentLockPassword(e.target.value)}
                      className="w-full bg-transparent text-sm text-ink focus:outline-none"
                    />
                  </div>
                  <div>
                    <label htmlFor="change-lock-password" className="text-xs text-ink-muted mb-1 block">
                      {t("settings.changeLockPasswordLabel")}
                    </label>
                    <input
                      id="change-lock-password"
                      type="password"
                      value={changeLockPassword}
                      onChange={(e) => setChangeLockPassword(e.target.value)}
                      placeholder={t("settings.changeLockPasswordPlaceholder")}
                      className="w-full bg-transparent text-sm text-ink focus:outline-none"
                    />
                  </div>
                  <div className="flex items-center gap-3 pt-0.5">
                    <button
                      type="button"
                      onClick={handleChangeProfileLock}
                      disabled={!currentLockPassword || !changeLockPassword || lockBusy}
                      className="text-xs text-accent hover:underline disabled:opacity-40 disabled:cursor-not-allowed"
                    >
                      {t("settings.changeLockButton")}
                    </button>
                    <button
                      type="button"
                      onClick={() => setPendingRemoveLock(true)}
                      disabled={!currentLockPassword || lockBusy}
                      className="text-xs text-red-500 hover:underline disabled:opacity-40 disabled:cursor-not-allowed"
                    >
                      {t("settings.removeLockButton")}
                    </button>
                  </div>
                </div>
              )}
            </div>
          </Accordion>

          {pendingRemoveLock && (
            <ConfirmDialog
              title={t("settings.removeLockConfirmTitle")}
              message={t("settings.removeLockConfirmMessage")}
              confirmLabel={t("settings.removeLockConfirm")}
              confirmDisabled={lockBusy}
              onConfirm={handleRemoveProfileLock}
              onCancel={() => setPendingRemoveLock(false)}
            />
          )}

          {backupProviders.length > 0 && (
            <Accordion title={t("settings.remoteBackup")} open={openSection === "remote"} onToggle={() => toggleSection("remote")} query={settingsQuery} keywords={t("settings.searchTermsRemote")}>
              <div className="space-y-2">
                {/* Provider selector */}
                <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
                  <label className="text-xs text-ink-muted mb-1 block">{t("settings.provider")}</label>
                  <select
                    value={selectedProvider}
                    onChange={(e) => handleProviderChange(e.target.value)}
                    className="w-full bg-transparent text-sm text-ink focus:outline-none cursor-pointer"
                  >
                    {backupProviders.map((p) => (
                      <option key={p.providerType} value={p.providerType}>
                        {p.label}
                      </option>
                    ))}
                  </select>
                </div>

                {/* Dynamic config fields */}
                {currentProviderInfo && currentProviderInfo.fields.map((field) => {
                  if (field.fieldType === "checkbox") {
                    return (
                      <label key={field.key} className="flex items-center gap-2.5 cursor-pointer px-1">
                        <input
                          type="checkbox"
                          checked={backupFieldValues[field.key] === "true"}
                          onChange={(e) =>
                            updateBackupField(field.key, e.target.checked ? "true" : "false")
                          }
                          className="accent-accent"
                        />
                        <span className="text-sm text-ink">{field.label}</span>
                      </label>
                    );
                  }
                  return (
                    <div key={field.key} className="bg-warm-subtle rounded-xl px-3 py-2.5">
                      <label className="text-xs text-ink-muted mb-1 block">
                        {field.label}
                        {field.required && (
                          <span className="text-accent ml-1">*</span>
                        )}
                      </label>
                      <input
                        type={field.fieldType === "password" ? "password" : "text"}
                        value={backupFieldValues[field.key] ?? ""}
                        onChange={(e) => updateBackupField(field.key, e.target.value)}
                        placeholder={field.placeholder}
                        className="w-full bg-transparent text-sm text-ink placeholder:text-ink-muted/50 focus:outline-none"
                      />
                    </div>
                  );
                })}

                {/* Save and Test connection — two distinct actions, each
                    with its own feedback so a save failure can be told apart
                    from a connection failure. */}
                <div className="flex gap-2">
                  <button
                    onClick={handleSaveBackupConfig}
                    disabled={savingBackupConfig || !selectedProvider}
                    className="flex-1 px-3 py-2 text-sm font-medium bg-accent text-surface rounded-xl hover:opacity-90 transition-opacity disabled:opacity-40"
                  >
                    {savingBackupConfig ? t("common.saving") : t("common.save")}
                  </button>
                  <button
                    onClick={handleTestConnection}
                    disabled={testingConnection || !selectedProvider}
                    className="flex-1 px-3 py-2 text-sm font-medium text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors disabled:opacity-40 flex items-center justify-center gap-2"
                  >
                    {testingConnection && (
                      <svg className="animate-spin w-3.5 h-3.5 shrink-0" viewBox="0 0 24 24" fill="none">
                        <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                        <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
                      </svg>
                    )}
                    {testingConnection ? t("settings.testingConnection") : t("settings.testConnection")}
                  </button>
                </div>

                {/* Save feedback — distinct from connection feedback */}
                {saveFeedback && (
                  <p className={`text-xs px-1 ${saveFeedback.isError ? "text-red-500" : "text-green-600"}`}>
                    {saveFeedback.text}
                  </p>
                )}

                {/* Test connection feedback — distinct from save feedback */}
                {testFeedback && (
                  <p className={`text-xs px-1 ${testFeedback.isError ? "text-red-500" : "text-green-600"}`}>
                    {testFeedback.text}
                  </p>
                )}

                {/* Backup now — only shown when a config has been saved */}
                {savedBackupConfig && (
                  <button
                    onClick={handleRunBackup}
                    disabled={runningBackup}
                    className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left disabled:opacity-40 flex items-center gap-2"
                  >
                    {runningBackup && (
                      <svg
                        className="animate-spin w-3.5 h-3.5 shrink-0"
                        viewBox="0 0 24 24"
                        fill="none"
                      >
                        <circle
                          className="opacity-25"
                          cx="12" cy="12" r="10"
                          stroke="currentColor"
                          strokeWidth="4"
                        />
                        <path
                          className="opacity-75"
                          fill="currentColor"
                          d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z"
                        />
                      </svg>
                    )}
                    {runningBackup ? (backupProgressText || t("settings.backingUp")) : t("settings.backupNow")}
                  </button>
                )}

                {/* Last backup timestamp */}
                {backupStatus && (
                  <p className="text-xs text-ink-muted px-1">
                    {t("settings.lastBackup")}{" "}
                    {new Date(backupStatus.lastSyncAt * 1000).toLocaleString()}
                    {" · "}
                    {t("settings.device")} {backupStatus.deviceId}
                  </p>
                )}

                {/* Status messages (backup run progress / result) */}
                {remoteBackupMessage && (
                  <p className="text-xs px-1 text-ink-muted">
                    {remoteBackupMessage}
                  </p>
                )}

                {/* Sync toggle */}
                <div className="bg-warm-subtle rounded-xl px-3 py-2.5 mt-2">
                  <label className="flex items-center justify-between cursor-pointer">
                    <div>
                      <span className="text-sm font-medium text-ink">Sync reading progress across devices</span>
                      <p className="text-xs text-ink-muted mt-0.5">
                        {savedBackupConfig
                          ? "Syncs reading progress, bookmarks, and highlights across devices using your configured remote backup destination. Does not sync book files."
                          : "Configure a remote backup destination to enable sync."}
                      </p>
                    </div>
                    <input
                      id="sync-toggle"
                      type="checkbox"
                      checked={syncEnabled}
                      disabled={!savedBackupConfig}
                      onChange={(e) => handleToggleSync(e.target.checked)}
                      className="ml-3 h-4 w-4 rounded accent-accent"
                      aria-label={!savedBackupConfig ? "Sync disabled: configure a remote backup destination first" : "Sync reading progress across devices"}
                    />
                  </label>

                  {syncEnabled && savedBackupConfig && (
                    <div className="mt-2 text-xs text-ink-muted space-y-0.5">
                      {lastSyncSuccess ? (
                        <p>Last successful sync: {new Date(lastSyncSuccess * 1000).toLocaleString()}</p>
                      ) : (
                        <p>Sync will run automatically when you open a book</p>
                      )}
                      {lastSyncError && (!lastSyncSuccess || lastSyncError.at > lastSyncSuccess) && (
                        <p className="text-red-500">Last sync error: {lastSyncError.message}</p>
                      )}
                    </div>
                  )}
                </div>
              </div>
            </Accordion>
          )}

          <Accordion title={t("plugins.section")} open={openSection === "plugins"} onToggle={() => toggleSection("plugins")} query={settingsQuery} keywords={t("settings.searchTermsPlugins")}>
            <PluginsPanel onToast={(msg, type) => addToast(msg, type ?? "error")} />
          </Accordion>

          <Accordion title={t("settings.aboutSection")} open={openSection === "about"} onToggle={() => toggleSection("about")} query={settingsQuery} keywords={t("settings.searchTermsAbout")}>
            <div className="space-y-3 text-sm">
              <p className="text-ink">
                {appVersion
                  ? t("settings.aboutVersion", { version: appVersion })
                  : t("settings.aboutTagline")}
              </p>
              <button
                type="button"
                onClick={() => emit("whats-new-open")}
                disabled={!hasReleaseNotes}
                className="text-sm text-accent hover:text-accent-hover transition-colors hover:underline disabled:opacity-50 disabled:cursor-not-allowed"
                title={!hasReleaseNotes ? "No release notes for this version" : undefined}
              >
                {t("whatsNew.settingsButton")}
              </button>
              <p className="text-ink-muted">{t("settings.aboutLicense")}</p>
              <div className="space-y-2">
                <h4 className="text-xs font-semibold uppercase tracking-wider text-ink-muted">
                  {t("settings.thirdPartyHeading")}
                </h4>
                <ul className="space-y-1.5 text-ink-muted">
                  <li>
                    <span className="text-ink">libmobi</span> — LGPL-3.0-or-later,{" "}
                    {/*
                      Windows ships a statically-linked libmobi (mobi.lib baked
                      into Carrel.exe). Linux and arm64 macOS dynamically link
                      against the system libmobi. Using `navigator.userAgent`
                      avoids pulling in `@tauri-apps/plugin-os` for this single
                      string — the user-agent inside Tauri's WebView reliably
                      contains "Windows NT" on Windows builds.
                    */}
                    {navigator.userAgent.includes("Windows")
                      ? t("settings.staticallyLinked")
                      : t("settings.dynamicallyLinked")}
                    .{" "}
                    <a
                      href="https://github.com/bfabiszewski/libmobi"
                      target="_blank"
                      rel="noreferrer"
                      className="text-accent hover:underline"
                    >
                      {t("settings.source")}
                    </a>
                  </li>
                  <li>
                    <span className="text-ink">pdfium</span> — BSD-3-Clause,{" "}
                    {t("settings.bundled")}.{" "}
                    <a
                      href="https://pdfium.googlesource.com/pdfium/"
                      target="_blank"
                      rel="noreferrer"
                      className="text-accent hover:underline"
                    >
                      {t("settings.source")}
                    </a>
                  </li>
                  <li>
                    <span className="text-ink">unrar</span> — UnRAR license (decompression only).{" "}
                    <a
                      href="https://www.rarlab.com/rar_add.htm"
                      target="_blank"
                      rel="noreferrer"
                      className="text-accent hover:underline"
                    >
                      {t("settings.source")}
                    </a>
                  </li>
                </ul>
              </div>
              <p className="text-xs text-ink-muted pt-1">
                {t("settings.aboutFullNotice")}{" "}
                <a
                  href="https://github.com/mikedamoiseau/folio/blob/main/THIRD_PARTY_LICENSES.md"
                  target="_blank"
                  rel="noreferrer"
                  className="text-accent hover:underline"
                >
                  THIRD_PARTY_LICENSES.md
                </a>
                .
              </p>
            </div>
          </Accordion>
        </div>
      </div>

      {showActivityLog && <ActivityLog onClose={() => setShowActivityLog(false)} />}

      {/* Migration confirmation dialog */}
      {migrationDialog && (
        <>
          <div
            className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-[60]"
            onClick={handleCancelMigration}
            aria-hidden="true"
          />
          <div
            role="dialog"
            aria-label={t("settings.changeLibraryFolder")}
            aria-modal="true"
            className="fixed inset-0 z-[70] flex items-center justify-center p-4"
          >
            <div className="bg-surface rounded-2xl shadow-2xl w-full max-w-md border border-warm-border p-6 space-y-5">
              <h3 className="font-serif text-base font-semibold text-ink">
                {t("settings.changeLibraryFolder")}
              </h3>

              {/* Paths */}
              <div className="space-y-2 text-sm">
                <div>
                  <p className="text-xs text-ink-muted mb-0.5">{t("settings.currentFolder")}</p>
                  <p className="text-ink font-mono text-xs break-all bg-warm-subtle rounded-lg px-2.5 py-1.5">
                    {migrationDialog.currentFolder}
                  </p>
                </div>
                <div className="flex justify-center text-ink-muted text-xs">↓</div>
                <div>
                  <p className="text-xs text-ink-muted mb-0.5">{t("settings.newFolder")}</p>
                  <p className="text-ink font-mono text-xs break-all bg-warm-subtle rounded-lg px-2.5 py-1.5">
                    {migrationDialog.newFolder}
                  </p>
                </div>
              </div>

              {/* File count / size */}
              <p className="text-sm text-ink-muted">
                {migrationDialog.fileCount === 1 ? t("settings.fileCount", { count: migrationDialog.fileCount }) : t("settings.filesCount", { count: migrationDialog.fileCount })},{" "}
                {formatBytes(migrationDialog.totalSizeBytes)}
              </p>

              {/* Don't move checkbox */}
              <label className="flex items-start gap-2.5 cursor-pointer group">
                <input
                  type="checkbox"
                  checked={dontMoveFiles}
                  onChange={(e) => setDontMoveFiles(e.target.checked)}
                  disabled={migrating}
                  className="mt-0.5 accent-accent"
                />
                <span className="flex flex-col gap-1 leading-snug">
                  <span className="text-sm text-ink">
                    {t("settings.dontMoveFiles")}
                  </span>
                  {dontMoveFiles && (
                    <span className="text-xs text-amber-700 dark:text-amber-400">
                      {t("settings.dontMoveFilesHint")}
                    </span>
                  )}
                </span>
              </label>

              {/* Error */}
              {migrationError && (
                <p className="text-sm text-red-600 dark:text-red-400 bg-red-50 dark:bg-red-900/20 rounded-lg px-3 py-2">
                  {migrationError}
                </p>
              )}

              {/* Actions */}
              <div className="flex gap-2 justify-end">
                <button
                  onClick={handleCancelMigration}
                  disabled={migrating}
                  className="px-4 py-2 text-sm text-ink-muted hover:text-ink rounded-xl transition-colors disabled:opacity-40"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleConfirmMigration}
                  disabled={migrating}
                  className="px-4 py-2 text-sm font-medium bg-accent text-surface rounded-xl hover:opacity-90 transition-opacity disabled:opacity-50 flex items-center gap-2"
                >
                  {migrating && (
                    <svg
                      className="animate-spin w-3.5 h-3.5"
                      viewBox="0 0 24 24"
                      fill="none"
                    >
                      <circle
                        className="opacity-25"
                        cx="12" cy="12" r="10"
                        stroke="currentColor"
                        strokeWidth="4"
                      />
                      <path
                        className="opacity-75"
                        fill="currentColor"
                        d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z"
                      />
                    </svg>
                  )}
                  {dontMoveFiles ? t("settings.changeFolder2") : t("settings.moveAndUpdate")}
                </button>
              </div>
            </div>
          </div>
        </>
      )}
      {cleanupState !== "idle" && (
        <>
          <div
            className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-[60]"
            onClick={() => cleanupState !== "scanning" && setCleanupState("idle")}
            aria-hidden="true"
          />
          <div
            role="dialog"
            aria-label={t("settings.cleanupConfirmTitle")}
            aria-modal="true"
            className="fixed inset-0 z-[70] flex items-center justify-center p-4"
          >
            <div className="bg-surface rounded-2xl shadow-2xl w-full max-w-md border border-warm-border p-6 space-y-5">
              <h3 className="font-serif text-base font-semibold text-ink">
                {t("settings.cleanupConfirmTitle")}
              </h3>

              {cleanupState === "confirm" && (
                <>
                  <p className="text-sm text-ink-muted">
                    {t("settings.cleanupConfirmMessage")}
                  </p>
                  <div className="flex gap-3 justify-end pt-1">
                    <button
                      onClick={() => setCleanupState("idle")}
                      className="px-4 py-2 text-sm text-ink-muted hover:text-ink transition-colors"
                    >
                      {t("common.cancel")}
                    </button>
                    <button
                      onClick={handleCleanup}
                      className="px-4 py-2 text-sm bg-accent text-white rounded-xl hover:bg-accent-hover transition-colors font-medium"
                    >
                      {t("settings.cleanupConfirmContinue")}
                    </button>
                  </div>
                </>
              )}

              {cleanupState === "scanning" && (
                <p className="text-sm text-ink-muted">
                  {t("settings.cleanupScanning", {
                    current: cleanupProgress.current,
                    total: cleanupProgress.total,
                  })}
                </p>
              )}

              {cleanupState === "done" && (
                <>
                  <p className="text-sm text-ink-muted">
                    {cleanupResult && cleanupResult.removedCount > 0
                      ? t("settings.cleanupDoneRemoved", { count: cleanupResult.removedCount })
                      : t("settings.cleanupDoneNone")}
                  </p>
                  {cleanupResult && (
                    <p className="text-xs text-ink-muted/70 break-all font-mono">
                      {t("settings.cleanupBackupSaved", { path: cleanupResult.backupPath })}
                    </p>
                  )}
                  <div className="flex justify-end pt-1">
                    <button
                      onClick={() => {
                        setCleanupState("idle");
                        loadLibraryFolder();
                      }}
                      className="px-4 py-2 text-sm bg-accent text-white rounded-xl hover:bg-accent-hover transition-colors font-medium"
                    >
                      {t("settings.cleanupDone")}
                    </button>
                  </div>
                </>
              )}
            </div>
          </div>
        </>
      )}
      {restoreModalOpen && !restoreConfirmPath && (
        <>
          <div
            className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-[60]"
            onClick={() => !restoring && setRestoreModalOpen(false)}
            aria-hidden="true"
          />
          <div
            role="dialog"
            aria-label={t("settings.restoreTitle")}
            aria-modal="true"
            className="fixed inset-0 z-[70] flex items-center justify-center p-4"
          >
            <div className="bg-surface rounded-2xl shadow-2xl w-full max-w-md border border-warm-border p-6 space-y-5">
              <h3 className="font-serif text-base font-semibold text-ink">
                {t("settings.restoreTitle")}
              </h3>

              <div>
                <p className="text-xs font-medium text-ink-muted mb-2">{t("settings.autoBackups")}</p>
                {autoBackups.length === 0 ? (
                  <p className="text-sm text-ink-muted/70 italic">{t("settings.noAutoBackups")}</p>
                ) : (
                  <div className="max-h-[200px] overflow-y-auto space-y-1.5">
                    {autoBackups.map((backup) => (
                      <div
                        key={backup.path}
                        className="flex items-center justify-between gap-2 bg-warm-subtle rounded-xl px-3 py-2"
                      >
                        <div className="min-w-0">
                          <p className="text-sm text-ink truncate">
                            {backup.label} — {new Intl.DateTimeFormat(undefined, {
                              dateStyle: "medium",
                              timeStyle: "short",
                            }).format(new Date(backup.timestamp * 1000))}
                          </p>
                          <p className="text-xs text-ink-muted">{formatBytes(backup.sizeBytes)}</p>
                        </div>
                        <button
                          onClick={() => setRestoreConfirmPath(backup.path)}
                          className="shrink-0 px-3 py-1.5 text-xs bg-accent text-white rounded-lg hover:bg-accent-hover transition-colors font-medium"
                        >
                          {t("settings.restore")}
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <div>
                <p className="text-xs font-medium text-ink-muted mb-2">{t("settings.fromFile")}</p>
                <button
                  onClick={handleRestoreFromFile}
                  className="w-full px-3 py-2 text-sm text-ink-muted hover:text-ink bg-warm-subtle hover:bg-warm-border rounded-xl transition-colors text-left"
                >
                  {t("settings.chooseFile")}
                </button>
              </div>

              <div className="flex justify-end pt-1">
                <button
                  onClick={() => setRestoreModalOpen(false)}
                  className="px-4 py-2 text-sm text-ink-muted hover:text-ink transition-colors"
                >
                  {t("common.cancel")}
                </button>
              </div>
            </div>
          </div>
        </>
      )}

      {restoreConfirmPath && (
        <>
          <div className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-[80]" aria-hidden="true" />
          <div
            role="dialog"
            aria-label={t("settings.restoreTitle")}
            aria-modal="true"
            className="fixed inset-0 z-[90] flex items-center justify-center p-4"
          >
            <div className="bg-surface rounded-2xl shadow-2xl w-full max-w-md border border-warm-border p-6 space-y-5">
              <h3 className="font-serif text-base font-semibold text-ink">
                {t("settings.restoreTitle")}
              </h3>
              <p className="text-sm text-ink-muted">
                {t("settings.restoreConfirmMessage")}
              </p>
              <div className="flex gap-3 justify-end pt-1">
                <button
                  onClick={() => setRestoreConfirmPath(null)}
                  disabled={restoring}
                  className="px-4 py-2 text-sm text-ink-muted hover:text-ink transition-colors"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleConfirmRestore}
                  disabled={restoring}
                  className="px-4 py-2 text-sm bg-accent text-white rounded-xl hover:bg-accent-hover transition-colors font-medium disabled:opacity-40"
                >
                  {restoring ? t("common.working") : t("settings.restore")}
                </button>
              </div>
            </div>
          </div>
        </>
      )}
    </>
  );
}
