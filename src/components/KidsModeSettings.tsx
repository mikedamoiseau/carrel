import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { toCarrelError } from "../lib/errors";
import { useKidsMode } from "../hooks/useKidsMode";

/** Settings section: set/change the parents' code and turn Kids mode on.
 *  Rendered only while Kids mode is off (the backend refuses these commands
 *  otherwise). */
export default function KidsModeSettings() {
  const { t } = useTranslation();
  const { codeSet, refresh } = useKidsMode();
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const saveCode = async () => {
    setSaved(false);
    if (!/^\d{4,12}$/.test(code)) {
      setError(t("kids.codeInvalid"));
      return;
    }
    setError(null);
    try {
      await invoke("set_kids_code", { code });
      setCode("");
      setSaved(true);
      await refresh();
    } catch (err) {
      setError(toCarrelError(err).message);
    }
  };

  const turnOn = async () => {
    setError(null);
    setSaved(false);
    try {
      await invoke("enter_kids_mode");
    } catch (err) {
      setError(toCarrelError(err).message);
    }
  };

  return (
    <div className="space-y-2">
      <p className="text-xs text-ink-muted px-1">{t("kids.settingsDescription")}</p>
      <div className="bg-warm-subtle rounded-xl px-3 py-2.5">
        <label htmlFor="kids-code" className="text-xs text-ink-muted mb-1 block">
          {t("kids.codeLabel")}
        </label>
        <input
          id="kids-code"
          type="password"
          inputMode="numeric"
          autoComplete="off"
          value={code}
          onChange={(e) => { setCode(e.target.value); setError(null); setSaved(false); }}
          className="w-full bg-transparent text-sm text-ink focus:outline-none"
        />
        <button
          type="button"
          onClick={() => void saveCode()}
          className="mt-2 px-3 py-1.5 text-xs font-medium rounded-lg bg-warm-border text-ink hover:bg-warm-border/70 transition-colors"
        >
          {codeSet ? t("kids.changeCode") : t("kids.setCode")}
        </button>
        {saved && <p className="text-xs text-ink-muted mt-1.5">{t("kids.codeSaved")}</p>}
      </div>
      <button
        type="button"
        onClick={() => void turnOn()}
        className="px-3 py-1.5 text-xs font-medium rounded-lg bg-accent text-white hover:bg-accent-hover transition-colors"
      >
        {t("kids.turnOn")}
      </button>
      {error && <p role="alert" className="text-xs text-red-500 px-1">{error}</p>}
    </div>
  );
}
