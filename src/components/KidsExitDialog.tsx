import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { toCarrelError } from "../lib/errors";
import { useFocusTrap } from "../lib/useFocusTrap";

interface KidsExitDialogProps {
  onClose: () => void;
  /** Called after the backend accepted the code and Kids mode is off. */
  onExited: () => void;
}

/** Parents' code prompt for leaving Kids mode. Backend messages are shown
 *  verbatim — `friendlyError` would replace them with generic copy. */
export default function KidsExitDialog({ onClose, onExited }: KidsExitDialogProps) {
  const { t } = useTranslation();
  const dialogRef = useFocusTrap(onClose);
  const [code, setCode] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (submitting || !code) return;
    setSubmitting(true);
    setError(null);
    try {
      await invoke("exit_kids_mode", { code });
      onExited();
    } catch (err) {
      setError(toCarrelError(err).message);
      setSubmitting(false);
    }
  };

  return (
    <>
      <div className="fixed inset-0 z-[90] bg-ink/30 backdrop-blur-sm" onClick={onClose} />
      <div className="fixed inset-0 z-[91] flex items-center justify-center p-4 pointer-events-none">
        <div
          ref={dialogRef}
          role="dialog"
          aria-modal="true"
          aria-label={t("kids.leave")}
          className="bg-surface rounded-2xl shadow-xl border border-warm-border w-full max-w-sm pointer-events-auto overflow-hidden"
        >
          <form onSubmit={handleSubmit} className="px-6 py-5 space-y-3">
            <h2 className="font-serif text-lg font-semibold text-ink leading-snug">{t("kids.leave")}</h2>
            <div>
              <label htmlFor="kids-exit-code" className="text-xs text-ink-muted mb-1 block">
                {t("kids.codeLabel")}
              </label>
              <input
                id="kids-exit-code"
                type="password"
                inputMode="numeric"
                autoComplete="off"
                value={code}
                autoFocus
                disabled={submitting}
                onChange={(e) => { setCode(e.target.value); setError(null); }}
                className="w-full bg-warm-subtle border border-warm-border rounded-lg px-2.5 py-1.5 text-sm text-ink focus:outline-none focus:border-accent"
              />
              {error && <p role="alert" className="text-xs text-red-500 mt-1.5">{error}</p>}
            </div>
            <div className="flex gap-2 justify-end pt-1">
              <button
                type="button"
                onClick={onClose}
                className="px-4 py-1.5 text-sm font-medium text-ink-muted hover:text-ink hover:bg-warm-subtle rounded-lg transition-colors duration-150"
              >
                {t("common.cancel")}
              </button>
              <button
                type="submit"
                disabled={submitting || !code}
                className="px-4 py-1.5 text-sm font-medium text-white bg-accent hover:bg-accent-hover rounded-lg transition-colors duration-150 disabled:opacity-40 disabled:cursor-not-allowed"
              >
                {t("kids.leaveConfirm")}
              </button>
            </div>
          </form>
        </div>
      </div>
    </>
  );
}
