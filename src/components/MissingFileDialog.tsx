import { useTranslation } from "react-i18next";
import { useFocusTrap } from "../lib/useFocusTrap";

interface MissingFileDialogProps {
  /** Dismiss the dialog without removing the book. */
  onCancel: () => void;
  /** Remove the book from the library, then navigate away. Omitted (Kids
   *  mode): the dialog offers only Cancel. */
  onRemove?: () => void;
}

/**
 * Blocking dialog shown when a book's underlying file can no longer be found
 * (moved or deleted). Offers two recovery actions: dismiss (back to library)
 * or remove the orphaned entry from the library.
 *
 * Controlled: the caller mounts it only while the missing-file condition holds.
 */
export default function MissingFileDialog({
  onCancel,
  onRemove,
}: MissingFileDialogProps) {
  const { t } = useTranslation();
  const dialogRef = useFocusTrap(onCancel);

  return (
    <>
      <div
        className="fixed inset-0 bg-ink/40 backdrop-blur-sm z-[90]"
        aria-hidden="true"
        onClick={onCancel}
      />
      <div className="fixed inset-0 z-[90] flex items-center justify-center p-4 pointer-events-none">
        <div
          ref={dialogRef}
          role="dialog"
          aria-modal="true"
          aria-label={t("reader.missingFileTitle")}
          className="bg-surface rounded-2xl shadow-2xl w-full max-w-md border border-warm-border p-6 space-y-5 pointer-events-auto"
          onClick={(e) => e.stopPropagation()}
        >
          <h3 className="font-serif text-base font-semibold text-ink">
            {t("reader.missingFileTitle")}
          </h3>
          <p className="text-sm text-ink-muted">
            {t("reader.missingFileMessage")}
          </p>
          <div className="flex gap-3 justify-end pt-1">
            <button
              onClick={onCancel}
              className="px-4 py-2 text-sm text-ink-muted hover:text-ink transition-colors"
            >
              {t("common.cancel")}
            </button>
            {onRemove && (
              <button
                onClick={onRemove}
                className="px-4 py-2 text-sm bg-red-600 text-white rounded-xl hover:bg-red-700 transition-colors font-medium"
              >
                {t("reader.removeFromLibrary")}
              </button>
            )}
          </div>
        </div>
      </div>
    </>
  );
}
