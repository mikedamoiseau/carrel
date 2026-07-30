import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { ReleaseVersion } from "../../vite-plugin-release-notes";

interface WhatsNewModalProps {
  release: ReleaseVersion;
  onClose: () => void;
}

const CHANGELOG_URL = "https://github.com/mikedamoiseau/carrel/blob/main/CHANGELOG.md";

export default function WhatsNewModal({ release, onClose }: WhatsNewModalProps) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    // Focus the dialog container (tabIndex -1) rather than the first button
    // (the ✕), so opening the modal doesn't paint a focus ring on Close.
    dialogRef.current?.focus();
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label={t("whatsNew.modalTitle", { version: release.version })}
        tabIndex={-1}
        className="bg-surface border border-warm-border rounded-2xl shadow-xl max-w-lg w-full mx-4 max-h-[80vh] flex flex-col overflow-hidden focus:outline-none"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-center justify-between p-5 border-b border-warm-border">
          <div>
            <h2 className="text-lg font-semibold text-ink">
              {t("whatsNew.modalTitle", { version: release.version })}
            </h2>
            <p className="text-sm text-ink-muted mt-0.5">{release.date}</p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="p-1.5 rounded-lg text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none">
              <path d="M18 6L6 18M6 6l12 12" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-5 space-y-5">
          {Object.entries(release.categories).map(([category, entries]) => (
            <div key={category}>
              <h3 className="text-xs font-semibold uppercase tracking-wider text-ink-muted mb-2">
                {category}
              </h3>
              <ul className="space-y-1.5">
                {entries.map((entry) => (
                  <li key={entry.title} className="text-sm text-ink font-medium">
                    {entry.title}
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>

        {/* Footer */}
        <div className="p-4 border-t border-warm-border">
          <button
            type="button"
            onClick={() => openUrl(CHANGELOG_URL)}
            className="text-sm text-accent hover:text-accent-hover transition-colors hover:underline"
          >
            {t("whatsNew.modalFullChangelog")} ↗
          </button>
        </div>
      </div>
    </div>
  );
}
