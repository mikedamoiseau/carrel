import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { toCarrelError } from "../lib/errors";

type Mode = "follow" | "allowed" | "blocked";

interface KidsBookMark {
  override: Mode;
  series_allowed: boolean;
  visible: boolean;
}

/** Per-book Kids mode mark: follow the series, force OK, or force hidden,
 *  plus whether the book is currently visible in Kids mode. */
export default function KidsMarkControl({ bookId }: { bookId: string }) {
  const { t } = useTranslation();
  const [mark, setMark] = useState<KidsBookMark | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setMark(await invoke<KidsBookMark | null>("get_kids_book_mark", { bookId }));
    } catch (err) {
      setError(toCarrelError(err).message);
    }
  }, [bookId]);

  useEffect(() => { void load(); }, [load]);

  const change = async (mode: Mode) => {
    setError(null);
    try {
      await invoke("set_kids_book_override", { bookId, mode });
      await load();
    } catch (err) {
      setError(toCarrelError(err).message);
    }
  };

  if (!mark) return error ? <p className="text-xs text-red-500">{error}</p> : null;

  return (
    <div className="flex items-center gap-2 text-xs">
      <select
        aria-label={t("kids.settingsTitle")}
        value={mark.override}
        onChange={(e) => void change(e.target.value as Mode)}
        className="bg-warm-subtle border border-warm-border rounded-lg px-2 py-1 text-ink focus:outline-none focus:border-accent"
      >
        <option value="follow">{t("kids.followSeries")}</option>
        <option value="allowed">{t("kids.okForKids")}</option>
        <option value="blocked">{t("kids.notForKids")}</option>
      </select>
      <span className="text-ink-muted">{mark.visible ? t("kids.visible") : t("kids.hidden")}</span>
      {error && <span className="text-red-500">{error}</span>}
    </div>
  );
}
