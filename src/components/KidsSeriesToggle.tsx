import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";

/** Marks or unmarks one series as OK for kids (series drill-in header). */
export default function KidsSeriesToggle({ series }: { series: string }) {
  const { t } = useTranslation();
  const [allowed, setAllowed] = useState<boolean | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await invoke<string[]>("get_kids_series");
      setAllowed(list.includes(series));
    } catch {
      setAllowed(null);
    }
  }, [series]);

  useEffect(() => { void load(); }, [load]);

  if (allowed === null) return null;

  const toggle = async () => {
    try {
      await invoke("set_kids_series", { series, allowed: !allowed });
      await load();
    } catch {
      // leave the label as is; the next load reflects the real state
    }
  };

  return (
    <button
      type="button"
      onClick={() => void toggle()}
      className="col-[1/-1] justify-self-start mb-2 px-2.5 py-1 text-xs font-medium rounded-lg bg-warm-subtle text-ink hover:bg-warm-border transition-colors"
    >
      {allowed ? t("kids.unmarkSeries") : t("kids.markSeries")}
    </button>
  );
}
