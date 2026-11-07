import { useEffect, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import type { Book } from "../types";
import { formatDuration, formatDate } from "../lib/utils";
import WantToReadToggle from "./WantToReadToggle";
import KidsMarkControl from "./KidsMarkControl";

interface BookReadingStats {
  totalReadingTimeSecs: number;
  sessionCount: number;
  firstReadAt: number | null;
  finishedAt: number | null;
}

interface BookDetailModalProps {
  book: Book;
  onClose: () => void;
  // Action handlers are optional: when none are provided (e.g. the reader's
  // read-only "book details" popup) the action footer is omitted entirely.
  onOpen?: (id: string) => void;
  onEdit?: (id: string) => void;
  onScan?: (id: string) => Promise<void>;
  // Marks/unmarks the book as "want to read". Present only for the actionable
  // library modal; the reader's read-only popup omits it (and the footer).
  onWantToReadChange?: (id: string, want: boolean) => void;
  onWantToReadError?: (err: unknown) => void;
}

function ScanButton({ bookId, onScan }: { bookId: string; onScan: (id: string) => Promise<void> }) {
  const { t } = useTranslation();
  const [scanning, setScanning] = useState(false);
  return (
    <button
      type="button"
      disabled={scanning}
      onClick={async () => {
        setScanning(true);
        try { await onScan(bookId); } finally { setScanning(false); }
      }}
      className="px-4 py-2 rounded-xl bg-warm-subtle text-ink text-sm font-medium hover:bg-warm-border transition-colors disabled:opacity-50 focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
      title={t("detail.scanForMetadata")}
    >
      {scanning ? (
        <div className="w-4 h-4 border-2 border-ink-muted border-t-transparent rounded-full animate-spin mx-auto" />
      ) : (
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" className="mx-auto">
          <path d="M9.813 15.904L9 18.75l-.813-2.846a4.5 4.5 0 00-3.09-3.09L2.25 12l2.846-.813a4.5 4.5 0 003.09-3.09L9 5.25l.813 2.846a4.5 4.5 0 003.09 3.09L15.75 12l-2.846.813a4.5 4.5 0 00-3.09 3.09zM18.259 8.715L18 9.75l-.259-1.035a3.375 3.375 0 00-2.455-2.456L14.25 6l1.036-.259a3.375 3.375 0 002.455-2.456L18 2.25l.259 1.035a3.375 3.375 0 002.455 2.456L21.75 6l-1.036.259a3.375 3.375 0 00-2.455 2.456zM16.894 20.567L16.5 21.75l-.394-1.183a2.25 2.25 0 00-1.423-1.423L13.5 18.75l1.183-.394a2.25 2.25 0 001.423-1.423l.394-1.183.394 1.183a2.25 2.25 0 001.423 1.423l1.183.394-1.183.394a2.25 2.25 0 00-1.423 1.423z" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      )}
    </button>
  );
}

export default function BookDetailModal({ book, onClose, onOpen, onEdit, onScan, onWantToReadChange, onWantToReadError }: BookDetailModalProps) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDivElement>(null);
  // Read-only variant (e.g. the reader's info popup) has no action footer, so
  // it would otherwise contain zero focusable elements — breaking the focus
  // trap and leaving nothing to auto-focus. Give it an explicit close button.
  const readOnly = !onOpen && !onEdit && !onScan;

  const [readingStats, setReadingStats] = useState<BookReadingStats | null>(null);

  // Known limitation: this reflects completed sessions only. When opened from
  // the reader's info popup, the current session hasn't been recorded yet —
  // record_reading_session only fires on reader unmount — so an in-progress
  // (or first) session isn't counted until the reader closes.
  useEffect(() => {
    let ignore = false;
    setReadingStats(null);
    invoke<BookReadingStats | null>("get_book_reading_stats", { bookId: book.id })
      .then((stats) => {
        if (!ignore) setReadingStats(stats);
      })
      .catch(() => {
        if (!ignore) setReadingStats(null);
      });
    return () => {
      ignore = true;
    };
  }, [book.id]);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
        return;
      }
      if (e.key === "Tab" && dialogRef.current) {
        const focusable = dialogRef.current.querySelectorAll<HTMLElement>(
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
    };
    document.addEventListener("keydown", handleKeyDown);
    // Auto-focus. The read-only variant's only control is the corner X;
    // focusing it would flash a focus ring the instant the (mouse-opened)
    // popup appears, so focus the dialog container instead — Tab still reaches
    // the X (and shows its ring for keyboard users), it just isn't pre-focused.
    if (readOnly) {
      dialogRef.current?.focus();
    } else {
      const firstBtn = dialogRef.current?.querySelector<HTMLElement>("button");
      firstBtn?.focus();
    }
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [onClose, readOnly]);

  const coverSrc = book.cover_path ? convertFileSrc(book.cover_path) : null;

  const metadataRows: { label: string; value: string }[] = [];
  if (book.series) {
    const val = book.volume != null ? `${book.series} #${book.volume}` : book.series;
    metadataRows.push({ label: t("detail.series"), value: val });
  }
  if (book.language) metadataRows.push({ label: t("detail.language"), value: book.language });
  if (book.publish_year != null) metadataRows.push({ label: t("detail.year"), value: String(book.publish_year) });
  if (book.publisher) metadataRows.push({ label: t("detail.publisher"), value: book.publisher });

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label={t("detail.detailsFor", { title: book.title })}
        tabIndex={-1}
        className="relative bg-surface border border-warm-border rounded-2xl shadow-xl max-w-md w-full mx-4 overflow-hidden focus:outline-none"
        onClick={(e) => e.stopPropagation()}
      >
        {readOnly && (
          <button
            type="button"
            onClick={onClose}
            aria-label={t("common.close")}
            title={t("common.close")}
            className="absolute top-3 right-3 z-10 p-1.5 rounded-lg text-ink-muted hover:text-ink hover:bg-warm-subtle transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        )}

        {/* Header: cover + title */}
        <div className="flex gap-4 p-5">
          {coverSrc ? (
            <img
              src={coverSrc}
              alt={t("detail.coverAlt", { title: book.title })}
              className="w-[100px] h-[150px] object-cover rounded-lg shadow-sm flex-shrink-0"
            />
          ) : (
            <div className="w-[100px] h-[150px] bg-warm-subtle rounded-lg flex items-center justify-center flex-shrink-0">
              <svg width="32" height="32" viewBox="0 0 24 24" fill="none" className="text-ink-muted opacity-50">
                <path
                  d="M4 19.5v-15A2.5 2.5 0 016.5 2H20v20H6.5a2.5 2.5 0 010-5H20"
                  stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"
                />
              </svg>
            </div>
          )}
          <div className="flex flex-col justify-center min-w-0">
            <h2 className="text-lg font-semibold text-ink leading-snug">{book.title}</h2>
            <p className="text-sm text-ink-muted mt-1">{book.author}</p>
            {book.format !== "epub" && (
              <span className="mt-2 self-start text-[10px] font-semibold uppercase tracking-wider bg-warm-subtle text-ink-muted px-2 py-0.5 rounded">
                {book.format}
              </span>
            )}
          </div>
        </div>

        {/* Metadata rows */}
        {metadataRows.length > 0 && (
          <div className="px-5 pb-3">
            <div className="border-t border-warm-border pt-3 space-y-1.5">
              {metadataRows.map((row) => (
                <div key={row.label} className="flex text-sm">
                  <span className="text-ink-muted w-20 flex-shrink-0">{row.label}</span>
                  <span className="text-ink">{row.value}</span>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Reading insights (F-1-7). Each row renders only when its own datum
            is present, so e.g. a book finished via sync (finished_at only,
            no local sessions) shows just the "Finished" row. */}
        {readingStats && (
          <div className="px-5 pb-3">
            <div className="border-t border-warm-border pt-3 space-y-1.5">
              <h3 className="text-xs font-semibold text-ink-muted uppercase tracking-wide">{t("detail.reading")}</h3>
              {readingStats.sessionCount > 0 && (
                <>
                  <div className="flex text-sm">
                    <span className="text-ink-muted w-24 flex-shrink-0">{t("detail.timeRead")}</span>
                    <span className="text-ink">{formatDuration(readingStats.totalReadingTimeSecs)}</span>
                  </div>
                  <div className="flex text-sm">
                    <span className="text-ink-muted w-24 flex-shrink-0">{t("detail.sessions")}</span>
                    <span className="text-ink">{readingStats.sessionCount}</span>
                  </div>
                  <div className="flex text-sm">
                    <span className="text-ink-muted w-24 flex-shrink-0">{t("detail.avgSession")}</span>
                    <span className="text-ink">
                      {formatDuration(Math.floor(readingStats.totalReadingTimeSecs / readingStats.sessionCount))}
                    </span>
                  </div>
                </>
              )}
              {readingStats.firstReadAt != null && (
                <div className="flex text-sm">
                  <span className="text-ink-muted w-24 flex-shrink-0">{t("detail.started")}</span>
                  <span className="text-ink">{formatDate(readingStats.firstReadAt)}</span>
                </div>
              )}
              {readingStats.finishedAt != null && (
                <div className="flex text-sm">
                  <span className="text-ink-muted w-24 flex-shrink-0">{t("detail.finished")}</span>
                  <span className="text-ink">{formatDate(readingStats.finishedAt)}</span>
                </div>
              )}
            </div>
          </div>
        )}

        {/* Description */}
        {book.description && (
          <div className="px-5 pb-3">
            <div className="border-t border-warm-border pt-3">
              <p className="text-sm text-ink-muted leading-relaxed max-h-40 overflow-y-auto whitespace-pre-line">
                {book.description}
              </p>
            </div>
          </div>
        )}

        {/* Actions — omitted entirely for the read-only (reader) variant */}
        {(onOpen || onEdit || onScan) && (
          <div className="flex gap-3 items-center px-5 py-4 border-t border-warm-border">
            {onWantToReadChange && (
              <WantToReadToggle
                bookId={book.id}
                value={book.want_to_read}
                onChange={(next) => onWantToReadChange(book.id, next)}
                onError={onWantToReadError}
              />
            )}
            {onOpen && (
              <button
                type="button"
                onClick={() => onOpen(book.id)}
                className="flex-1 px-4 py-2 rounded-xl bg-accent text-white text-sm font-medium hover:bg-accent/90 transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              >
                {t("common.open")}
              </button>
            )}
            {onEdit && (
              <button
                type="button"
                onClick={() => onEdit(book.id)}
                className="flex-1 px-4 py-2 rounded-xl bg-warm-subtle text-ink text-sm font-medium hover:bg-warm-border transition-colors focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
              >
                {t("common.edit")}
              </button>
            )}
            {onScan && <ScanButton bookId={book.id} onScan={onScan} />}
          </div>
        )}
        {/* Kids mode marking — admin only: tied to the editable variant, which
            the library omits (no onEdit) while Kids mode is on. */}
        {onEdit && (
          <div className="px-5 pb-4">
            <KidsMarkControl bookId={book.id} />
          </div>
        )}
      </div>
    </div>
  );
}
