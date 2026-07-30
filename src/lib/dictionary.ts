// Pure, framework-free helpers for the in-reader dictionary (F-1-1).
//
// The heavy lifting (artifact download, morphy resolution, SQLite lookup) lives
// in the Rust backend; this module owns the small pieces of client logic that
// benefit from being unit-tested in isolation:
//   - `extractLookupWord` — decides whether a text selection is a single word
//     worth looking up, and normalizes it. Also the gate that shows/hides the
//     reader's "Define" button (a `null` result → no button).
//   - `groupSensesByPos` — orders senses into the fixed n/v/a/r part-of-speech
//     grouping the definition card renders.
//
// These mirror the shapes the backend returns (`carrel-core::dictionary`), which
// serialize as camelCase.

/** On-disk state of the artifact, from `get_dictionary_status`. */
export interface DictionaryStatus {
  state: "missing" | "ready" | "corrupt";
  wordnetVersion: string | null;
  sizeBytes: number | null;
}

/** A single sense, as returned by `lookup_word`. */
export interface DictionarySense {
  /** Part of speech: `n` | `v` | `a` | `r`. */
  pos: string;
  senseNum: number;
  gloss: string;
  examples: string[];
  synonyms: string[];
}

/** A successful lookup result. */
export interface DictionaryEntry {
  /** The normalized (lowercased) query word. */
  word: string;
  /** The lemma actually matched — differs from `word` after morphological
   *  normalization (e.g. "running" → "run"). */
  matchedWord: string;
  senses: DictionarySense[];
}

/**
 * Reduce a raw text selection to a single lowercase lookup word, or `null` when
 * the selection isn't a lookable single word. Rules (mirrors the backend's
 * expectations): trim, reject anything containing whitespace (multi-word),
 * strip surrounding non-letters and a trailing possessive (`'s`), then require
 * an ASCII-Latin word of 2–64 characters. Non-Latin scripts and single letters
 * return `null`, which hides the reader's Define button.
 */
export function extractLookupWord(selection: string): string | null {
  const trimmed = selection.trim();
  // Multi-word (or empty) selections are not single-word lookups.
  if (trimmed === "" || /\s/u.test(trimmed)) {
    return null;
  }
  // Strip surrounding punctuation/quotes/brackets (anything not a letter).
  let word = trimmed.replace(/^[^\p{L}]+/u, "").replace(/[^\p{L}]+$/u, "");
  // Drop a trailing possessive: dog's → dog, dogs' already lost its quote above.
  word = word.replace(/['’]s$/iu, "").replace(/['’]$/u, "");
  const lower = word.toLowerCase();
  // ASCII-Latin only; 2–64 chars; inner apostrophes/hyphens allowed.
  if (!/^[a-z][a-z'-]{1,63}$/.test(lower)) {
    return null;
  }
  return lower;
}

/** Parts of speech in the fixed display order used by the definition card. */
export const POS_ORDER = ["n", "v", "a", "r"] as const;

/** i18n key for each part-of-speech label. */
export const POS_LABEL_KEYS: Record<string, string> = {
  n: "reader.dictionary.posNoun",
  v: "reader.dictionary.posVerb",
  a: "reader.dictionary.posAdjective",
  r: "reader.dictionary.posAdverb",
};

/** A part-of-speech group for rendering. */
export interface PosGroup {
  pos: string;
  senses: DictionarySense[];
}

/**
 * Group senses by part of speech in the fixed n/v/a/r order, preserving each
 * group's incoming sense order. Any unexpected POS values are appended after
 * the known ones (defensive — the backend only emits n/v/a/r).
 */
export function groupSensesByPos(senses: DictionarySense[]): PosGroup[] {
  const groups: PosGroup[] = [];
  for (const pos of POS_ORDER) {
    const forPos = senses.filter((s) => s.pos === pos);
    if (forPos.length > 0) {
      groups.push({ pos, senses: forPos });
    }
  }
  // Preserve any unknown POS values, grouped in first-seen order.
  const known = new Set<string>(POS_ORDER);
  const seen = new Set<string>();
  for (const sense of senses) {
    if (known.has(sense.pos) || seen.has(sense.pos)) {
      continue;
    }
    seen.add(sense.pos);
    groups.push({ pos: sense.pos, senses: senses.filter((s) => s.pos === sense.pos) });
  }
  return groups;
}
