import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import LanguageDetector from "i18next-browser-languagedetector";
import en from "./locales/en.json";
import fr from "./locales/fr.json";

export const LANGUAGES = [
  { code: "en", flag: "\uD83C\uDDEC\uD83C\uDDE7", label: "English" },
  { code: "fr", flag: "\uD83C\uDDEB\uD83C\uDDF7", label: "Fran\u00e7ais" },
] as const;

i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: {
      en: { translation: en },
      fr: { translation: fr },
    },
    fallbackLng: "en",
    interpolation: { escapeValue: false },
    detection: {
      order: ["localStorage", "navigator"],
      lookupLocalStorage: "carrel-language",
      caches: ["localStorage"],
    },
  });

export default i18n;
