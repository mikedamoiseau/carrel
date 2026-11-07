import { useState } from "react";
import { useTranslation } from "react-i18next";
import KidsExitDialog from "./KidsExitDialog";

/** App-wide strip shown while Kids mode is on; the only way out is the
 *  parents' code dialog it opens. */
export default function KidsModeBanner() {
  const { t } = useTranslation();
  const [leaving, setLeaving] = useState(false);

  return (
    <>
      <div
        role="status"
        className="shrink-0 h-8 px-4 flex items-center justify-center gap-3 bg-accent text-white text-xs font-medium select-none"
      >
        <span>{t("kids.badge")}</span>
        <button
          type="button"
          onClick={() => setLeaving(true)}
          className="underline underline-offset-2 hover:no-underline focus-visible:ring-2 focus-visible:ring-white rounded"
        >
          {t("kids.leave")}
        </button>
      </div>
      {leaving && <KidsExitDialog onClose={() => setLeaving(false)} onExited={() => setLeaving(false)} />}
    </>
  );
}
