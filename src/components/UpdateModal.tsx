import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useToast } from "./Toast";
import { friendlyError } from "../lib/errors";

export interface UpdateCheck {
  update_available: boolean;
  current_version: string;
  latest_version: string;
  release_url: string;
  changelog_url: string;
  release_notes: string;
}

export type UpdateModalState =
  | { status: "loading" }
  | { status: "available"; data: UpdateCheck }
  | { status: "uptodate"; data: UpdateCheck }
  | { status: "error"; rateLimited: boolean };

/** Defense-in-depth on top of authoritative Rust validation. Each target has
 * its own narrow allow-list — a release tag URL, or exactly the releases page. */
function trusted(raw: string, pathOk: (p: string) => boolean): boolean {
  try {
    const u = new URL(raw);
    return u.protocol === "https:" && u.hostname === "github.com" && pathOk(u.pathname);
  } catch {
    return false;
  }
}

export function isTrustedReleaseUrl(raw: string): boolean {
  return trusted(raw, (p) => p.startsWith("/mikedamoiseau/carrel/releases/tag/"));
}

export function isTrustedChangelogUrl(raw: string): boolean {
  return trusted(raw, (p) => p === "/mikedamoiseau/carrel/releases" || p === "/mikedamoiseau/carrel/releases/");
}

interface Props {
  state: UpdateModalState;
  onClose: () => void;
}

export default function UpdateModal({ state, onClose }: Props) {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const dialogRef = useRef<HTMLDivElement>(null);
  const downloadRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopImmediatePropagation();
        onClose();
      }
    };
    document.addEventListener("keydown", onKey);
    // Focus the primary Download action when present; otherwise focus the
    // dialog container itself (tabIndex -1) rather than the Close button, so
    // states without a primary action don't show a focus ring on the ✕.
    (downloadRef.current ?? dialogRef.current)?.focus();
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const title =
    state.status === "available"
      ? t("updateCheck.titleAvailable")
      : state.status === "uptodate"
        ? t("updateCheck.titleUpToDate")
        : state.status === "error"
          ? t("updateCheck.titleError")
          : t("updateCheck.loading");

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
        className="bg-surface border border-warm-border rounded-2xl shadow-xl max-w-lg w-full mx-4 max-h-[80vh] flex flex-col overflow-hidden focus:outline-none"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between p-5 border-b border-warm-border">
          <h2 className="text-lg font-semibold text-ink">{title}</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("updateCheck.close")}
            className="p-1.5 rounded-lg text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none">
              <path d="M18 6L6 18M6 6l12 12" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        <div className="p-5 overflow-y-auto text-sm text-ink">
          {state.status === "loading" && <p>{t("updateCheck.loading")}</p>}

          {state.status === "error" && (
            <p>{state.rateLimited ? t("updateCheck.rateLimitBody") : t("updateCheck.errorBody")}</p>
          )}

          {state.status === "uptodate" && (
            <p>{t("updateCheck.upToDateBody", { version: state.data.latest_version })}</p>
          )}

          {state.status === "available" && (
            <>
              <p className="font-medium">
                {t("updateCheck.newVersion", { version: state.data.latest_version })}
              </p>
              <p className="text-ink-muted mt-0.5">
                {t("updateCheck.currentVersion", { version: state.data.current_version })}
              </p>
              <h3 className="mt-4 mb-1 text-xs font-semibold uppercase tracking-wide text-ink-muted">
                {t("updateCheck.notesHeading")}
              </h3>
              {state.data.release_notes.trim() ? (
                <pre className="whitespace-pre-wrap break-words font-sans text-sm text-ink bg-warm-subtle rounded-lg p-3">
                  {state.data.release_notes}
                </pre>
              ) : (
                <p className="text-ink-muted">{t("updateCheck.notesEmpty")}</p>
              )}
              <button
                type="button"
                onClick={() => {
                  if (isTrustedChangelogUrl(state.data.changelog_url))
                    openUrl(state.data.changelog_url).catch((err) => addToast(friendlyError(err, t), "error"));
                }}
                className="mt-3 text-accent hover:underline"
              >
                {t("updateCheck.fullChangelog")}
              </button>
            </>
          )}
        </div>

        {state.status === "available" && (
          <div className="flex justify-end p-4 border-t border-warm-border">
            <button
              ref={downloadRef}
              type="button"
              onClick={() => {
                if (isTrustedReleaseUrl(state.data.release_url))
                  openUrl(state.data.release_url).catch((err) => addToast(friendlyError(err, t), "error"));
              }}
              className="px-4 py-2 rounded-lg bg-accent text-white text-sm font-medium hover:opacity-90 focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
            >
              {t("updateCheck.download")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
