import { useTranslation } from "react-i18next";
import { usePrivateMode } from "../hooks/usePrivateMode";

/**
 * App-wide persistent indicator for "don't track this session" (private
 * mode). Mounted once at the very top of the app shell — above the nav and
 * outside the reader's own auto-hiding header — so the signal stays visible
 * everywhere, including while actively reading a book (when the reader
 * header, and its PrivateModeToggle, is hidden on scroll).
 *
 * Indicator only: the on/off switch lives in the header PrivateModeToggle
 * and its popover. This strip's job is purely to make the app *look*
 * different while tracking is paused (cf. a browser's incognito chrome),
 * using the cool indigo `--private` hue that sits deliberately outside
 * Carrel's warm palette.
 */
export default function PrivateModeBar() {
  const { t } = useTranslation();
  const { enabled } = usePrivateMode();

  if (!enabled) return null;

  return (
    <div
      role="status"
      aria-live="polite"
      className="shrink-0 h-7 flex items-center justify-center gap-1.5 bg-private text-private-fg text-xs font-medium tracking-wide select-none animate-fade-in"
    >
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <path d="M9.88 9.88a3 3 0 1 0 4.24 4.24" />
        <path d="M10.73 5.08A10.43 10.43 0 0 1 12 5c7 0 10 7 10 7a13.16 13.16 0 0 1-1.67 2.68" />
        <path d="M6.61 6.61A13.526 13.526 0 0 0 2 12s3 7 10 7a9.74 9.74 0 0 0 5.39-1.61" />
        <path d="M2 2l20 20" />
      </svg>
      <span>{t("privateMode.stripLabel")}</span>
    </div>
  );
}
