import en from "../locales/en.json";

/** Resolve a dotted key against the English locale — lets tests assert the
 *  real user-facing copy instead of raw keys. */
export function enT(key: string, params?: Record<string, unknown>): string {
  const v = key
    .split(".")
    .reduce<unknown>((o, k) => (o && typeof o === "object" ? (o as Record<string, unknown>)[k] : undefined), en);
  let s = typeof v === "string" ? v : key;
  if (params) for (const [k, val] of Object.entries(params)) s = s.replace(`{{${k}}}`, String(val));
  return s;
}
